use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::platform::affinity::{proc_snapshot, read_cmdline, task_tids, tid_comm};
use crate::engine::cache::ProcCache;
use crate::config::AppConfig;
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

pub struct ProcScanState {
    pub cache: ProcCache,
    pub tracked_pids: HashMap<i32, String>,
    pub last_snapshot: HashSet<i32>,
    pub force_affinity: bool,
    last_reconcile: Instant,
}

impl ProcScanState {
    pub fn new() -> Self {
        Self {
            cache: ProcCache::new(),
            tracked_pids: HashMap::new(),
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
fn remove_dead(state: &mut ProcScanState, snapshot: &HashSet<i32>) -> bool {
    let removed: HashSet<i32> = state.last_snapshot.difference(snapshot).copied().collect();
    if removed.is_empty() {
        return false;
    }
    state.tracked_pids.retain(|pid, _| !removed.contains(pid));
    state.cache.remove_pkgs(&removed);
    true
}

fn collect_new(
    cfg: &AppConfig,
    state: &mut ProcScanState,
    snapshot: &HashSet<i32>,
    reconcile: bool,
) -> bool {
    let candidates: Vec<i32> = if reconcile {
        snapshot
            .iter()
            .filter(|p| !state.tracked_pids.contains_key(p))
            .copied()
            .collect()
    } else {
        snapshot.difference(&state.last_snapshot).copied().collect()
    };
    let mut changed = false;
    for pid in candidates {
        let Some(pkg) = read_cmdline(pid).or_else(|| tid_comm(pid)) else {
            continue;
        };
        if !cfg.pkgs.contains(&pkg) {
            continue;
        }
        state.tracked_pids.insert(pid, pkg.clone());
        if proc_tasks(pid, &pkg, cfg, &mut state.cache) {
            changed = true;
        }
    }
    changed
}

fn reconcile_tracked(cfg: &AppConfig, state: &mut ProcScanState) -> bool {
    let tracked: Vec<(i32, String)> = state
        .tracked_pids
        .iter()
        .map(|(pid, pkg)| (*pid, pkg.clone()))
        .collect();
    let mut changed = false;
    for (pid, pkg) in tracked {
        match read_cmdline(pid).or_else(|| tid_comm(pid)) {
            Some(current) if cfg.pkgs.contains(&current) => {
                if current != pkg {
                    state.tracked_pids.insert(pid, current.clone());
                }
                if proc_tasks(pid, &current, cfg, &mut state.cache) {
                    changed = true;
                }
            }
            _ => {
                state.tracked_pids.remove(&pid);
                state.cache.remove_pkgs(&HashSet::from([pid]));
                changed = true;
            }
        }
    }
    changed
}

fn refresh_tracked(cfg: &AppConfig, state: &mut ProcScanState) -> bool {
    let mut changed = false;
    for (pid, pkg) in &state.tracked_pids {
        if proc_tasks(*pid, pkg, cfg, &mut state.cache) {
            changed = true;
        }
    }
    changed
}

pub fn proc_diff(cfg: &AppConfig, state: &mut ProcScanState) {
    let Some(snapshot) = proc_snapshot() else {
        return;
    };
    let reconcile = state.last_reconcile.elapsed() >= RECONCILE_INTERVAL;
    if reconcile {
        state.last_reconcile = Instant::now();
    }

    let mut changed = remove_dead(state, &snapshot);
    changed |= collect_new(cfg, state, &snapshot, reconcile);
    changed |= if reconcile {
        reconcile_tracked(cfg, state)
    } else {
        refresh_tracked(cfg, state)
    };

    state.last_snapshot = snapshot;
    state.force_affinity |= changed;
}
fn proc_tasks(pid: i32, pkg: &str, cfg: &AppConfig, cache: &mut ProcCache) -> bool {
    let has_thread_rules =
        cfg.has_thread_rules.contains(pkg) || cfg.has_dyn_rules.contains(pkg);
    let Some(tids) = task_tids(pid) else { return false };
    let live: HashSet<i32> = tids.iter().copied().collect();
    let unchanged = cache
        .by_pid
        .get(&pid)
        .is_some_and(|o| o.len() == live.len() && o.iter().all(|t| live.contains(t)));
    if unchanged {
        return false;
    }
    for tid in &tids {
        let thread_name = if has_thread_rules {
            cache
                .tasks
                .get(tid)
                .map(|e| e.comm.clone())
                .unwrap_or_else(|| tid_comm(*tid).unwrap_or_default())
        } else {
            String::new()
        };
        cache.task_apply(*tid, pid, pkg, &thread_name, cfg, |_, _, _| false);
    }
    cache.retain_pkg_tids(pid, &live);
    true
}