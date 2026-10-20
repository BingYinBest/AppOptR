use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::apply_affinity::{proc_snapshot, read_cmdline, task_tids, tid_comm};
use crate::cache::ProcCache;
use crate::config::AppConfig;

/// 增量 diff 会漏掉「单轮 readdir 时读 cmdline 失败」的进程，周期性对账自愈
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

pub struct ProcScanState {
    pub cache: ProcCache,
    /// 目标进程 pid 集合，随快照 diff 增量维护
    pub tracked_pids: HashSet<i32>,
    /// 上次 readdir 快照的全部 pid，用于增量 diff
    pub last_snapshot: HashSet<i32>,
    /// 本轮有绑定变化时置位，下轮立即应用亲和性
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

    /// 配置变更或模式切换后强制全量重匹配：清空快照与跟踪集合，下轮全部按新增处理
    pub fn reset_scan(&mut self) {
        self.last_snapshot.clear();
        self.tracked_pids.clear();
        self.cache.clear();
        self.force_affinity = true;
    }
}

/// 快照 diff 增量同步：只处理新增/消失进程与已跟踪进程的线程变化，不做全量扫描
pub fn proc_diff(cfg: &AppConfig, state: &mut ProcScanState) {
    let Some(snapshot) = proc_snapshot() else {
        return;
    };

    let mut changed = false;

    // 消失进程：无需文件读取，直接清理跟踪与缓存条目
    let removed: HashSet<i32> = state.last_snapshot.difference(&snapshot).copied().collect();
    if !removed.is_empty() {
        state.tracked_pids.retain(|pid| !removed.contains(pid));
        state.cache.remove_pkgs(&removed);
        changed = true;
    }

    // 新增进程：读完整包名匹配目标后填充线程；对账轮放宽到所有未跟踪 pid，自愈漏检
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

    // 已跟踪进程：重读 cmdline 确认包名，pid 被非目标进程复用时可解除误管；再重扫 task 目录
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

/// 扫描 pid 的 task 目录增量更新缓存，返回 true 表示线程集合有变化
fn proc_tasks(pid: i32, pkg: &str, cfg: &AppConfig, cache: &mut ProcCache) -> bool {
    // 静态线程规则或动态规则都需要线程名，缺一即空字符串（主线程条目）
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
