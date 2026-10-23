
use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use crate::apply_affinity::affinity_set;
use crate::cache::ProcCache;
use crate::config::{AppConfig, DynSpec};
use crate::rule_match::{cpuset_dir_cached, fnmatch_c, AffinityResult};
const MIN_HOLD: Duration = Duration::from_secs(2);
const LOAD_BAND_PCT: u32 = 5;
// 7 点：高于 EMA 平滑后的噪声带（≈5），低于明显领先（≈10），换手收益能覆盖迁移代价
const ELECT_SWITCH_PCT: u32 = 7;
pub struct DynTidState {
    pub tier: u8,
    pub switch_at: Instant,
    pub jiffies: u64,
    pub sampled_at: Instant,
    pub load: u32,
    pub sampled: bool,
}

impl DynTidState {
    fn new() -> Self {
        Self {
            tier: 1,
            // 初始状态从未切换过，不占防抖窗口，让首次负载/档位响应即时生效
            switch_at: Instant::now() - MIN_HOLD,
            jiffies: 0,
            sampled_at: Instant::now(),
            load: 0,
            sampled: false,
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
fn ema_load(prev: u32, instant: u32) -> u32 {
    (prev + instant) / 2
}
pub fn elect_decide(
    cur_winner: Option<i32>,
    loads: &[(i32, u32)],
    winner_held: bool,
    switch_pct: u32,
) -> Option<i32> {
    if loads.is_empty() {
        return None;
    }
    if cur_winner.is_some() && !winner_held {
        // 防抖窗口内保持现有 winner，避免负载相近时频繁换手
        return None;
    }
    let Some(cur) = cur_winner else {
        return top_loader(loads);
    };
    let Some(&cur_load) = loads.iter().find(|(id, _)| *id == cur).map(|(_, l)| l) else {
        // winner 已不在成员中（改名/退出），直接重选
        return top_loader(loads);
    };
    let best = top_loader(loads)?;
    if best == cur {
        return None;
    }
    let best_load = loads
        .iter()
        .find(|(id, _)| *id == best)
        .map(|(_, l)| *l)
        .unwrap_or(0);
    let beats = if cur_load >= 100 - switch_pct {
        // 满载附近 5 点差无法表达，退化为纯大小比较，避免永远不换手
        best_load > cur_load
    } else {
        best_load > cur_load + switch_pct
    };
    if beats {
        Some(best)
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
            let mut any_load = false;
            for (tid, e) in cache.tasks.iter() {
                if e.pkg != *pkg || !fnmatch_c(&rule.thread_pattern, &e.comm) {
                    continue;
                }
                let now = Instant::now();
                let st = cache.dyn_state.tids.entry(*tid).or_insert_with(DynTidState::new);
                if let Some(j) = thread_cpu_jiffies(e.pid, *tid) {
                    let elapsed = now.duration_since(st.sampled_at);
                    if elapsed >= Duration::from_secs(1) {
                        // 首轮 jiffies 可能为 0，但 delta 与 elapsed 同为完整运行期，平均负载仍正确
                        let instant = load_pct_from(j - st.jiffies, elapsed.as_secs(), clk_tck);
                        if st.sampled {
                            st.load = ema_load(st.load, instant);
                        } else {
                            st.load = instant;
                            st.sampled = true;
                        }
                        any_load = true;
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
                    if any_load
                        && let Some(new_w) = elect_decide(cur, &members, held, ELECT_SWITCH_PCT)
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
    use std::collections::{HashMap, HashSet};
    use std::ffi::CString;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;

    use crate::cache::TaskEntry;
    use crate::config::DynRule;
    use crate::cpuset::{CpuSet, CpuTopology};

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
    fn ema_load_smooths_instant_values() {
        assert_eq!(ema_load(0, 0), 0);
        assert_eq!(ema_load(50, 50), 50);
        assert_eq!(ema_load(40, 80), 60);
        assert_eq!(ema_load(80, 20), 50);
    }

    #[test]
    fn elect_picks_top_when_no_winner() {
        let loads = [(1, 10), (2, 80), (3, 50)];
        assert_eq!(elect_decide(None, &loads, true, ELECT_SWITCH_PCT), Some(2));
    }

    #[test]
    fn elect_switches_when_surpass_pct() {
        // 组内负载最高者上高档，challenger 需高过 winner 至少 7 点才换手
        let loads = [(1, 79), (2, 94)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_SWITCH_PCT), Some(2));
        let loads = [(1, 79), (2, 85)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_SWITCH_PCT), None);
        let loads = [(1, 79), (2, 86)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_SWITCH_PCT), None);
        let loads = [(1, 79), (2, 87)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_SWITCH_PCT), Some(2));
        // 满载附近退化：winner 97 时 challenger 100 只高 3 点也应换
        let loads = [(1, 97), (2, 100)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_SWITCH_PCT), Some(2));
        let loads = [(1, 90), (2, 93)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_SWITCH_PCT), None);
    }

    #[test]
    fn elect_requires_hold() {
        let loads = [(1, 100), (2, 130)];
        assert_eq!(elect_decide(Some(1), &loads, false, ELECT_SWITCH_PCT), None);
    }

    #[test]
    fn elect_keeps_winner_on_tie() {
        let loads = [(1, 100), (2, 100)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_SWITCH_PCT), None);
    }

