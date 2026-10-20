use std::ffi::CString;

use crate::{lock_ignore_poison, MAX_THREAD_LEN};
use crate::apply_affinity::read_cmdline;
use crate::config::{pattern_has_meta, AppConfig};
use crate::cpuset::{base_cpuset, create_cpuset_dir, CpuSet};

/// 线程亲和性计算结果
pub struct AffinityResult {
    pub cpus: CpuSet,
    pub cpuset_dir: String,
    pub is_thread_rule: bool,
}

/// 线程规则合并后的 cpuset 目录，缓存已存在目录避免重复创建系统调用
pub(crate) fn cpuset_dir_cached(cpus: &CpuSet, cfg: &AppConfig) -> String {
    if !cfg.topo.cpuset_enabled {
        return String::new();
    }
    let dir_name = cpus.to_range_string();
    {
        let dirs = lock_ignore_poison(&cfg.cpuset_dirs);
        if dirs.contains(&dir_name) {
            return dir_name;
        }
    }
    let path = format!("{}/{}", base_cpuset(), dir_name);
    if create_cpuset_dir(&path, &dir_name, &cfg.topo.mems_str) {
        lock_ignore_poison(&cfg.cpuset_dirs).insert(dir_name.clone());
        dir_name
    } else {
        String::new()
    }
}

/// 线程规则 CPU 累加，无线程匹配走包级 fallback，仍无则返回 None
pub fn thread_affinity(
    pkg: &str,
    thread: &str,
    cfg: &AppConfig,
) -> Option<AffinityResult> {
    let mut cpus = CpuSet::new();
    let mut cpuset_dir = String::new();
    let mut matched = false;

    let pkg_rules: &[usize] = cfg.rules_by_pkg.get(pkg).map(Vec::as_slice).unwrap_or(&[]);

    if !thread.is_empty() {
        // 精确线程名 O(1)，未命中再对该包含通配符的规则做 fnmatch
        if let Some(exact) = cfg.exact_thread_rules.get(pkg).and_then(|m| m.get(thread)) {
            for &i in exact {
                cpus.or(&cfg.rules[i].cpus);
                matched = true;
            }
        }
        for &i in pkg_rules {
            let rule = &cfg.rules[i];
            if rule.thread.is_empty() || !pattern_has_meta(&rule.thread) {
                continue;
            }
            if fnmatch_c(&rule.thread_pattern, thread) {
                cpus.or(&rule.cpus);
                matched = true;
            }
        }
        // 按合并后的 CPU 集合重算 cpuset 目录，确保与亲和性一致
        if matched {
            cpuset_dir = cpuset_dir_cached(&cpus, cfg);
        }
    }

    if !matched {
        for &i in pkg_rules {
            let rule = &cfg.rules[i];
            if !rule.thread.is_empty() {
                continue;
            }
            cpus.or(&rule.cpus);
        }
        if cpus.count() > 0 {
            cpuset_dir = cpuset_dir_cached(&cpus, cfg);
        }
    }

    if cpus.count() == 0 {
        if cfg.has_thread_rules.contains(pkg) {
            return Some(AffinityResult {
                cpus: cfg.topo.present_cpus,
                cpuset_dir: String::new(),
                is_thread_rule: false,
            });
        }
        None
    } else {
        Some(AffinityResult {
            cpus,
            cpuset_dir,
            is_thread_rule: matched,
        })
    }
}

/// POSIX fnmatch 封装，需预转换为 CString
pub(crate) fn fnmatch_c(pattern: &CString, string: &str) -> bool {
    if string.len() >= MAX_THREAD_LEN {
        return false;
    }
    let mut buf = [0u8; MAX_THREAD_LEN];
    buf[..string.len()].copy_from_slice(string.as_bytes());
    unsafe { libc::fnmatch(pattern.as_ptr(), buf.as_ptr() as *const _, libc::FNM_NOESCAPE) == 0 }
}

