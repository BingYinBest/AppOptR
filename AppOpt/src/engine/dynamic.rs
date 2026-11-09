use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use crate::platform::affinity::affinity_set;
use crate::engine::cache::ProcCache;
use crate::config::{AppConfig, DynRule, DynSpec};
use crate::rules::matcher::{cpuset_dir_cached, fnmatch_c, AffinityResult};
use crate::trace;
/// Dynamic-rule scheduler.
///
/// Load is tracked on two time scales: a fast EWMA that reacts within one sample
/// window and a slow EWMA that represents the steady state. A thread is
/// "bursting" when `fast` is `BURST_ENTER_PCT` above `slow` while `fast` is
/// already above `BURST_MIN_PCT`. Bursting threads are promoted to the high
/// tier immediately; returning to the low tier requires `DEMOTE_HOLD` of calm.
pub const DYN_PROBE: Duration = Duration::from_millis(500);
const SWITCH_HOLD: Duration = Duration::from_secs(2);
const BURST_HOLD: Duration = Duration::from_secs(1);
const DEMOTE_HOLD: Duration = Duration::from_secs(6);
const LOAD_BAND_PCT: u32 = 5;
const ELECT_SWITCH_PCT: u32 = 7;
const FAST_WEIGHT_PCT: u32 = 60;
const SLOW_WEIGHT_PCT: u32 = 15;
const BURST_ENTER_PCT: u32 = 25;
const BURST_MIN_PCT: u32 = 45;
const BURST_TAKEOVER_PCT: u32 = 70;
const IDLE_WINNER_PCT: u32 = 15;
const IDLE_CHALLENGER_PCT: u32 = 35;
const IDLE_LEAD_PCT: u32 = 20;
const PRUNE_EVERY: Duration = Duration::from_secs(5);

pub struct DynTidState {
    pub tier: u8,
    pub switch_at: Instant,
    pub jiffies: u64,
    pub sampled_at: Instant,
    /// Fast EWMA, reacts to bursts within one sample window.
    pub fast: u32,
    /// Slow EWMA, approximates steady-state load.
    pub slow: u32,
    pub sampled: bool,
}

impl DynTidState {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            tier: 1,
            switch_at: now - SWITCH_HOLD,
            jiffies: 0,
            sampled_at: now,
            fast: 0,
            slow: 0,
            sampled: false,
        }
    }
}

pub struct DynState {
    pub tids: HashMap<i32, DynTidState>,
    /// Group id -> current winner.
    pub winners: HashMap<u64, i32>,
    pub last_prune: Option<Instant>,
}

impl DynState {
    pub fn new() -> Self {
        Self {
            tids: HashMap::new(),
            winners: HashMap::new(),
            last_prune: None,
        }
    }
}

/// CPU usage percentage over an arbitrary window (milliseconds).
pub fn load_pct(delta_jiffies: u64, elapsed_ms: u64, clk_tck: u64) -> u32 {
    if elapsed_ms == 0 || clk_tck == 0 {
        return 0;
    }
    let capacity = clk_tck.saturating_mul(elapsed_ms);
    if capacity == 0 {
        return 0;
    }
    let pct = (delta_jiffies as u128 * 100_000) / capacity as u128;
    pct.min(100) as u32
}

pub fn ewma(prev: u32, instant: u32, weight_pct: u32) -> u32 {
    if weight_pct >= 100 {
        return instant.min(100);
    }
    let kept = prev as u64 * (100 - weight_pct) as u64;
    let fresh = instant as u64 * weight_pct as u64;
    ((kept + fresh) / 100).min(100) as u32
}

pub fn burst_score(fast: u32, slow: u32) -> u32 {
    fast.saturating_sub(slow)
}

pub fn is_burst(fast: u32, slow: u32) -> bool {
    fast >= BURST_MIN_PCT && burst_score(fast, slow) >= BURST_ENTER_PCT
}