    #[test]
    fn elect_replaces_lost_winner() {
        let loads = [(3, 40), (4, 60)];
        assert_eq!(elect_decide(Some(1), &loads, true, ELECT_SWITCH_PCT), Some(4));
    }

    #[test]
    fn elect_tie_breaks_by_smaller_tid() {
        let loads = [(9, 80), (3, 80)];
        assert_eq!(elect_decide(None, &loads, true, ELECT_SWITCH_PCT), Some(3));
    }

    #[test]
    fn load_band_switches_with_hysteresis() {
        assert_eq!(load_decide(1, 60, 50, LOAD_BAND_PCT, true), Some(0));
        assert_eq!(load_decide(1, 50, 50, LOAD_BAND_PCT, true), None);
        assert_eq!(load_decide(0, 40, 50, LOAD_BAND_PCT, true), Some(1));
        assert_eq!(load_decide(0, 50, 50, LOAD_BAND_PCT, true), None);
        assert_eq!(load_decide(1, 90, 50, LOAD_BAND_PCT, false), None);
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
            cpuset_dirs: Mutex::new(HashSet::new()),
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
            cpuset_dirs: Mutex::new(HashSet::new()),
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
        let spawn_busy = |name: &str| -> i32 {
            let (tx, rx) = mpsc::channel::<i32>();
            let stop_t = stop.clone();
            let name = name.to_string();
            let h = thread::Builder::new()
                .name(name.clone())
                .spawn(move || {
                    tx.send(unsafe { libc::syscall(libc::SYS_gettid) as i32 })
                        .unwrap();
                    while !stop_t.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
                .unwrap();
            let tid = rx.recv().unwrap();
            drop(h);
            tid
        };
        let tid_a = spawn_busy("Thread-1");
        let tid_b = spawn_busy("Thread-2");

        let pid = std::process::id() as i32;
        let cfg = dyn_elect_cfg();
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
        }

        dyn_eval(&cfg, &mut cache);
        assert!(
            cache.dyn_state.winners.is_empty(),
            "首轮无有效负载不应选出 winner"
        );
        thread::sleep(Duration::from_secs(2));
        dyn_eval(&cfg, &mut cache);

        let winner = cache.dyn_state.winners.get(&0).copied();
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

        thread::sleep(Duration::from_secs(2));
        dyn_eval(&cfg, &mut cache);
        assert_eq!(
            cache.dyn_state.winners.get(&0).copied(),
            Some(w),
            "平局时 winner 应保持不换手"
        );

        stop.store(true, Ordering::Relaxed);
    }
}
