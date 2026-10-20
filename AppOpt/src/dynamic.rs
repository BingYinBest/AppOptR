
use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use crate::apply_affinity::affinity_set;
use crate::cache::ProcCache;
use crate::config::{AppConfig, DynSpec};
use crate::rule_match::{cpuset_dir_cached, fnmatch_c, AffinityResult};
const ELECT_MARGIN_PCT: u32 = 20;
const MIN_HOLD: Duration = Duration::from_secs(5);
const LOAD_BAND_PCT: u32 = 5;
pub struct DynTidState {
    pub tier: u8,
    pub switch_at: Instant,
    pub jiffies: u64,
    pub sampled_at: Instant,
    pub load: u32,
}

impl DynTidState {
    fn new() -> Self {
        Self {
            tier: 1,
            switch_at: Instant::now(),
            jiffies: 0,
            sampled_at: Instant::now(),
            load: 0,
        }
    }
}
pub struct DynState {
    pub tids: HashMap<i32, DynTidState>,
    pub winners: HashMap<usize, i32>,
}

impl DynState {
    pub fn new() -> Self {
        Self {
            tids: HashMap::new(),
            winners: HashMap::new(),
        }
    }
}
pub fn load_pct_from(delta_jiffies: u64, elapsed_secs: u64, clk_tck: u64) -> u32 {
    if elapsed_secs == 0 || clk_tck == 0 {
        return 0;
    }
    let total = clk_tck.saturating_mul(elapsed_secs);
    if total == 0 {
        return 0;
    }
    (delta_jiffies.min(total) as u128 * 100 / total as u128) as u32
}
pub fn elect_decide(
    cur_winner: Option<i32>,
    loads: &[(i32, u32)],
    winner_held: bool,
    margin_pct: u32,
) -> Option<i32> {
    if loads.is_empty() {
        return None;
    }
    let Some(cur) = cur_winner else {
        return top_loader(loads);
    };
    let Some(&cur_load) = loads.iter().find(|(id, _)| *id == cur).map(|(_, l)| l) else {
        return top_loader(loads);
    };
    let best_other = loads
        .iter()
        .filter(|(id, _)| *id != cur)
        .map(|(_, l)| *l)
        .max()
        .unwrap_or(0);
    if winner_held && best_other.saturating_mul(100) > cur_load.saturating_mul(100 + margin_pct) {
        top_loader(loads)
    } else {
        None
    }
}
fn top_loader(loads: &[(i32, u32)]) -> Option<i32> {
    loads
        .iter()
        .max_by_key(|(id, l)| (*l, std::cmp::Reverse(*id)))
        .map(|(id, _)| *id)
}
pub fn load_decide(
    cur_tier: u8,
    load_pct: u32,
    threshold: u32,
    band_pct: u32,
    held: bool,
) -> Option<u8> {
    match cur_tier {
        0 => {
            if held && load_pct < threshold.saturating_sub(band_pct) {
                Some(1)
            } else {
                None
            }
        }
        _ => {
            if held && load_pct > threshold.saturating_add(band_pct) {
                Some(0)
            } else {
                None
            }
        }
    }
}
fn thread_cpu_jiffies(pid: i32, tid: i32) -> Option<u64> {
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
pub fn dyn_group_of<'a>(cfg: &'a AppConfig, pkg: &str, comm: &str) -> Option<(usize, &'a DynSpec)> {
    let idxs = cfg.dyn_by_pkg.get(pkg)?;
    for &gi in idxs {
        let rule = &cfg.dyn_rules[gi];
        if fnmatch_c(&rule.thread_pattern, comm) {
            return Some((gi, &rule.spec));
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
        cache.tasks.remove(&tid);
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
pub fn dyn_eval(cfg: &AppConfig, cache: &mut ProcCache) -> usize {
    if cfg.dyn_rules.is_empty() {
        return 0;
    }
    cache.dyn_state.tids.retain(|tid, _| cache.tasks.contains_key(tid));
    cache.dyn_state.winners.retain(|_, tid| cache.tasks.contains_key(tid));

    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let mut changed = 0;

    for (pkg, idxs) in &cfg.dyn_by_pkg {
        for &gi in idxs {
            let rule = &cfg.dyn_rules[gi];
            let mut members: Vec<(i32, u32)> = Vec::new();
            for (tid, e) in cache.tasks.iter() {
                if e.pkg != *pkg || !fnmatch_c(&rule.thread_pattern, &e.comm) {
                    continue;
                }
                let now = Instant::now();
                let st = cache.dyn_state.tids.entry(*tid).or_insert_with(DynTidState::new);
                if let Some(j) = thread_cpu_jiffies(e.pid, *tid) {
                    let elapsed = now.duration_since(st.sampled_at);
                    if st.jiffies > 0 && elapsed >= Duration::from_secs(1) {
                        st.load = load_pct_from(j - st.jiffies, elapsed.as_secs(), clk_tck);
                    } else {
                        st.load = 0;
                    }
                    st.jiffies = j;
                    st.sampled_at = now;
                }
                members.push((*tid, st.load));
            }

            match &rule.spec {
                DynSpec::Elect { .. } => {
                    let cur = cache.dyn_state.winners.get(&gi).copied();
                    let held = cur
                        .and_then(|t| cache.dyn_state.tids.get(&t))
                        .map(|s| s.switch_at.elapsed() >= MIN_HOLD)
                        .unwrap_or(false);
                    if let Some(new_w) = elect_decide(cur, &members, held, ELECT_MARGIN_PCT)
                        && cur != Some(new_w)
                    {
                        cache.dyn_state.winners.insert(gi, new_w);
                        for (tid, _) in &members {
                            changed += apply_tier(cache, cfg, *tid, if *tid == new_w { 0 } else { 1 }, &rule.spec);
                        }
                    }
                }
                DynSpec::Load { threshold, .. } => {
                    for (tid, load) in members {
                        let held = cache
                            .dyn_state
                            .tids
                            .get(&tid)
                            .map(|s| s.switch_at.elapsed() >= MIN_HOLD)
                            .unwrap_or(false);
                        let cur_tier = cache.dyn_state.tids.get(&tid).map(|s| s.tier).unwrap_or(1);
                        if let Some(new_tier) =
                            load_decide(cur_tier, load, *threshold, LOAD_BAND_PCT, held)
                        {
                            changed += apply_tier(cache, cfg, tid, new_tier, &rule.spec);
                        }
                    }
                }
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_pct_basic() {
        assert_eq!(load_pct_from(50, 1, 100), 50);
        assert_eq!(load_pct_from(100, 1, 100), 100);
        assert_eq!(load_pct_from(0, 5, 100), 0);
        assert_eq!(load_pct_from(300, 10, 100), 30);
    }

    #[test]
    fn load_pct_clamps_and_guards() {
        assert_eq!(load_pct_from(0, 0, 100), 0);
        assert_eq!(load_pct_from(10, 5, 0), 0);
        assert_eq!(load_pct_from(999, 1, 100), 100);
        assert_eq!(load_pct_from(500, 1, 100), 100);
    }

    #[test]
    fn elect_picks_top_when_no_winner() {
        let loads = [(1, 10), (2, 80), (3, 50)];
        assert_eq!(elect_decide(None, &loads, true, ELECT_MARGIN_PCT), Some(2));
    }

    #[test]
    fn elect_keeps_winner_within_margin() {
        let loads = [(1, 100), (2, 110)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_MARGIN_PCT), None);
    }

    #[test]
    fn elect_switches_when_clearly_ahead() {
        let loads = [(1, 100), (2, 130)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_MARGIN_PCT), Some(2));
    }

    #[test]
    fn elect_requires_hold() {
        let loads = [(1, 100), (2, 130)];
        assert_eq!(elect_decide(Some(1), &loads, false, ELECT_MARGIN_PCT), None);
    }

    #[test]
    fn elect_replaces_lost_winner() {
        let loads = [(3, 40), (4, 60)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_MARGIN_PCT), Some(4));
    }

    #[test]
    fn elect_tie_breaks_by_smaller_tid() {
        let loads = [(9, 80), (3, 80)];
        assert_eq!(elect_decide(None, &loads, true, ELECT_MARGIN_PCT), Some(3));
    }

    #[test]
    fn load_band_switches_with_hysteresis() {
        assert_eq!(load_decide(1, 60, 50, LOAD_BAND_PCT, true), Some(0));
        assert_eq!(load_decide(1, 50, 50, LOAD_BAND_PCT, true), None);
        assert_eq!(load_decide(0, 40, 50, LOAD_BAND_PCT, true), Some(1));
        assert_eq!(load_decide(0, 50, 50, LOAD_BAND_PCT, true), None);
        assert_eq!(load_decide(1, 90, 50, LOAD_BAND_PCT, false), None);
    }
}
