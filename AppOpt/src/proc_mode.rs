use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::apply_affinity::{proc_snapshot, read_cmdline, task_tids, tid_comm};
use crate::cache::ProcCache;
use crate::config::AppConfig;
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

pub struct ProcScanState {
    pub cache: ProcCache,
    pub tracked_pids: HashSet<i32>,
    pub last_snapshot: HashSet<i32>,
    pub force_affinity: bool,
    last_reconcile: Instant,
}

impl ProcScanState {
    pub fn new() -> Self {
        Self {
            cache: ProcCache::new(),
            tracked_pids: HashSet::new(),
            last_snapshot: HashSet::new(),
            force_affinity: false,
            last_reconcile: Instant::now(),
        }
    }
    pub fn reset_scan(&mut self) {
        self.last_snapshot.clear();
        self.tracked_pids.clear();
        self.cache.clear();
        self.force_affinity = true;
    }
}
pub fn proc_diff(cfg: &AppConfig, state: &mut ProcScanState) {
    let Some(snapshot) = proc_snapshot() else {
        return;
    };

    let mut changed = false;
    let removed: HashSet<i32> = state.last_snapshot.difference(&snapshot).copied().collect();
    if !removed.is_empty() {
        state.tracked_pids.retain(|pid| !removed.contains(pid));
        state.cache.remove_pkgs(&removed);
        changed = true;
    }
    let reconcile = state.last_reconcile.elapsed() >= RECONCILE_INTERVAL;
    let candidates: Vec<i32> = if reconcile {
        snapshot.difference(&state.tracked_pids).copied().collect()
    } else {
        snapshot.difference(&state.last_snapshot).copied().collect()
    };
    for pid in candidates {
        let Some(pkg) = read_cmdline(pid).or_else(|| tid_comm(pid)) else {
            continue;
        };
        if !cfg.pkgs.contains(&pkg) {
            continue;
        }
        state.tracked_pids.insert(pid);
        if proc_tasks(pid, &pkg, cfg, &mut state.cache) {
            changed = true;
        }
    }
    if reconcile {
        state.last_reconcile = Instant::now();
    }
    let tracked: Vec<i32> = state.tracked_pids.iter().copied().collect();
    for pid in tracked {
        let Some(pkg) = read_cmdline(pid).or_else(|| tid_comm(pid)) else {
            continue;
        };
        if !cfg.pkgs.contains(&pkg) {
            state.tracked_pids.remove(&pid);
            state.cache.remove_pkgs(&HashSet::from([pid]));
            changed = true;
            continue;
        }
        if proc_tasks(pid, &pkg, cfg, &mut state.cache) {
            changed = true;
        }
    }

    state.last_snapshot = snapshot;
    state.force_affinity |= changed;
}
fn proc_tasks(pid: i32, pkg: &str, cfg: &AppConfig, cache: &mut ProcCache) -> bool {
    let has_thread_rules =
        cfg.has_thread_rules.contains(pkg) || cfg.has_dyn_rules.contains(pkg);
    let Some(tids) = task_tids(pid) else { return false };
    let live: HashSet<i32> = tids.iter().copied().collect();
    let old: HashSet<i32> = cache
        .tasks
        .iter()
        .filter(|(_, e)| e.pid == pid)
        .map(|(tid, _)| *tid)
        .collect();
    if old == live {
        return false;
    }
    for tid in &tids {
        let thread_name = if has_thread_rules {
            tid_comm(*tid).unwrap_or_default()
        } else {
            String::new()
        };
        cache.task_apply(*tid, pid, pkg, &thread_name, cfg, |_, _, _| false);
    }
    cache.retain_pkg_tids(pid, &live);
    true
}
