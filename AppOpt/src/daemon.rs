//!

use std::env;
use std::fs;
use std::process;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::parse_args;
use crate::config::{
    config_loader, init_inotify, load_config, AppConfig, CHECK_INTERVAL, CONFIG_FILE, CURRENT_CONFIG,
};
use crate::platform::cpuset::{init_cpu_topo, set_base_cpuset};
use crate::engine::dynamic::{dyn_eval, DYN_PROBE};
use crate::engine::hot::{hot_eval, HOT_PROBE};
use crate::engine::ebpf::{
    affinity_check, comm_map_init, dropped_take, event_dispatch, full_scan,
    ebpf_init, CommMapUpdate, EbpfState,
};
use crate::engine::proc::{proc_diff, ProcScanState};
use crate::web::{
    cache_stats, settings_load, settings_save, web_start, WebStats, MODE_FORCE, SETTINGS_FILE,
    WEB_ENABLED, WEB_STATS,
};
use crate::{lock_ignore_poison, CONFIG_UPDATED, EBPF_GAVE_UP};

struct LoopState {
    config_handle: thread::JoinHandle<()>,
    proc_state: Option<ProcScanState>,
    ebpf_state: Option<EbpfState>,
    affinity_deadline: Instant,
    web_stats_deadline: Instant,
    ebpf_retry_at: Instant,
    dyn_deadline: Instant,
    hot_deadline: Instant,
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

fn ebpf_reload_requested(state: &mut LoopState, cfg: &AppConfig, config_changed: bool) -> bool {
    let Some(es) = state.ebpf_state.as_mut() else {
        return false;
    };
    if !config_changed {
        return false;
    }
    match comm_map_init(&mut es.bpf, &cfg.pkgs, es.comm_capacity) {
        CommMapUpdate::Updated => {
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
    hot_deadline: &mut Instant,
    cfg: &AppConfig,
    interval: u64,
) -> bool {
    let mut dead = false;
    match es.event_rx.recv_timeout(DYN_PROBE) {
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
    if dyn_deadline.elapsed() >= DYN_PROBE {
        dyn_eval(cfg, &mut es.cache);
        if hot_deadline.elapsed() >= HOT_PROBE {
            hot_eval(cfg, &mut es.cache);
            *hot_deadline = Instant::now();
        }
        *dyn_deadline = Instant::now();
    }
    dead
}

fn proc_cycle(
    cache: &mut ProcScanState,
    affinity_deadline: &mut Instant,
    dyn_deadline: &mut Instant,
    hot_deadline: &mut Instant,
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
    if dyn_deadline.elapsed() >= DYN_PROBE {
        dyn_eval(cfg, &mut cache.cache);
        if hot_deadline.elapsed() >= HOT_PROBE {
            hot_eval(cfg, &mut cache.cache);
            *hot_deadline = Instant::now();
        }
        *dyn_deadline = Instant::now();
    }
    // Sleep in DYN_PROBE slices so a short burst is promoted before the next
    // full /proc scan. The scan itself still runs once per `interval`.
    let mut left = Duration::from_secs(interval);
    while !left.is_zero() {
        let step = DYN_PROBE.min(left);
        thread::sleep(step);
        left -= step;
        if dyn_deadline.elapsed() >= DYN_PROBE {
            dyn_eval(cfg, &mut cache.cache);
            if hot_deadline.elapsed() >= HOT_PROBE {
                hot_eval(cfg, &mut cache.cache);
                *hot_deadline = Instant::now();
            }
            *dyn_deadline = Instant::now();
        }
    }
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

pub fn run() {
    let args: Vec<String> = env::args().collect();
    let cli = parse_args(&args);
    let settings = settings_load(SETTINGS_FILE);

    let config_file = cli.config_file.unwrap_or(settings.config_file);
    let sleep_interval = cli.interval.unwrap_or(settings.check_interval);
    let cpuset_name = cli.cpuset_name.unwrap_or(settings.cpuset_name.clone());
    let web_enable = cli.web || settings.web_enable;
    let web_port = cli.web_port.unwrap_or(settings.web_port);
    if cpuset_name != settings.cpuset_name {
        crate::platform::cpuset::remove_cpuset_dir(&settings.cpuset_name);
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
        web_start(web_port);
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
        ebpf_retry_at: Instant::now() - Duration::from_secs(60),
        dyn_deadline: Instant::now(),
        hot_deadline: Instant::now(),
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
            ebpf_event_step(es, &mut st.affinity_deadline, &mut st.dyn_deadline, &mut st.hot_deadline, &cfg, interval)
        } else {
            let cache = st.proc_state.get_or_insert_with(ProcScanState::new);
            proc_cycle(cache, &mut st.affinity_deadline, &mut st.dyn_deadline, &mut st.hot_deadline, &cfg, interval, config_changed);
            false
        };

        if ebpf_dead {
            fallback_to_proc(&mut st);
        }

        update_web_stats(&mut st, &cfg);
    }
}
