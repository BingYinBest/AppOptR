#![cfg(any(target_os = "linux", target_os = "android"))]
#[cfg(target_pointer_width = "32")]
compile_error!("AppOpt requires 64-bit target due to cpu_set_t binary layout assumptions");

mod apply_affinity;
mod cache;
mod config;
mod cpuset;
mod dynamic;
mod ebpf_mode;
mod proc_mode;
mod rule_edit;
mod rule_match;
mod web;

use std::env;
use std::fs;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{
    config_loader, init_inotify, load_config, AppConfig, CHECK_INTERVAL, CONFIG_FILE, CURRENT_CONFIG,
};
use crate::cpuset::{init_cpu_topo, set_base_cpuset};
use crate::dynamic::dyn_eval;
use crate::ebpf_mode::{
    affinity_check, comm_map_init, dropped_take, event_dispatch, full_scan, ebpf_init,
    CommMapUpdate, EbpfState,
};
use crate::proc_mode::{proc_diff, ProcScanState};
use crate::web::{
    cache_stats, settings_load, settings_save, web_start, WebStats,
    WEB_ENABLED, WEB_STATS, MODE_FORCE, SETTINGS_FILE,
};

pub const MAX_PKG_LEN: usize = 128;
pub const MAX_THREAD_LEN: usize = 32;

pub static CONFIG_UPDATED: AtomicBool = AtomicBool::new(false);
pub static EBPF_GAVE_UP: AtomicBool = AtomicBool::new(false);

pub(crate) fn lock_ignore_poison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| {
        eprintln!("警告: 互斥锁中毒，尝试恢复...");
        e.into_inner()
    })
}

fn print_help() {
    println!("Usage: AppOpt [OPTIONS]");
    println!("Options:");
    println!("  -c <config_file>   指定配置文件 (默认: ./applist.conf)");
    println!("  -s <interval>      设置检查间隔(秒) (必须>=1, 默认: 2)");
    println!("  -b <cpuset_name>   指定 BASE_CPUSET 目录名 (默认: AppOpt)");
    println!("  -w                 启用网页前端 (仅本机 127.0.0.1:8889)");
    println!("  -v                 显示程序版本");
    println!("  -h                 显示帮助信息");
}

/// 命令行参数
struct CliOptions {
    config_file: Option<String>,
    interval: Option<u64>,
    cpuset_name: Option<String>,
    web: bool,
}

/// 读取选项的后置参数，缺省时直接退出
fn arg_value(args: &[String], index: &mut usize, flag: &str) -> String {
    *index += 1;
    match args.get(*index) {
        Some(value) => value.clone(),
        None => {
            eprintln!("错误: {} 需要指定参数值", flag);
            process::exit(1);
        }
    }
}