/// `loads` entries are `(tid, fast_load, slow_load)`.
///
/// Burst takeover: a challenger that is clearly bursting can replace a winner
/// that has been in place for `BURST_HOLD`, without waiting for `SWITCH_HOLD`.
pub fn elect_decide(
    cur_winner: Option<i32>,
    loads: &[(i32, u32, u32)],
    since_switch: Duration,
    switch_pct: u32,
) -> Option<i32> {
    if loads.is_empty() {
        return None;
    }
    let Some(cur) = cur_winner else {
        return top_loader(loads).map(|(id, _, _)| id);
    };
    let Some((_, cur_fast, _cur_slow)) = loads.iter().find(|(id, _, _)| *id == cur).copied() else {
        return top_loader(loads).map(|(id, _, _)| id);
    };
    let (best, best_fast, best_slow) = top_loader(loads)?;
    if best == cur {
        return None;
    }
    if best_fast >= BURST_TAKEOVER_PCT && is_burst(best_fast, best_slow) {
        return (since_switch >= BURST_HOLD).then_some(best);
    }
    // A winner that already went idle should not block a clearly busier
    // challenger for the full SWITCH_HOLD window.
    if cur_fast <= IDLE_WINNER_PCT
        && best_fast >= IDLE_CHALLENGER_PCT
        && best_fast >= cur_fast + IDLE_LEAD_PCT
    {
        return (since_switch >= BURST_HOLD).then_some(best);
    }
    if since_switch < SWITCH_HOLD {
        return None;
    }
    // A nearly saturated winner only needs a higher challenger, which keeps
    // the winner from thrashing while the whole group is hot.
    let beats = if cur_fast >= 100 - switch_pct {
        best_fast > cur_fast
    } else {
        best_fast > cur_fast + switch_pct
    };
    beats.then_some(best)
}

fn top_loader(loads: &[(i32, u32, u32)]) -> Option<(i32, u32, u32)> {
    loads
        .iter()
        .max_by_key(|(id, fast, slow)| (*fast, *slow, std::cmp::Reverse(*id)))
        .map(|(id, fast, slow)| (*id, *fast, *slow))
}

