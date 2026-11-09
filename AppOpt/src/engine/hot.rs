//! Built-in hot-thread governor.
//!
//! Independent of the rule engine: any thread of a tracked package that
//! saturates a core for about a second is pinned to the `hp-core` tier and
//! released once it has been calm for a while. The governor only acts when the
//! topology exposes a distinct high-performance tier, so devices without a
//! big-core cluster pay nothing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::AppConfig;
use crate::engine::cache::ProcCache;
use crate::engine::dynamic::{ewma, load_pct, thread_cpu_jiffies};
use crate::platform::affinity::affinity_set;
use crate::rules::matcher::cpuset_dir_cached;
use crate::trace;

pub const HOT_PROBE: Duration = Duration::from_millis(1000);
pub const HOT_PROMOTE_PCT: u32 = 60;
pub const HOT_RELEASE_PCT: u32 = 22;
pub const HOT_MIN_HOLD: Duration = Duration::from_secs(2);
pub const HOT_MAX_THREADS: usize = 6;

const FAST_WEIGHT_PCT: u32 = 55;
const SLOW_WEIGHT_PCT: u32 = 12;
const BURST_DELTA_PCT: u32 = 18;
const BURST_MIN_PCT: u32 = 75;
const PRUNE_EVERY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotAction {
    Promote,
    Release,
    Hold,
}

pub struct HotEntry {
    pub fast: u32,
    pub slow: u32,
    jiffies: u64,
    sampled_at: Instant,
    sampled: bool,
    promoted_at: Option<Instant>,
}

impl HotEntry {
    fn new(now: Instant) -> Self {
        Self {
            fast: 0,
            slow: 0,
            jiffies: 0,
            sampled_at: now,
            sampled: false,
            promoted_at: None,
        }
    }
}

pub struct HotState {
    pub tids: HashMap<i32, HotEntry>,
    last_prune: Option<Instant>,
}

impl HotState {
    pub fn new() -> Self {
        Self {
            tids: HashMap::new(),
            last_prune: None,
        }
    }

    pub fn clear(&mut self) {
        self.tids.clear();
        self.last_prune = None;
    }

    pub fn promoted_count(&self) -> usize {
        self.tids.values().filter(|e| e.promoted_at.is_some()).count()
    }
}

impl Default for HotState {
    fn default() -> Self {
        Self::new()
    }
}

/// `slots` is false when the promotion budget is already spent.
pub fn hot_decision(
    fast: u32,
    slow: u32,
    promoted: bool,
    held: Duration,
    slots: bool,
) -> HotAction {
    if promoted {
        return if held >= HOT_MIN_HOLD && fast <= HOT_RELEASE_PCT {
            HotAction::Release
        } else {
            HotAction::Hold
        };
    }
    let delta = fast.saturating_sub(slow);
    if slots && fast >= HOT_PROMOTE_PCT && (fast >= BURST_MIN_PCT || delta >= BURST_DELTA_PCT) {
        HotAction::Promote
    } else {
        HotAction::Hold
    }
}

fn sample(
    states: &mut HashMap<i32, HotEntry>,
    tid: i32,
    pid: i32,
    now: Instant,
    clk_tck: u64,
) -> (u32, u32, bool) {
    let Some(jiffies) = thread_cpu_jiffies(pid, tid) else {
        return states
            .get(&tid)
            .map(|e| (e.fast, e.slow, e.promoted_at.is_some()))
            .unwrap_or((0, 0, false));
    };
    let st = states.entry(tid).or_insert_with(|| HotEntry::new(now));
    let elapsed_ms = now.duration_since(st.sampled_at).as_millis() as u64;
    if elapsed_ms >= HOT_PROBE.as_millis() as u64 {
        let instant = load_pct(jiffies.saturating_sub(st.jiffies), elapsed_ms, clk_tck);
        let stale = elapsed_ms > (HOT_PROBE.as_millis() as u64) * 3;
        if st.sampled && !stale {
            st.fast = ewma(st.fast, instant, FAST_WEIGHT_PCT);
            st.slow = ewma(st.slow, instant, SLOW_WEIGHT_PCT);
        } else {
            st.fast = instant;
            st.slow = instant;
        }
        st.sampled = true;
        st.jiffies = jiffies;
        st.sampled_at = now;
    }
    (st.fast, st.slow, st.promoted_at.is_some())
}

fn distinct_high_tier(topo: &crate::platform::cpuset::CpuTopology) -> bool {
    topo.hp_core.count() > 0 && topo.hp_core != topo.present_cpus
}