fn parse_args(args: &[String]) -> CliOptions {
    let mut opts = CliOptions {
        config_file: None,
        interval: None,
        cpuset_name: None,
        web: false,
    };
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-c" => {
                let path = arg_value(args, &mut i, "-c");
                println!("配置文件: {}", path);
                opts.config_file = Some(path);
            }
            "-s" => {
                let raw = arg_value(args, &mut i, "-s");
                match raw.parse::<u64>() {
                    Ok(value) if value >= 1 => {
                        println!("检查间隔: {} 秒", value);
                        opts.interval = Some(value);
                    }
                    _ => {
                        eprintln!("无效的时间间隔: {}", raw);
                        eprintln!("间隔必须是 >=1 的整数");
                        process::exit(1);
                    }
                }
            }
            "-w" => opts.web = true,
            "-b" => {
                let name = arg_value(args, &mut i, "-b");
                if name.is_empty() || name.contains('/') {
                    eprintln!("无效的 cpuset 目录名: {}", name);
                    eprintln!("目录名不能为空或包含路径分隔符");
                    process::exit(1);
                }
                println!("cpuset 目录名: {}", name);
                opts.cpuset_name = Some(name);
            }
            "-v" => {
                let ebpf = crate::ebpf_mode::ebpf_probe();
                println!(
                    "AppOpt 版本 {}{}",
                    env!("CARGO_PKG_VERSION"),
                    if ebpf { " eBPF" } else { "" }
                );
                process::exit(0);
            }
            "-h" => {
                print_help();
                process::exit(0);
            }
            other => {
                eprintln!("未知选项: {}", other);
                print_help();
                process::exit(1);
            }
        }
        i += 1;
    }
    opts
}
struct LoopState {
    config_handle: thread::JoinHandle<()>,
    proc_state: Option<ProcScanState>,
    ebpf_state: Option<EbpfState>,
    affinity_deadline: Instant,
    web_stats_deadline: Instant,
    ebpf_retry_at: Instant,
    dyn_deadline: Instant,
    ebpf_reconcile_deadline: Instant,
    prog_start: Instant,
}
fn revive_config_loader(state: &mut LoopState) {
    if state.config_handle.is_finished() {
        eprintln!("警告: 配置加载线程异常退出，尝试重启...");
        state.config_handle = thread::spawn(config_loader);
    }
}
fn enter_proc_mode(state: &mut LoopState) {
    state.ebpf_state = None;
    let cache = state.proc_state.get_or_insert_with(ProcScanState::new);
    cache.reset_scan();
    state.affinity_deadline = Instant::now();
}
fn switch_to_proc_if_forced(state: &mut LoopState, mode: u8) {
    if mode == 2 && state.ebpf_state.is_some() {
        println!("工作模式切换: /proc 轮询");
        enter_proc_mode(state);
    }
}
fn ebpf_start(state: &mut LoopState, cfg: &AppConfig) -> bool {
    let Some(mut new_es) = ebpf_init() else { return false };
    if comm_map_init(&mut new_es.bpf, &cfg.pkgs, new_es.comm_capacity) == CommMapUpdate::ReloadRequired
    {
        eprintln!("eBPF: 白名单配置失败，回退到 /proc 轮询");
        return false;
    }
    full_scan(cfg, &mut new_es);
    state.ebpf_state = Some(new_es);
    state.affinity_deadline = Instant::now();
    true
}
fn try_ebpf_reinit(state: &mut LoopState, cfg: &AppConfig, mode: u8) {
    if mode != 2
        && state.ebpf_state.is_none()
        && (mode == 1 || !EBPF_GAVE_UP.load(Ordering::Relaxed))
        && state.ebpf_retry_at.elapsed() >= Duration::from_secs(30)
    {
        state.ebpf_retry_at = Instant::now();
        if ebpf_start(state, cfg) {
            println!("工作模式切换: eBPF 事件驱动");
        } else if mode == 0 {
            EBPF_GAVE_UP.store(true, Ordering::Relaxed);
        }
    }
}
/// 配置变更后同步内核白名单；需要重载 eBPF 时返回 true
fn ebpf_reload_requested(state: &mut LoopState, cfg: &AppConfig, config_changed: bool) -> bool {
    let Some(es) = state.ebpf_state.as_mut() else {
        return false;
    };
    if !config_changed {
        return false;
    }
    match comm_map_init(&mut es.bpf, &cfg.pkgs, es.comm_capacity) {
        CommMapUpdate::Updated => {
            // 白名单可能新增了包名，重扫一次让存量进程立即应用规则
            full_scan(cfg, es);
            false
        }
        CommMapUpdate::ReloadRequired => true,
    }
}
fn rebuild_ebpf(state: &mut LoopState, cfg: &AppConfig) {
    state.ebpf_state = None;
    ebpf_start(state, cfg);
}
fn ebpf_event_step(
    es: &mut EbpfState,
    affinity_deadline: &mut Instant,
    dyn_deadline: &mut Instant,
    cfg: &AppConfig,
    interval: u64,
) -> bool {
    let mut dead = false;
    match es.event_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(batch) => {
            for event in &batch {
                event_dispatch(event, cfg, es);
            }
            while let Ok(batch) = es.event_rx.try_recv() {
                for event in &batch {
                    event_dispatch(event, cfg, es);
                }
            }
            let dropped = dropped_take(&mut es.bpf);
            if dropped > 0 {
                eprintln!("eBPF: 事件溢出丢弃 {} 条（RingBuf 满载，请关注事件风暴）", dropped);
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            dead = true;
        }
    }

    if affinity_deadline.elapsed() >= Duration::from_secs(3 * interval) {
        affinity_check(es, cfg);
        *affinity_deadline = Instant::now();
    }
    if dyn_deadline.elapsed() >= Duration::from_secs(interval) {
        dyn_eval(cfg, &mut es.cache);
        *dyn_deadline = Instant::now();
    }
    dead
}
fn proc_cycle(
    cache: &mut ProcScanState,
    affinity_deadline: &mut Instant,
    dyn_deadline: &mut Instant,
    cfg: &AppConfig,
    interval: u64,
    config_changed: bool,
) {
    if config_changed {
        cache.reset_scan();
    }
    proc_diff(cfg, cache);
    if affinity_deadline.elapsed() >= Duration::from_secs(5 * interval) || cache.force_affinity {
        cache.cache.affinity_sync(&cfg.topo);
        *affinity_deadline = Instant::now();
        cache.force_affinity = false;
    }
    if dyn_deadline.elapsed() >= Duration::from_secs(interval) {
        dyn_eval(cfg, &mut cache.cache);
        *dyn_deadline = Instant::now();
    }
    thread::sleep(Duration::from_secs(interval));
}
fn fallback_to_proc(state: &mut LoopState) {
    eprintln!("eBPF: 事件通道断开，回退到 /proc 轮询");
    enter_proc_mode(state);
}
fn update_web_stats(state: &mut LoopState, cfg: &AppConfig) {
    if WEB_ENABLED.load(Ordering::Relaxed) && state.web_stats_deadline.elapsed() >= Duration::from_secs(2) {
        state.web_stats_deadline = Instant::now();
        let (threads, hit_pkgs) = match (&state.ebpf_state, &state.proc_state) {
            (Some(es), _) => cache_stats(&es.cache),
            (None, Some(ps)) => cache_stats(&ps.cache),
            _ => (0, 0),
        };
        *lock_ignore_poison(&WEB_STATS) = Some(WebStats {
            rules: cfg.rules.len() + cfg.dyn_rules.len(),
            pkgs: cfg.pkgs.len(),
            hit_pkgs,
            threads,
            ebpf: state.ebpf_state.is_some(),
            uptime: state.prog_start.elapsed().as_secs(),
        });
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let cli = parse_args(&args);
    let settings = settings_load(SETTINGS_FILE);

    let config_file = cli.config_file.unwrap_or(settings.config_file);
    let sleep_interval = cli.interval.unwrap_or(settings.check_interval);
    let cpuset_name = cli.cpuset_name.unwrap_or(settings.cpuset_name.clone());
    let web_enable = cli.web || settings.web_enable;
    if cpuset_name != settings.cpuset_name {
        crate::cpuset::remove_cpuset_dir(&settings.cpuset_name);
    }
    set_base_cpuset(&cpuset_name);
    let topo = init_cpu_topo();

    if fs::metadata(&config_file).is_err() {
        let initial_content = "# 规则编写与使用说明请参考 http://AppOpt.suto.top\n\n";
        if fs::write(&config_file, initial_content).is_ok() {
            println!("配置文件不存在，重建一个空的配置文件: {}", config_file);
        }
    }

    {
        let mut guard = lock_ignore_poison(&CONFIG_FILE);
        *guard = config_file.clone();
    }
    CHECK_INTERVAL.store(sleep_interval, Ordering::Release);
    MODE_FORCE.store(settings.mode, Ordering::Release);

    let initial_config = match load_config(&config_file, &topo) {
        Some(cfg) => cfg,
        None => {
            eprintln!("初始配置加载失败");
            process::exit(1);
        }
    };

    {
        let mut guard = lock_ignore_poison(&CURRENT_CONFIG);
        *guard = Some(Arc::new(initial_config));
    }
    CONFIG_UPDATED.store(true, Ordering::Release);

    init_inotify(&config_file);

    if web_enable {
        web_start();
        settings_save();
    }
    let config_handle = thread::spawn(config_loader);

    println!("启动AppOpt服务 v{}", env!("CARGO_PKG_VERSION"));
    let ebpf_state: Option<EbpfState> =
        if MODE_FORCE.load(Ordering::Relaxed) == 2 { None } else { ebpf_init() };
    if ebpf_state.is_none() && MODE_FORCE.load(Ordering::Relaxed) == 0 {
        EBPF_GAVE_UP.store(true, Ordering::Relaxed);
    }

    let mut st = LoopState {
        config_handle,
        proc_state: None,
        ebpf_state,
        affinity_deadline: Instant::now(),
        web_stats_deadline: Instant::now(),
        // 立即满足首次 eBPF 重试条件
        ebpf_retry_at: Instant::now() - Duration::from_secs(60),
        dyn_deadline: Instant::now(),
        ebpf_reconcile_deadline: Instant::now(),
        prog_start: Instant::now(),
    };

    loop {
        let config_changed = CONFIG_UPDATED.swap(false, Ordering::AcqRel);
        let Some(cfg) = lock_ignore_poison(&CURRENT_CONFIG).clone() else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };
        let interval = CHECK_INTERVAL.load(Ordering::Relaxed).max(1);
        let mode = MODE_FORCE.load(Ordering::Relaxed);

        revive_config_loader(&mut st);
        switch_to_proc_if_forced(&mut st, mode);
        try_ebpf_reinit(&mut st, &cfg, mode);

        let need_reload = ebpf_reload_requested(&mut st, &cfg, config_changed);
        if need_reload {
            rebuild_ebpf(&mut st, &cfg);
            continue;
        }

        let ebpf_dead = if let Some(es) = st.ebpf_state.as_mut() {
            if st.ebpf_reconcile_deadline.elapsed() >= Duration::from_secs(600) {
                st.ebpf_reconcile_deadline = Instant::now();
                full_scan(&cfg, es);
            }
            ebpf_event_step(es, &mut st.affinity_deadline, &mut st.dyn_deadline, &cfg, interval)
        } else {
            let cache = st.proc_state.get_or_insert_with(ProcScanState::new);
            proc_cycle(cache, &mut st.affinity_deadline, &mut st.dyn_deadline, &cfg, interval, config_changed);
            false
        };

        if ebpf_dead {
            fallback_to_proc(&mut st);
        }

        update_web_stats(&mut st, &cfg);
    }
}
