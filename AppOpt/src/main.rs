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
    affinity_check, full_scan, event_dispatch, comm_map_init,
    ebpf_init, EbpfState,
};
use crate::proc_mode::{proc_diff, ProcScanState};
use crate::web::{
    cache_stats, settings_load, settings_save, web_start, WebStats,
    WEB_ENABLED, WEB_STATS, MODE_FORCE, SETTINGS_FILE,
};

pub const MAX_PKG_LEN: usize = 128;
pub const MAX_THREAD_LEN: usize = 32;

pub static CONFIG_UPDATED: AtomicBool = AtomicBool::new(false);

/// 自动模式下 eBPF 初始化失败后的放弃标记，用户强制切换时清除
pub static EBPF_GAVE_UP: AtomicBool = AtomicBool::new(false);

pub(crate) fn lock_ignore_poison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| {
        eprintln!("警告: 互斥锁中毒，尝试恢复...");
        e.into_inner()
    })
}

fn print_help(prog_name: &str) {
    println!("Usage: {} [OPTIONS]", prog_name);
    println!("Options:");
    println!("  -c <config_file>   指定配置文件 (默认: ./applist.conf)");
    println!("  -s <interval>      设置检查间隔(秒) (必须>=1, 默认: 2)");
    println!("  -b <cpuset_name>   指定 BASE_CPUSET 目录名 (默认: AppOpt)");
    println!("  -w                 启用网页前端 (仅本机 127.0.0.1:8889)");
    println!("  -v                 显示程序版本");
    println!("  -h                 显示帮助信息");
    println!();
    println!("示例:");
    println!("  {} -c /data/applist.conf -s 3", prog_name);
    println!("  {} -b MyAppOpt", prog_name);
    println!();
    println!("应用设置保存于 ./AppOpt.json，首次运行自动创建；");
    println!("命令行参数优先于设置文件，web 端修改会写回该文件。");
    println!();
    println!("规则格式:");
    println!("  # 注释以 # 或 // 开头");
    println!("  com.example=0-3                包级规则，绑定到 CPU 0-3");
    println!("  com.example:push=0-3           子进程规则，限定 :push 子进程");
    println!("  com.example{{RenderThread}}=0-3  线程规则，绑定指定线程（支持 * ? [] 通配）");
    println!();
    println!("  语义核心（按 cpufreq 频率分组推断），可逗号组合：");
    println!("    e-core     能效小核");
    println!("    m-core     中频档（4 档及以上芯片的第二档）");
    println!("    p-core     性能中核");
    println!("    hp-core    高性能大核");
    println!("    all-core   所有核心");
    println!("    示例: com.example=e-core,p-core");
    println!();
    println!("  块语法，包级规则 + 线程规则");
    println!("  com.example {{");
    println!("    RenderThread=6-7");
    println!("    Thread-1=0-5");
    println!("  }}");
    println!("  线程 RenderThread 绑定到 CPU 6-7");
    println!("  线程 Thread-1 绑定到 CPU 0-5");
    println!();
    println!("  动态规则（按负载自动选择）：");
    println!("    Thread-*=elect(6-7 | 0-5)    组内负载最高线程绑 6-7，其余绑 0-5");
    println!("    Thread-*=load(7 | 4-6 | 50)  单线程负载>50% 绑 7，否则绑 4-6");
    println!("  动态规则优先于同包静态线程规则，组内线程不参与静态合并；");
    println!("  elect 换人需超现任 20% 且驻留 5s，load 阈值带 5% 滞回，切换均需驻留 5s 防抖");
}

/// 主循环运行状态，收敛主循环内的全部局部状态
struct LoopState {
    config_handle: thread::JoinHandle<()>,
    proc_state: Option<ProcScanState>,
    ebpf_state: Option<EbpfState>,
    affinity_deadline: Instant,
    web_stats_deadline: Instant,
    ebpf_retry_at: Instant,
    /// 动态规则（按负载选举/阈值）周期评估
    dyn_deadline: Instant,
    /// eBPF 事件丢失兜底：周期全量对账（proc 模式有 60s reconcile，此处低频全扫）
    ebpf_reconcile_deadline: Instant,
    prog_start: Instant,
}

/// 配置加载线程 panic 恢复
fn revive_config_loader(state: &mut LoopState) {
    if state.config_handle.is_finished() {
        eprintln!("警告: 配置加载线程异常退出，尝试重启...");
        state.config_handle = thread::spawn(config_loader);
    }
}