/// 通过内核 comm 匹配配置包名
pub fn comm_to_pkg(pid: i32, comm: &str, cfg: &AppConfig) -> Option<String> {
    if cfg.pkgs.contains(comm) {
        return Some(comm.to_string());
    }
    // comm 大于等于15字节则读取 cmdline
    if comm.len() >= 15
        && cfg
            .pkgs
            .iter()
            .any(|p| p.starts_with(comm) || p.ends_with(comm))
    {
        return read_cmdline(pid).filter(|name| cfg.pkgs.contains(name));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use crate::config::AffinityRule;
    use crate::cpuset::CpuTopology;

    fn cpus_of(cpus: &[usize]) -> CpuSet {
        let mut s = CpuSet::new();
        for &c in cpus {
            s.set(c);
        }
        s
    }

    fn topo() -> CpuTopology {
        CpuTopology {
            present_cpus: cpus_of(&[0, 1, 2, 3, 4, 5, 6]),
            present_str: "0-6".to_string(),
            mems_str: "0".to_string(),
            cpuset_enabled: false,
            e_core: cpus_of(&[0, 1]),
            m_core: CpuSet::new(),
            p_core: cpus_of(&[2, 3]),
            hp_core: cpus_of(&[4, 5, 6]),
        }
    }

    fn pkg_rule(pkg: &str, cpus: &[usize]) -> AffinityRule {
        AffinityRule {
            pkg: pkg.to_string(),
            thread: String::new(),
            thread_pattern: CString::new("").unwrap(),
            cpus: cpus_of(cpus),
        }
    }

    fn thread_rule(pkg: &str, thread: &str, cpus: &[usize]) -> AffinityRule {
        AffinityRule {
            pkg: pkg.to_string(),
            thread: thread.to_string(),
            thread_pattern: CString::new(thread).unwrap(),
            cpus: cpus_of(cpus),
        }
    }

    fn cfg(rules: Vec<AffinityRule>) -> AppConfig {
        let pkgs: HashSet<String> = rules.iter().map(|r| r.pkg.clone()).collect();
        let has_thread_rules: HashSet<String> = rules
            .iter()
            .filter(|r| !r.thread.is_empty())
            .map(|r| r.pkg.clone())
            .collect();
        let mut rules_by_pkg: HashMap<String, Vec<usize>> = HashMap::new();
        let mut exact_thread_rules: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        for (i, r) in rules.iter().enumerate() {
            rules_by_pkg.entry(r.pkg.clone()).or_default().push(i);
            if !r.thread.is_empty() && !pattern_has_meta(&r.thread) {
                exact_thread_rules
                    .entry(r.pkg.clone())
                    .or_default()
                    .entry(r.thread.clone())
                    .or_default()
                    .push(i);
            }
        }
        AppConfig {
            rules,
            pkgs,
            has_thread_rules,
            topo: topo(),
            rules_by_pkg,
            exact_thread_rules,
            dyn_rules: Vec::new(),
            dyn_by_pkg: HashMap::new(),
            has_dyn_rules: HashSet::new(),
            cpuset_dirs: Mutex::new(HashSet::new()),
        }
    }

    #[test]
    fn fallback_to_pkg_rule() {
        let c = cfg(vec![pkg_rule("com.a", &[0, 1])]);
        let r = thread_affinity("com.a", "RenderThread", &c).expect("应命中包级规则");
        assert_eq!(r.cpus, cpus_of(&[0, 1]));
        assert!(!r.is_thread_rule);
        assert!(r.cpuset_dir.is_empty());
    }

    #[test]
    fn thread_rule_exact_match() {
        let c = cfg(vec![thread_rule("com.a", "RenderThread", &[6])]);
        let r = thread_affinity("com.a", "RenderThread", &c).expect("应命中线程规则");
        assert_eq!(r.cpus, cpus_of(&[6]));
        assert!(r.is_thread_rule);
    }

    #[test]
    fn thread_rule_fnmatch() {
        let c = cfg(vec![thread_rule("com.a", "RenderThread*", &[6, 7])]);
        let r = thread_affinity("com.a", "RenderThread-1", &c).expect("fnmatch 应命中");
        assert_eq!(r.cpus, cpus_of(&[6, 7]));
        assert!(r.is_thread_rule);
    }

    #[test]
    fn thread_match_merges_rules() {
        let c = cfg(vec![
            thread_rule("com.a", "Render*", &[0]),
            thread_rule("com.a", "*Thread", &[1]),
        ]);
        let r = thread_affinity("com.a", "RenderThread", &c).expect("两条规则都应命中");
        assert_eq!(r.cpus, cpus_of(&[0, 1]));
        assert!(r.is_thread_rule);
    }

    #[test]
    fn multiple_pkg_rules_merge_cpus() {
        let c = cfg(vec![pkg_rule("com.a", &[0]), pkg_rule("com.a", &[1])]);
        let r = thread_affinity("com.a", "anything", &c).expect("包级 fallback 应合并");
        assert_eq!(r.cpus, cpus_of(&[0, 1]));
        assert!(!r.is_thread_rule);
    }

    #[test]
    fn thread_rules_only_unmatched_returns_present() {
        let c = cfg(vec![thread_rule("com.a", "RenderThread", &[6])]);
        let r = thread_affinity("com.a", "OtherThread", &c).expect("有线程规则的包应回退到全部核心");
        assert_eq!(r.cpus, cpus_of(&[0, 1, 2, 3, 4, 5, 6]));
        assert!(!r.is_thread_rule);
        assert!(r.cpuset_dir.is_empty());
    }

    #[test]
    fn unknown_pkg_no_rules() {
        let c = cfg(vec![thread_rule("com.a", "RenderThread", &[6])]);
        assert!(thread_affinity("com.other", "x", &c).is_none());
    }

    /// 复刻优化前的全量线性扫描实现，作为等价性参照
    fn linear_thread_affinity(pkg: &str, thread: &str, cfg: &AppConfig) -> Option<(CpuSet, bool, String)> {
        let mut cpus = CpuSet::new();
        let mut matched = false;
        if !thread.is_empty() {
            for rule in &cfg.rules {
                if rule.pkg != pkg || rule.thread.is_empty() {
                    continue;
                }
                if fnmatch_c(&rule.thread_pattern, thread) {
                    cpus.or(&rule.cpus);
                    matched = true;
                }
            }
        }
        if !matched {
            for rule in &cfg.rules {
                if rule.pkg != pkg || !rule.thread.is_empty() {
                    continue;
                }
                cpus.or(&rule.cpus);
            }
        }
        if cpus.count() == 0 {
            if cfg.has_thread_rules.contains(pkg) {
                return Some((cfg.topo.present_cpus, false, String::new()));
            }
            return None;
        }
        Some((cpus, matched, String::new()))
    }

    #[test]
    fn indexed_matches_linear_reference() {
        let c = cfg(vec![
            pkg_rule("com.a", &[0, 1]),
            thread_rule("com.a", "RenderThread", &[6]),
            thread_rule("com.a", "Worker-*", &[4, 5]),
            thread_rule("com.b", "*Thread", &[2]),
            pkg_rule("com.c", &[0]),
            pkg_rule("com.c", &[1]),
            thread_rule("com.d", "main", &[7]),
            thread_rule("com.d", "Render*", &[3]),
        ]);
        for pkg in ["com.a", "com.b", "com.c", "com.d", "com.zz"] {
            for th in ["RenderThread", "main", "Worker-1", "Worker-9", "Other", "x", ""] {
                let got = thread_affinity(pkg, th, &c);
                let want = linear_thread_affinity(pkg, th, &c);
                match (got, want) {
                    (Some(g), Some(w)) => assert_eq!(
                        (g.cpus, g.is_thread_rule, g.cpuset_dir),
                        (w.0, w.1, w.2),
                        "pkg={} th={}",
                        pkg,
                        th
                    ),
                    (None, None) => {}
                    _ => panic!("pkg={} th={} 结果不一致", pkg, th),
                }
            }
        }
    }
}