pub fn load_decide(
    cur_tier: u8,
    fast: u32,
    slow: u32,
    threshold: u32,
    band_pct: u32,
    since_switch: Duration,
) -> Option<u8> {
    match cur_tier {
        0 => {
            let calm = fast <= threshold.saturating_sub(band_pct)
                && slow <= threshold.saturating_add(band_pct)
                && burst_score(fast, slow) < BURST_ENTER_PCT / 2;
            (calm && since_switch >= DEMOTE_HOLD).then_some(1)
        }
        _ => {
            if is_burst(fast, slow) || fast > threshold.saturating_add(band_pct) {
                Some(0)
            } else {
                None
            }
        }
    }
}
pub(crate) fn thread_cpu_jiffies(pid: i32, tid: i32) -> Option<u64> {
    let mut path = [0u8; 64];
    let mut cur = std::io::Cursor::new(&mut path[..]);
    write!(cur, "/proc/{}/task/{}/stat", pid, tid).ok()?;
    let len = cur.position() as usize;
    let path_str = std::str::from_utf8(&path[..len]).ok()?;
    let file = fs::File::open(path_str).ok()?;
    let mut buf = [0u8; 512];
    let n = file.read_at(&mut buf, 0).ok()?;
    if n == 0 {
        return None;
    }
    let s = std::str::from_utf8(&buf[..n]).ok()?;
    let rparen = s.rfind(')')?;
    let fields: Vec<&str> = s[rparen + 1..].split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime.saturating_add(stime))
}
pub fn dyn_group_of<'a>(
    cfg: &'a AppConfig,
    pkg: &str,
    comm: &str,
) -> Option<(u64, &'a DynSpec)> {
    let idxs = cfg.dyn_by_pkg.get(pkg)?;
    for &idx in idxs {
        let rule = &cfg.dyn_rules[idx];
        if fnmatch_c(&rule.thread_pattern, comm) {
            return Some((rule.group_id, &rule.spec));
        }
    }
    None
}
pub fn dyn_resolve(
    cache: &mut ProcCache,
    tid: i32,
    spec: &DynSpec,
    cfg: &AppConfig,
) -> AffinityResult {
    let tier = cache
        .dyn_state
        .tids
        .get(&tid)
        .map(|s| s.tier)
        .unwrap_or_else(|| {
            cache.dyn_state.tids.insert(tid, DynTidState::new());
            1
        });
    let cpus = spec.tier_cpus(tier);
    let cpuset_dir = cpuset_dir_cached(&cpus, cfg);
    AffinityResult {
        cpus,
        cpuset_dir,
        is_thread_rule: true,
    }
}
fn apply_tier(cache: &mut ProcCache, cfg: &AppConfig, tid: i32, want: u8, spec: &DynSpec) -> usize {
    if cache.dyn_state.tids.get(&tid).map(|s| s.tier) == Some(want) {
        return 0;
    }
    let cpus = spec.tier_cpus(want);
    let cpuset_dir = cpuset_dir_cached(&cpus, cfg);
    if affinity_set(tid, &cpus, &cpuset_dir, &cfg.topo) {
        cache.task_del(tid);
        cache.dyn_state.tids.remove(&tid);
        return 0;
    }
    if let Some(st) = cache.dyn_state.tids.get_mut(&tid) {
        st.tier = want;
        st.switch_at = Instant::now();
    }
    if let Some(e) = cache.tasks.get_mut(&tid) {
        e.cpus = cpus;
        e.cpuset_dir = cpuset_dir;
    }
    1
}
/// Samples thread CPU time and updates both EWMAs. A sample needs a >=1s window
/// so the percentage is not dominated by scheduling noise; shorter probes reuse
/// the previous values.
fn sample_load(
    states: &mut HashMap<i32, DynTidState>,
    tid: i32,
    pid: i32,
    now: Instant,
    clk_tck: u64,
) -> (u32, u32, bool) {
    let Some(jiffies) = thread_cpu_jiffies(pid, tid) else {
        let (fast, slow) = states
            .get(&tid)
            .map(|s| (s.fast, s.slow))
            .unwrap_or((0, 0));
        return (fast, slow, false);
    };
    let st = states.entry(tid).or_insert_with(DynTidState::new);
    let elapsed_ms = now.duration_since(st.sampled_at).as_millis() as u64;
    let mut sampled = false;
    if elapsed_ms >= DYN_PROBE.as_millis() as u64 {
        let instant = load_pct(jiffies.saturating_sub(st.jiffies), elapsed_ms, clk_tck);
        if st.sampled {
            st.fast = ewma(st.fast, instant, FAST_WEIGHT_PCT);
            st.slow = ewma(st.slow, instant, SLOW_WEIGHT_PCT);
        } else {
            st.fast = instant;
            st.slow = instant;
        }
        st.sampled = true;
        sampled = true;
        st.jiffies = jiffies;
        st.sampled_at = now;
    }
    (st.fast, st.slow, sampled)
}

fn prune_groups(cfg: &AppConfig, cache: &mut ProcCache) {
    let live_groups: HashSet<u64> = cfg.dyn_rules.iter().map(|r| r.group_id).collect();
    cache.dyn_members.retain(|gid, set| {
        set.retain(|tid| cache.tasks.contains_key(tid));
        live_groups.contains(gid) && !set.is_empty()
    });
    let members: HashSet<i32> = cache.dyn_members.values().flatten().copied().collect();
    cache.dyn_state.tids.retain(|tid, _| members.contains(tid));
    cache.dyn_state.winners.retain(|_, tid| members.contains(tid));
}