/// 强制 /proc 模式时卸载 eBPF 并触发全量扫描
fn switch_to_proc_if_forced(state: &mut LoopState, mode: u8) {
    if mode == 2 && state.ebpf_state.is_some() {
        println!("工作模式切换: /proc 轮询");
        state.ebpf_state = None;
        let cache = state.proc_state.get_or_insert_with(ProcScanState::new);
        cache.reset_scan();
        state.affinity_deadline = Instant::now();
    }
}

/// eBPF 缺失时周期重试，自动模式失败一次后放弃，强制模式持续重试
fn try_ebpf_reinit(state: &mut LoopState, cfg: &AppConfig, mode: u8) {
    if mode != 2
        && state.ebpf_state.is_none()
        && (mode == 1 || !EBPF_GAVE_UP.load(Ordering::Relaxed))
        && state.ebpf_retry_at.elapsed() >= Duration::from_secs(30)
    {
        state.ebpf_retry_at = Instant::now();
        if let Some(mut new_es) = ebpf_init() {
            if comm_map_init(&mut new_es.bpf, &cfg.pkgs, new_es.comm_capacity) {
                eprintln!("eBPF: 白名单容量不足，保持 /proc 轮询");
                if mode == 0 {
                    EBPF_GAVE_UP.store(true, Ordering::Relaxed);
                }
            } else {
                println!("工作模式切换: eBPF 事件驱动");
                full_scan(cfg, &mut new_es);
                state.ebpf_state = Some(new_es);
                state.affinity_deadline = Instant::now();
            }
        } else if mode == 0 {
            EBPF_GAVE_UP.store(true, Ordering::Relaxed);
        }
    }
}

/// 配置变更时刷新 eBPF 白名单，返回 true 表示需重建程序
fn ebpf_reload_requested(state: &mut LoopState, cfg: &AppConfig, config_changed: bool) -> bool {
    if let Some(es) = state.ebpf_state.as_mut()
        && config_changed
    {
        // 容量不足时交由 rebuild_ebpf 重建（新程序会重新 full_scan），避免对将被丢弃的旧程序做无效扫描
        if comm_map_init(&mut es.bpf, &cfg.pkgs, es.comm_capacity) {
            true
        } else {
            full_scan(cfg, es);
            false
        }
    } else {
        false
    }
}

/// 白名单容量不足时重建 eBPF 程序，失败回退 /proc
fn rebuild_ebpf(state: &mut LoopState, cfg: &AppConfig) {
    state.ebpf_state = None;
    if let Some(mut new_es) = ebpf_init() {
        if comm_map_init(&mut new_es.bpf, &cfg.pkgs, new_es.comm_capacity) {
            eprintln!("eBPF: 重载后白名单容量仍不足，回退到 /proc 轮询");
            return;
        }
        full_scan(cfg, &mut new_es);
        state.ebpf_state = Some(new_es);
    }
}

/// eBPF 分支：事件派发与周期性 affinity_check，返回事件通道是否断开
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
    if dyn_deadline.elapsed() >= Duration::from_secs(5 * interval) {
        dyn_eval(cfg, &mut es.cache);
        *dyn_deadline = Instant::now();
    }
    dead
}

/// /proc 分支：增量同步与周期性 affinity_sync
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
    if dyn_deadline.elapsed() >= Duration::from_secs(5 * interval) {
        dyn_eval(cfg, &mut cache.cache);
        *dyn_deadline = Instant::now();
    }
    thread::sleep(Duration::from_secs(interval));
}

/// eBPF 事件通道断开后回退到 /proc 轮询
fn fallback_to_proc(state: &mut LoopState) {
    eprintln!("eBPF: 事件通道断开，回退到 /proc 轮询");
    state.ebpf_state = None;
    let cache = state.proc_state.get_or_insert_with(ProcScanState::new);
    cache.reset_scan();
    state.affinity_deadline = Instant::now();
}