pub fn hot_eval(cfg: &AppConfig, cache: &mut ProcCache) -> usize {
    let hp = cfg.topo.hp_core;
    if !distinct_high_tier(&cfg.topo) {
        if !cache.hot.tids.is_empty() {
            cache.hot.clear();
        }
        return 0;
    }
    let now = Instant::now();
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let prune_due = cache
        .hot
        .last_prune
        .map(|t| t.elapsed() >= PRUNE_EVERY)
        .unwrap_or(true);
    if prune_due {
        cache.hot.tids.retain(|tid, _| cache.tasks.contains_key(tid));
        cache.hot.last_prune = Some(now);
    }

    let mut actions: Vec<(i32, i32, HotAction)> = Vec::new();
    // Count promotions already queued in this round so several candidates
    // cannot overshoot HOT_MAX_THREADS together.
    let mut planned = cache.hot.promoted_count();
    for (&tid, entry) in &cache.tasks {
        let (fast, slow, promoted) = sample(&mut cache.hot.tids, tid, entry.pid, now, clk_tck);
        let held = cache
            .hot
            .tids
            .get(&tid)
            .and_then(|e| e.promoted_at)
            .map(|t| now.duration_since(t))
            .unwrap_or_default();
        let slots = planned < HOT_MAX_THREADS;
        let action = hot_decision(fast, slow, promoted, held, slots);
        if action == HotAction::Promote {
            planned += 1;
        }
        if action != HotAction::Hold || fast >= HOT_PROMOTE_PCT {
            let label = match action {
                HotAction::Promote => "promote",
                HotAction::Release => "release",
                HotAction::Hold => "hold",
            };
            trace::log("hot", tid, fast, slow, label);
        }
        if action != HotAction::Hold {
            actions.push((tid, entry.pid, action));
        }
    }

    let dir = cpuset_dir_cached(&hp, cfg);
    let mut changed = 0;
    for (tid, pid, action) in actions {
        match action {
            HotAction::Promote => {
                let Some(entry) = cache.tasks.get(&tid) else { continue };
                if entry.cpus == hp {
                    if let Some(st) = cache.hot.tids.get_mut(&tid) {
                        st.promoted_at = Some(now);
                    }
                    continue;
                }
                affinity_set(tid, &hp, &dir, &cfg.topo);
                if let Some(entry) = cache.tasks.get_mut(&tid) {
                    entry.cpus = hp;
                    entry.cpuset_dir = dir.clone();
                }
                if let Some(st) = cache.hot.tids.get_mut(&tid) {
                    st.promoted_at = Some(now);
                }
                changed += 1;
            }
            HotAction::Release => {
                let Some(entry) = cache.tasks.get(&tid) else { continue };
                let (pkg, comm) = (entry.pkg.clone(), entry.comm.clone());
                if let Some(st) = cache.hot.tids.get_mut(&tid) {
                    st.promoted_at = None;
                }
                let restored = cache.task_apply(tid, pid, &pkg, &comm, cfg, |tid, cpus, dir| {
                    affinity_set(tid, cpus, dir, &cfg.topo)
                });
                if !restored && cache.tasks.contains_key(&tid) {
                    // No rule resolves this thread any more: fall back to the
                    // full cpu set instead of leaving it on the hot tier.
                    let all = cfg.topo.present_cpus;
                    let all_dir = cpuset_dir_cached(&all, cfg);
                    affinity_set(tid, &all, &all_dir, &cfg.topo);
                    if let Some(entry) = cache.tasks.get_mut(&tid) {
                        entry.cpus = all;
                        entry.cpuset_dir = all_dir;
                    }
                }
                changed += 1;
            }
            HotAction::Hold => {}
        }
    }

    // A dynamic rule may retier a promoted thread; keep it on the hot tier
    // until the governor itself releases it.
    let promoted: Vec<i32> = cache
        .hot
        .tids
        .iter()
        .filter(|(_, e)| e.promoted_at.is_some())
        .map(|(&tid, _)| tid)
        .collect();
    for tid in promoted {
        let Some(entry) = cache.tasks.get(&tid) else { continue };
        if entry.cpus != hp {
            affinity_set(tid, &hp, &dir, &cfg.topo);
            if let Some(entry) = cache.tasks.get_mut(&tid) {
                entry.cpus = hp;
                entry.cpuset_dir = dir.clone();
            }
            changed += 1;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use crate::config::AppConfig;
    use crate::engine::cache::TaskEntry;
    use crate::platform::cpuset::{CpuSet, CpuTopology};

    fn cpus_of(list: &[usize]) -> CpuSet {
        let mut s = CpuSet::new();
        for &c in list {
            s.set(c);
        }
        s
    }

    fn cfg_hp() -> AppConfig {
        AppConfig {
            rules: Vec::new(),
            pkgs: HashSet::new(),
            has_thread_rules: HashSet::new(),
            topo: CpuTopology {
                present_cpus: cpus_of(&[0, 1]),
                present_str: "0-1".to_string(),
                mems_str: "0".to_string(),
                cpuset_enabled: false,
                e_core: cpus_of(&[0]),
                m_core: CpuSet::new(),
                p_core: CpuSet::new(),
                hp_core: cpus_of(&[1]),
            },
            rules_by_pkg: HashMap::new(),
            exact_thread_rules: HashMap::new(),
            dyn_rules: Vec::new(),
            dyn_by_pkg: HashMap::new(),
            has_dyn_rules: HashSet::new(),
            cpuset_dirs: Mutex::new(HashMap::new()),
            comm_prefixes: HashSet::new(),
            comm_suffixes: HashSet::new(),
        }
    }

    fn task(pid: i32) -> TaskEntry {
        TaskEntry {
            pid,
            pkg: "com.h".to_string(),
            comm: "Thread-0".to_string(),
            cpus: cpus_of(&[0]),
            cpuset_dir: String::new(),
            is_thread_rule: false,
        }
    }

    #[test]
    fn promotes_heavy_thread_to_high_tier() {
        let cfg = cfg_hp();
        let mut cache = ProcCache::new();
        cache.tasks.insert(42, task(999_999));
        cache.hot.tids.insert(
            42,
            HotEntry {
                fast: 85,
                slow: 20,
                jiffies: 0,
                sampled_at: Instant::now() - Duration::from_secs(2),
                sampled: true,
                promoted_at: None,
            },
        );
        let changed = hot_eval(&cfg, &mut cache);
        assert_eq!(changed, 1);
        assert_eq!(cache.tasks.get(&42).unwrap().cpus, cpus_of(&[1]));
        assert_eq!(cache.hot.promoted_count(), 1);
    }

    #[test]
    fn budget_holds_across_one_round() {
        let cfg = cfg_hp();
        let mut cache = ProcCache::new();
        for i in 0..10 {
            let tid = 100 + i;
            cache.tasks.insert(tid, task(900_000 + i));
            cache.hot.tids.insert(
                tid,
                HotEntry {
                    fast: 90,
                    slow: 20,
                    jiffies: 0,
                    sampled_at: Instant::now() - Duration::from_secs(2),
                    sampled: true,
                    promoted_at: None,
                },
            );
        }
        let changed = hot_eval(&cfg, &mut cache);
        assert_eq!(changed, HOT_MAX_THREADS);
        assert_eq!(cache.hot.promoted_count(), HOT_MAX_THREADS);
    }

    #[test]
    fn releases_calm_thread() {
        let cfg = cfg_hp();
        let mut cache = ProcCache::new();
        cache.tasks.insert(43, task(999_999));
        cache.tasks.get_mut(&43).unwrap().cpus = cpus_of(&[1]);
        cache.hot.tids.insert(
            43,
            HotEntry {
                fast: 5,
                slow: 5,
                jiffies: 0,
                sampled_at: Instant::now() - Duration::from_secs(2),
                sampled: true,
                promoted_at: Some(Instant::now() - Duration::from_secs(3)),
            },
        );
        let changed = hot_eval(&cfg, &mut cache);
        assert_eq!(changed, 1);
        assert_eq!(cache.hot.promoted_count(), 0);
        assert_ne!(cache.tasks.get(&43).unwrap().cpus, cpus_of(&[1]));
    }

    fn held(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn promotes_saturated_thread() {
        assert_eq!(hot_decision(85, 60, false, held(0), true), HotAction::Promote);
        // Steady moderate load is not a burst: keep it where the rules put it.
        assert_eq!(hot_decision(62, 60, false, held(0), true), HotAction::Hold);
    }

    #[test]
    fn promotes_burst_below_full_saturation() {
        assert_eq!(hot_decision(60, 30, false, held(0), true), HotAction::Promote);
        assert_eq!(hot_decision(58, 20, false, held(0), true), HotAction::Hold);
    }

    #[test]
    fn respects_budget() {
        assert_eq!(hot_decision(95, 40, false, held(0), false), HotAction::Hold);
    }

    #[test]
    fn holds_before_release() {
        assert_eq!(hot_decision(10, 10, true, held(500), true), HotAction::Hold);
        assert_eq!(hot_decision(10, 10, true, held(2000), true), HotAction::Release);
        assert_eq!(hot_decision(40, 30, true, held(9000), true), HotAction::Hold);
    }
}