fn eval_group(
    cfg: &AppConfig,
    cache: &mut ProcCache,
    pkg: &str,
    rule: &DynRule,
    now: Instant,
    clk_tck: u64,
) -> usize {
    let gid = rule.group_id;
    let Some(candidates) = cache.dyn_members.get(&gid) else {
        return 0;
    };
    let mut members: Vec<(i32, u32, u32)> = Vec::new();
    let mut any_load = false;
    for &tid in candidates {
        let (pid, matched) = {
            let Some(e) = cache.tasks.get(&tid) else {
                continue;
            };
            (e.pid, e.pkg == pkg && fnmatch_c(&rule.thread_pattern, &e.comm))
        };
        if !matched {
            continue;
        }
        let (fast, slow, sampled) = sample_load(&mut cache.dyn_state.tids, tid, pid, now, clk_tck);
        any_load |= sampled;
        members.push((tid, fast, slow));
    }

    let mut changed = 0;
    match &rule.spec {
        DynSpec::Elect { .. } => {
            let cur = cache.dyn_state.winners.get(&gid).copied();
            let since_switch = cur
                .and_then(|t| cache.dyn_state.tids.get(&t))
                .map(|s| s.switch_at.elapsed())
                .unwrap_or(SWITCH_HOLD);
            if any_load
                && let Some(new_w) = elect_decide(cur, &members, since_switch, ELECT_SWITCH_PCT)
                && cur != Some(new_w)
            {
                if let Some((_, bf, bs)) = members.iter().find(|(t, _, _)| *t == new_w) {
                    trace::log("elec", new_w, *bf, *bs, "winner");
                }
                cache.dyn_state.winners.insert(gid, new_w);
                for (tid, _, _) in &members {
                    let tier = if *tid == new_w { 0 } else { 1 };
                    changed += apply_tier(cache, cfg, *tid, tier, &rule.spec);
                }
            }
        }
        DynSpec::Load { threshold, .. } => {
            for (tid, fast, slow) in members {
                let since_switch = cache
                    .dyn_state
                    .tids
                    .get(&tid)
                    .map(|s| s.switch_at.elapsed())
                    .unwrap_or(DEMOTE_HOLD);
                let cur_tier = cache.dyn_state.tids.get(&tid).map(|s| s.tier).unwrap_or(1);
                if fast >= BURST_MIN_PCT {
                    trace::log("dync", tid, fast, slow, "cand");
                }
                if let Some(new_tier) =
                    load_decide(cur_tier, fast, slow, *threshold, LOAD_BAND_PCT, since_switch)
                {
                    trace::log(
                        "dyn",
                        tid,
                        fast,
                        slow,
                        if new_tier == 0 { "to-high" } else { "to-low" },
                    );
                    changed += apply_tier(cache, cfg, tid, new_tier, &rule.spec);
                }
            }
        }
    }
    changed
}