/// web 状态统计，低频更新避免高频事件循环下的额外开销
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
    let prog_name = &args[0];

    // 参数解析先行，-v/-h/错误用法在设置加载前退出，不产生文件副作用
    let (mut cli_cfg, mut cli_interval, mut cli_cpuset, mut cli_web) =
        (None, None, None, false);

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-c" => {
                i += 1;
                if i < args.len() {
                    cli_cfg = Some(args[i].clone());
                    println!("配置文件: {}", args[i]);
                } else {
                    eprintln!("错误: -c 需要指定配置文件路径");
                    process::exit(1);
                }
            }
            "-s" => {
                i += 1;
                if i < args.len() {
                    let val: u64 = match args[i].parse() {
                        Ok(v) if v >= 1 => v,
                        _ => {
                            eprintln!("无效的时间间隔: {}", args[i]);
                            eprintln!("间隔必须是 >=1 的整数");
                            process::exit(1);
                        }
                    };
                    cli_interval = Some(val);
                    println!("检查间隔: {} 秒", val);
                } else {
                    eprintln!("错误: -s 需要指定时间间隔");
                    process::exit(1);
                }
            }
            "-w" => {
                cli_web = true;
            }
            "-b" => {
                i += 1;
                if i < args.len() {
                    cli_cpuset = Some(args[i].clone());
                    if args[i].is_empty() || args[i].contains('/') {
                        eprintln!("无效的 cpuset 目录名: {}", args[i]);
                        eprintln!("目录名不能为空或包含路径分隔符");
                        process::exit(1);
                    }
                    println!("cpuset 目录名: {}", args[i]);
                } else {
                    eprintln!("错误: -b 需要指定 cpuset 目录名");
                    process::exit(1);
                }
            }
            "-v" => {
                if crate::ebpf_mode::ebpf_probe() {
                    println!("AppOpt 版本 {} eBPF", env!("CARGO_PKG_VERSION"));
                } else {
                    println!("AppOpt 版本 {}", env!("CARGO_PKG_VERSION"));
                }
                process::exit(0);
            }
            "-h" => {
                print_help(prog_name);
                process::exit(0);
            }
            other => {
                eprintln!("未知选项: {}", other);
                print_help(prog_name);
                process::exit(1);
            }
        }
        i += 1;
    }

    // 应用设置持久化于 AppOpt.json，命令行参数优先覆盖
    let st = settings_load(SETTINGS_FILE);
    let config_file = cli_cfg.unwrap_or(st.config_file);
    let sleep_interval = cli_interval.unwrap_or(st.check_interval);
    let cpuset_name = cli_cpuset.unwrap_or_else(|| st.cpuset_name.clone());
    let web_enable = cli_web || st.web_enable;

    // 更换 cpuset 目录名时清除旧目录及其中的任务，避免残留
    if cpuset_name != st.cpuset_name {
        crate::cpuset::remove_cpuset_dir(&st.cpuset_name);
    }

    // 先设置 cpuset 路径再初始化拓扑，init_cpu_topo 会创建 BASE_CPUSET 目录
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
    MODE_FORCE.store(st.mode, Ordering::Release);

    let mut tmp_mtime: i64 = -1;
    let initial_config = match load_config(&config_file, &topo, &mut tmp_mtime) {
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
        // -w 或设置恢复启用后落盘，重启保持开启
        settings_save();
    }

    // 守护进程模式，保存 JoinHandle 用于 panic 恢复检测
    let config_handle = thread::spawn(config_loader);

    println!("启动AppOpt服务 v{}", env!("CARGO_PKG_VERSION"));

    // 恢复的强制 proc 模式无需 eBPF 初始化
    let ebpf_state: Option<EbpfState> =
        if MODE_FORCE.load(Ordering::Relaxed) == 2 { None } else { ebpf_init() };
    // 仅自动模式失败一次即放弃；强制 eBPF 需持续重试，强制 proc 无需 eBPF
    if ebpf_state.is_none() && MODE_FORCE.load(Ordering::Relaxed) == 0 {
        EBPF_GAVE_UP.store(true, Ordering::Relaxed);
    }

    let mut st = LoopState {
        config_handle,
        proc_state: None,
        ebpf_state,
        affinity_deadline: Instant::now(),
        web_stats_deadline: Instant::now(),
        // 预支 60 秒使首次重试立即到期
        ebpf_retry_at: Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(Instant::now),
        dyn_deadline: Instant::now(),
        ebpf_reconcile_deadline: Instant::now(),
        prog_start: Instant::now(),
    };

    loop {
        // 先 swap CONFIG_UPDATED 再获取 cfg 防止漏更新
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