pub fn dyn_eval(cfg: &AppConfig, cache: &mut ProcCache) -> usize {
    if cfg.dyn_rules.is_empty() {
        return 0;
    }
    let now = Instant::now();
    let prune_due = cache
        .dyn_state
        .last_prune
        .map(|t| t.elapsed() >= PRUNE_EVERY)
        .unwrap_or(true);
    if prune_due {
        prune_groups(cfg, cache);
        cache.dyn_state.last_prune = Some(now);
    }

    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let mut changed = 0;
    for (pkg, idxs) in &cfg.dyn_by_pkg {
        for &idx in idxs {
            let rule = &cfg.dyn_rules[idx];
            changed += eval_group(cfg, cache, pkg, rule, now, clk_tck);
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::ffi::CString;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;

    use crate::engine::cache::TaskEntry;
    use crate::config::{dyn_group_id, DynRule};
    use crate::platform::cpuset::{CpuSet, CpuTopology};

    #[test]
    fn load_pct_basic() {
        assert_eq!(load_pct(50, 1000, 100), 50);
        assert_eq!(load_pct(100, 1000, 100), 100);
        assert_eq!(load_pct(0, 5000, 100), 0);
        assert_eq!(load_pct(300, 10_000, 100), 30);
        // Sub-second windows must keep the same scale as one-second windows.
        assert_eq!(load_pct(50, 500, 100), 100);
    }

    #[test]
    fn load_pct_clamps_and_guards() {
        assert_eq!(load_pct(0, 0, 100), 0);
        assert_eq!(load_pct(10, 5000, 0), 0);
        assert_eq!(load_pct(999, 1000, 100), 100);
        assert_eq!(load_pct(500, 1000, 100), 100);
    }

    #[test]
    fn ewma_weights_new_sample() {
        assert_eq!(ewma(0, 0, FAST_WEIGHT_PCT), 0);
        assert_eq!(ewma(50, 50, FAST_WEIGHT_PCT), 50);
        assert_eq!(ewma(40, 80, 60), 64);
        assert_eq!(ewma(40, 80, 15), 46);
        assert_eq!(ewma(90, 10, 100), 10);
    }

    #[test]
    fn burst_score_detects_sudden_jump() {
        assert_eq!(burst_score(80, 40), 40);
        assert_eq!(burst_score(40, 80), 0);
        assert!(is_burst(80, 40));
        assert!(!is_burst(40, 40));
        assert!(!is_burst(30, 0), "below the minimum absolute load");
    }

    #[test]
    fn elect_picks_top_when_no_winner() {
        let loads = [(1, 10, 10), (2, 80, 80), (3, 50, 50)];
        assert_eq!(
            elect_decide(None, &loads, SWITCH_HOLD, ELECT_SWITCH_PCT),
            Some(2)
        );
    }

    #[test]
    fn elect_switches_when_surpass_pct() {
        let loads = [(1, 79, 79), (2, 94, 94)];
        assert_eq!(
            elect_decide(Some(1), &loads, SWITCH_HOLD, ELECT_SWITCH_PCT),
            Some(2)
        );
        let loads = [(1, 79, 79), (2, 85, 85)];
        assert_eq!(
            elect_decide(Some(1), &loads, SWITCH_HOLD, ELECT_SWITCH_PCT),
            None
        );
        let loads = [(1, 79, 79), (2, 87, 87)];
        assert_eq!(
            elect_decide(Some(1), &loads, SWITCH_HOLD, ELECT_SWITCH_PCT),
            Some(2)
        );
        let loads = [(1, 97, 97), (2, 100, 100)];
        assert_eq!(
            elect_decide(Some(1), &loads, SWITCH_HOLD, ELECT_SWITCH_PCT),
            Some(2)
        );
    }

    #[test]
    fn elect_requires_switch_hold() {
        let loads = [(1, 100, 100), (2, 100, 100)];
        assert_eq!(
            elect_decide(Some(1), &loads, Duration::from_millis(500), ELECT_SWITCH_PCT),
            None
        );
    }

    #[test]
    fn elect_keeps_winner_on_tie() {
        let loads = [(1, 100, 100), (2, 100, 100)];
        assert_eq!(
            elect_decide(Some(1), &loads, SWITCH_HOLD, ELECT_SWITCH_PCT),
            None
        );
    }

    #[test]
    fn elect_replaces_lost_winner() {
        let loads = [(3, 40, 40), (4, 60, 60)];
        assert_eq!(
            elect_decide(Some(1), &loads, SWITCH_HOLD, ELECT_SWITCH_PCT),
            Some(4)
        );
    }

    #[test]
    fn elect_tie_breaks_by_smaller_tid() {
        let loads = [(9, 80, 80), (3, 80, 80)];
        assert_eq!(
            elect_decide(None, &loads, SWITCH_HOLD, ELECT_SWITCH_PCT),
            Some(3)
        );
    }

    #[test]
    fn elect_burst_takeover_bypasses_switch_hold() {
        // The steady winner is busy but a challenger suddenly spikes.
        let loads = [(1, 60, 60), (2, 95, 30)];
        assert_eq!(
            elect_decide(Some(1), &loads, BURST_HOLD, ELECT_SWITCH_PCT),
            Some(2)
        );
        // Same data but the winner changed too recently: no flip-flop.
        let loads = [(1, 60, 60), (2, 95, 30)];
        assert_eq!(
            elect_decide(Some(1), &loads, Duration::from_millis(200), ELECT_SWITCH_PCT),
            None
        );
    }

    #[test]
    fn elect_hands_off_when_winner_goes_idle() {
        let loads = [(1, 10, 40), (2, 45, 30)];
        assert_eq!(
            elect_decide(Some(1), &loads, BURST_HOLD, ELECT_SWITCH_PCT),
            Some(2)
        );
        assert_eq!(
            elect_decide(Some(1), &loads, Duration::from_millis(300), ELECT_SWITCH_PCT),
            None
        );
        // A moderately busy winner keeps the regular hold window.
        let loads = [(1, 30, 30), (2, 45, 30)];
        assert_eq!(
            elect_decide(Some(1), &loads, BURST_HOLD, ELECT_SWITCH_PCT),
            None
        );
    }

    #[test]
    fn load_demote_needs_slow_band_clear() {
        assert_eq!(
            load_decide(0, 30, 70, 50, LOAD_BAND_PCT, DEMOTE_HOLD),
            None
        );
        assert_eq!(
            load_decide(0, 20, 20, 50, LOAD_BAND_PCT, DEMOTE_HOLD),
            Some(1)
        );
    }

    #[test]
    fn elect_burst_takeover_needs_high_load() {
        // A 20-point jump is not a burst and 60 is below BURST_TAKEOVER_PCT.
        let loads = [(1, 55, 55), (2, 60, 40)];
        assert_eq!(
            elect_decide(Some(1), &loads, SWITCH_HOLD, ELECT_SWITCH_PCT),
            None
        );
    }

    #[test]
    fn load_band_switches_with_hysteresis() {
        assert_eq!(load_decide(1, 60, 60, 50, LOAD_BAND_PCT, Duration::ZERO), Some(0));
        assert_eq!(load_decide(1, 50, 50, 50, LOAD_BAND_PCT, Duration::ZERO), None);
        assert_eq!(
            load_decide(0, 40, 40, 50, LOAD_BAND_PCT, DEMOTE_HOLD),
            Some(1)
        );
        assert_eq!(
            load_decide(0, 50, 50, 50, LOAD_BAND_PCT, DEMOTE_HOLD),
            None
        );
    }

    #[test]
    fn load_burst_promotes_below_threshold() {
        // Fast load jumps to 70 from a 20 baseline: promote even though the
        // configured threshold is 90, because this is a burst.
        assert_eq!(load_decide(1, 70, 20, 90, LOAD_BAND_PCT, Duration::ZERO), Some(0));
        // Same fast load without a jump is steady state and stays low.
        assert_eq!(load_decide(1, 70, 70, 90, LOAD_BAND_PCT, Duration::ZERO), None);
    }

    #[test]
    fn load_demote_waits_for_calm_and_hold() {
        // Calm for 6s: demote.
        assert_eq!(load_decide(0, 20, 20, 50, LOAD_BAND_PCT, DEMOTE_HOLD), Some(1));
        // Calm but hold not reached: keep the high tier.
        assert_eq!(
            load_decide(0, 20, 20, 50, LOAD_BAND_PCT, Duration::from_secs(5)),
            None
        );
        // Load dropped but the thread was just bursting: keep the high tier.
        assert_eq!(load_decide(0, 45, 10, 90, LOAD_BAND_PCT, DEMOTE_HOLD), None);
    }

    fn cpus_of(cpus: &[usize]) -> CpuSet {
        let mut s = CpuSet::new();
        for &c in cpus {
            s.set(c);
        }
        s
    }

    fn dyn_elect_cfg() -> AppConfig {
        let dyn_rules = vec![DynRule {
            pkg: "com.b".to_string(),
            thread: "Thread-*".to_string(),
            thread_pattern: CString::new("Thread-*").unwrap(),
            spec: DynSpec::Elect {
                winner: Box::new(cpus_of(&[4, 5, 6, 7])),
                others: Box::new(cpus_of(&[0, 1, 2, 3])),
            },
            group_id: dyn_group_id("com.b", "Thread-*"),
        }];
        let mut dyn_by_pkg: HashMap<String, Vec<usize>> = HashMap::new();
        dyn_by_pkg.entry("com.b".to_string()).or_default().push(0);
        let mut pkgs = HashSet::new();
        pkgs.insert("com.b".to_string());
        let mut has_dyn_rules = HashSet::new();
        has_dyn_rules.insert("com.b".to_string());
        AppConfig {
            rules: Vec::new(),
            pkgs,
            has_thread_rules: HashSet::new(),
            topo: CpuTopology {
                present_cpus: cpus_of(&[0, 1, 2, 3, 4, 5, 6, 7]),
                present_str: "0-7".to_string(),
                mems_str: "0".to_string(),
                cpuset_enabled: false,
                e_core: cpus_of(&[0, 1, 2, 3]),
                m_core: CpuSet::new(),
                p_core: CpuSet::new(),
                hp_core: cpus_of(&[4, 5, 6, 7]),
            },
            rules_by_pkg: HashMap::new(),
            exact_thread_rules: HashMap::new(),
            dyn_rules,
            dyn_by_pkg,
            has_dyn_rules,
            cpuset_dirs: Mutex::new(HashMap::new()),
            comm_prefixes: HashSet::new(),
            comm_suffixes: HashSet::new(),
        }
    }

    fn dyn_load_cfg() -> AppConfig {
        let dyn_rules = vec![DynRule {
            pkg: "com.a".to_string(),
            thread: "Thread-*".to_string(),
            thread_pattern: CString::new("Thread-*").unwrap(),
            spec: DynSpec::Load {
                high: Box::new(cpus_of(&[4, 5, 6, 7])),
                low: Box::new(cpus_of(&[0, 1, 2, 3])),
                threshold: 50,
            },
            group_id: dyn_group_id("com.a", "Thread-*"),
        }];
        let mut dyn_by_pkg: HashMap<String, Vec<usize>> = HashMap::new();
        dyn_by_pkg.entry("com.a".to_string()).or_default().push(0);
        let mut pkgs = HashSet::new();
        pkgs.insert("com.a".to_string());
        let mut has_dyn_rules = HashSet::new();
        has_dyn_rules.insert("com.a".to_string());
        AppConfig {
            rules: Vec::new(),
            pkgs,
            has_thread_rules: HashSet::new(),
            topo: CpuTopology {
                present_cpus: cpus_of(&[0, 1, 2, 3, 4, 5, 6, 7]),
                present_str: "0-7".to_string(),
                mems_str: "0".to_string(),
                cpuset_enabled: false,
                e_core: cpus_of(&[0, 1, 2, 3]),
                m_core: CpuSet::new(),
                p_core: CpuSet::new(),
                hp_core: cpus_of(&[4, 5, 6, 7]),
            },
            rules_by_pkg: HashMap::new(),
            exact_thread_rules: HashMap::new(),
            dyn_rules,
            dyn_by_pkg,
            has_dyn_rules,
            cpuset_dirs: Mutex::new(HashMap::new()),
            comm_prefixes: HashSet::new(),
            comm_suffixes: HashSet::new(),
        }
    }

    #[test]
    fn dyn_eval_load_switches_busy_thread_to_high() {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx_busy, rx_busy) = mpsc::channel::<i32>();
        let stop_busy = stop.clone();
        let h_busy = thread::Builder::new()
            .name("Thread-1".to_string())
            .spawn(move || {
                tx_busy
                    .send(unsafe { libc::syscall(libc::SYS_gettid) as i32 })
                    .unwrap();
                while !stop_busy.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
            .unwrap();
        let tid_busy = rx_busy.recv().unwrap();

        let (tx_idle, rx_idle) = mpsc::channel::<i32>();
        let stop_idle = stop.clone();
        let h_idle = thread::Builder::new()
            .name("Thread-2".to_string())
            .spawn(move || {
                tx_idle
                    .send(unsafe { libc::syscall(libc::SYS_gettid) as i32 })
                    .unwrap();
                while !stop_idle.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                }
            })
            .unwrap();
        let tid_idle = rx_idle.recv().unwrap();

        let pid = std::process::id() as i32;
        let cfg = dyn_load_cfg();
        let gid = cfg.dyn_rules[0].group_id;
        let mut cache = ProcCache::new();
        for (tid, comm) in [(tid_busy, "Thread-1"), (tid_idle, "Thread-2")] {
            cache.tasks.insert(
                tid,
                TaskEntry {
                    pid,
                    pkg: "com.a".to_string(),
                    comm: comm.to_string(),
                    cpus: CpuSet::new(),
                    cpuset_dir: String::new(),
                    is_thread_rule: true,
                },
            );
            cache.dyn_members.entry(gid).or_default().insert(tid);
            cache.tid_group.insert(tid, gid);
        }

        dyn_eval(&cfg, &mut cache);
        thread::sleep(Duration::from_secs(2));
        dyn_eval(&cfg, &mut cache);

        assert_eq!(
            cache.dyn_state.tids.get(&tid_busy).map(|s| s.tier),
            Some(0),
            "满载线程应切到高档"
        );
        assert_eq!(
            cache.dyn_state.tids.get(&tid_idle).map(|s| s.tier),
            Some(1),
            "空闲线程保持低档"
        );

        stop.store(true, Ordering::Relaxed);
        h_busy.join().unwrap();
        h_idle.join().unwrap();
    }

    #[test]
    fn dyn_eval_elect_establishes_winner_after_first_sample() {
        let stop = Arc::new(AtomicBool::new(false));
        let spawn_busy = |name: &str| -> (i32, Arc<AtomicBool>) {
            let (tx, rx) = mpsc::channel::<i32>();
            let stop_t = stop.clone();
            let pause = Arc::new(AtomicBool::new(false));
            let pause_t = pause.clone();
            let name = name.to_string();
            let h = thread::Builder::new()
                .name(name.clone())
                .spawn(move || {
                    tx.send(unsafe { libc::syscall(libc::SYS_gettid) as i32 })
                        .unwrap();
                    while !stop_t.load(Ordering::Relaxed) {
                        if pause_t.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(10));
                        } else {
                            std::hint::spin_loop();
                        }
                    }
                })
                .unwrap();
            let tid = rx.recv().unwrap();
            drop(h);
            (tid, pause)
        };
        let (tid_a, pause_a) = spawn_busy("Thread-1");
        let (tid_b, pause_b) = spawn_busy("Thread-2");

        let pid = std::process::id() as i32;
        let cfg = dyn_elect_cfg();
        let gid = cfg.dyn_rules[0].group_id;
        let mut cache = ProcCache::new();
        for (tid, comm) in [(tid_a, "Thread-1"), (tid_b, "Thread-2")] {
            cache.tasks.insert(
                tid,
                TaskEntry {
                    pid,
                    pkg: "com.b".to_string(),
                    comm: comm.to_string(),
                    cpus: CpuSet::new(),
                    cpuset_dir: String::new(),
                    is_thread_rule: true,
                },
            );
            cache.dyn_members.entry(gid).or_default().insert(tid);
            cache.tid_group.insert(tid, gid);
        }

        dyn_eval(&cfg, &mut cache);
        assert!(
            cache.dyn_state.winners.is_empty(),
            "首轮无有效负载不应选出 winner"
        );
        thread::sleep(Duration::from_secs(2));
        dyn_eval(&cfg, &mut cache);

        let winner = cache.dyn_state.winners.get(&gid).copied();
        assert!(winner.is_some(), "有负载后应建立 winner");
        let w = winner.unwrap();
        assert_eq!(
            cache.dyn_state.tids.get(&w).map(|s| s.tier),
            Some(0),
            "winner 应绑高档"
        );
        let other = if w == tid_a { tid_b } else { tid_a };
        assert_eq!(
            cache.dyn_state.tids.get(&other).map(|s| s.tier),
            Some(1),
            "非 winner 线程保持低档"
        );

        if w == tid_a { pause_b.store(true, Ordering::Relaxed); } else { pause_a.store(true, Ordering::Relaxed); }
        thread::sleep(Duration::from_secs(2));
        dyn_eval(&cfg, &mut cache);
        assert_eq!(
            cache.dyn_state.winners.get(&gid).copied(),
            Some(w),
            "负载悬殊时 winner 应保持不换手"
        );

        stop.store(true, Ordering::Relaxed);
    }
}
