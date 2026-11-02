use std::collections::{HashMap, HashSet};

use crate::apply_affinity::affinity_sync_batch;
use crate::config::AppConfig;
use crate::cpuset::{CpuSet, CpuTopology};
use crate::dynamic::{dyn_group_of, dyn_resolve, DynState};
use crate::rule_match::{comm_to_pkg, thread_affinity};

pub struct TaskEntry {
    pub pid: i32,
    pub pkg: String,
    pub comm: String,
    pub cpus: CpuSet,
    pub cpuset_dir: String,
    pub is_thread_rule: bool,
}
pub struct ProcCache {
    pub tasks: HashMap<i32, TaskEntry>,
    pub dyn_state: DynState,
    pub by_pid: HashMap<i32, Vec<i32>>,
    pub dyn_members: HashMap<usize, HashSet<i32>>,
    pub tid_group: HashMap<i32, usize>,
}

impl ProcCache {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::with_capacity(64),
            dyn_state: DynState::new(),
            by_pid: HashMap::new(),
            dyn_members: HashMap::new(),
            tid_group: HashMap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.tasks.clear();
        self.dyn_state.tids.clear();
        self.dyn_state.winners.clear();
        self.by_pid.clear();
        self.dyn_members.clear();
        self.tid_group.clear();
    }

    pub fn task_del(&mut self, tid: i32) {
        if let Some(e) = self.tasks.remove(&tid) {
            if let Some(tids) = self.by_pid.get_mut(&e.pid) {
                tids.retain(|&t| t != tid);
            }
            self.tid_group_remove(tid);
        }
    }
    fn tid_group_remove(&mut self, tid: i32) {
        if let Some(gi) = self.tid_group.remove(&tid)
            && let Some(set) = self.dyn_members.get_mut(&gi)
        {
            set.remove(&tid);
        }
    }
    pub fn remove_pkgs(&mut self, pids: &HashSet<i32>) {
        for pid in pids {
            if let Some(tids) = self.by_pid.remove(pid) {
                for tid in &tids {
                    self.tasks.remove(tid);
                    self.tid_group_remove(*tid);
                }
            }
        }
    }
    pub fn retain_pkg_tids(&mut self, pid: i32, live: &HashSet<i32>) {
        let Some(mut old) = self.by_pid.remove(&pid) else {
            return;
        };
        for tid in &old {
            if !live.contains(tid) {
                self.tasks.remove(tid);
                self.tid_group_remove(*tid);
            }
        }
        old.retain(|tid| live.contains(tid));
        if !old.is_empty() {
            self.by_pid.insert(pid, old);
        }
    }
    pub fn pkg_of(&self, pid: i32) -> Option<String> {
        self.by_pid
            .get(&pid)?
            .iter()
            .find_map(|tid| self.tasks.get(tid))
            .map(|e| e.pkg.clone())
    }
    pub fn pkg_lookup_comm(&self, pid: i32, comm: &str, cfg: &AppConfig) -> Option<String> {
        comm_to_pkg(pid, comm, cfg).or_else(|| self.pkg_of(pid))
    }
    pub fn task_apply<F>(
        &mut self,
        tid: i32,
        pid: i32,
        pkg: &str,
        comm: &str,
        cfg: &AppConfig,
        apply_fn: F,
    ) -> bool
    where
        F: FnOnce(i32, &CpuSet, &str) -> bool,
    {
        let thread_name = if cfg.has_thread_rules.contains(pkg) || cfg.has_dyn_rules.contains(pkg) {
            comm
        } else {
            ""
        };
        let (result, group) =
            if let Some((gi, spec)) = dyn_group_of(cfg, pkg, thread_name) {
                (Some(dyn_resolve(self, tid, spec, cfg)), Some(gi))
            } else {
                (thread_affinity(pkg, thread_name, cfg), None)
            };
        let Some(result) = result else {
            return false;
        };

        if !result.is_thread_rule && self.tasks.get(&tid).is_some_and(|old| old.is_thread_rule) {
            return true;
        }
        if self.tasks.get(&tid).is_some_and(|old| {
            old.pkg == pkg
                && old.cpus == result.cpus
                && old.cpuset_dir == result.cpuset_dir
                && old.is_thread_rule == result.is_thread_rule
        }) {
            return true;
        }

        let dead = apply_fn(tid, &result.cpus, &result.cpuset_dir);
        if dead {
            self.task_del(tid);
            return false;
        }

        let old_pid = self.tasks.get(&tid).map(|old| old.pid);
        if let Some(op) = old_pid
            && op != pid
            && let Some(tids) = self.by_pid.get_mut(&op)
        {
            tids.retain(|&t| t != tid);
        }
        self.tasks.insert(
            tid,
            TaskEntry {
                pid,
                pkg: pkg.to_string(),
                comm: comm.to_string(),
                cpus: result.cpus,
                cpuset_dir: result.cpuset_dir,
                is_thread_rule: result.is_thread_rule,
            },
        );
        match self.by_pid.get_mut(&pid) {
            Some(tids) if !tids.contains(&tid) => tids.push(tid),
            Some(_) => {}
            None => {
                self.by_pid.insert(pid, vec![tid]);
            }
        }
        match group {
            Some(gi) => {
                if self.tid_group.get(&tid) != Some(&gi) {
                    self.tid_group_remove(tid);
                    self.dyn_members.entry(gi).or_default().insert(tid);
                    self.tid_group.insert(tid, gi);
                }
            }
            None => self.tid_group_remove(tid),
        }
        true
    }
    pub fn affinity_sync(&mut self, topo: &CpuTopology) -> Vec<i32> {
        let dead_tids = affinity_sync_batch(
            self.tasks.iter().map(|(tid, e)| (*tid, &e.cpus, e.cpuset_dir.as_str())),
            topo,
        );
        for tid in &dead_tids {
            self.task_del(*tid);
        }
        dead_tids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::ffi::CString;
    use std::sync::Mutex;

    use crate::config::{pattern_has_meta, AffinityRule, DynRule, DynSpec};
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

    fn rule(pkg: &str, thread: &str, cpus: &[usize]) -> AffinityRule {
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
            cpuset_dirs: Mutex::new(HashMap::new()),
            comm_prefixes: HashSet::new(),
            comm_suffixes: HashSet::new(),
        }
    }

    fn dyn_cfg() -> AppConfig {
        let mut c = cfg(vec![rule("com.a", "", &[0, 3])]);
        let spec = DynSpec::Elect {
            winner: Box::new(cpus_of(&[6, 7])),
            others: Box::new(cpus_of(&[0, 1])),
        };
        c.dyn_rules.push(DynRule {
            pkg: "com.a".to_string(),
            thread: "Thread-*".to_string(),
            thread_pattern: CString::new("Thread-*").unwrap(),
            spec,
        });
        c.dyn_by_pkg.entry("com.a".to_string()).or_default().push(0);
        c.has_dyn_rules.insert("com.a".to_string());
        c
    }

    #[test]
    fn dyn_thread_defaults_to_low_tier() {
        let c = dyn_cfg();
        let mut cache = ProcCache::new();
        let mut captured: Option<(CpuSet, String)> = None;
        let mut apply = |_t: i32, cpus: &CpuSet, dir: &str| {
            captured = Some((*cpus, dir.to_string()));
            false
        };
        assert!(cache.task_apply(100, 100, "com.a", "Thread-9", &c, &mut apply));
        let (cpus, _) = captured.unwrap();
        assert_eq!(cpus.to_range_string(), "0-1", "首次命中动态组应落在低档");
        let mut calls = 0;
        let mut apply2 = |_t: i32, _: &CpuSet, _: &str| {
            calls += 1;
            false
        };
        assert!(cache.task_apply(100, 100, "com.a", "Thread-9", &c, &mut apply2));
        assert_eq!(calls, 0);
    }

    #[test]
    fn dyn_thread_unmatched_still_static() {
        let c = dyn_cfg();
        let mut cache = ProcCache::new();
        let mut calls = 0;
        let mut apply = |_t: i32, _: &CpuSet, _: &str| {
            calls += 1;
            false
        };
        assert!(cache.task_apply(100, 100, "com.a", "RenderThread", &c, &mut apply));
        assert_eq!(calls, 1, "非动态组线程仍由静态逻辑处理");
        assert!(!cache.tasks[&100].is_thread_rule);
    }

    #[test]
    fn repeat_apply_skipped_when_result_unchanged() {
        let c = cfg(vec![rule("com.a", "", &[0, 1])]);
        let mut cache = ProcCache::new();
        let mut calls = 0;
        let mut apply = |_t: i32, _: &CpuSet, _: &str| {
            calls += 1;
            false
        };
        assert!(cache.task_apply(100, 100, "com.a", "", &c, &mut apply));
        assert!(cache.task_apply(100, 100, "com.a", "", &c, &mut apply));
        assert_eq!(calls, 1, "结果一致时第二次不应重复分配");
    }

    #[test]
    fn repeat_apply_skipped_for_thread_rule() {
        let c = cfg(vec![rule("com.a", "RenderThread", &[6])]);
        let mut cache = ProcCache::new();
        let mut calls = 0;
        let mut apply = |_t: i32, _: &CpuSet, _: &str| {
            calls += 1;
            false
        };
        assert!(cache.task_apply(100, 100, "com.a", "RenderThread", &c, &mut apply));
        assert!(cache.task_apply(100, 100, "com.a", "RenderThread", &c, &mut apply));
        assert_eq!(calls, 1, "同一线程名重复事件不应重复分配");
    }

    #[test]
    fn result_change_triggers_reapply() {
        let c = cfg(vec![
            rule("com.a", "RenderThread", &[6]),
            rule("com.a", "Worker-*", &[4]),
        ]);
        let mut cache = ProcCache::new();
        let mut calls = 0;
        let mut apply = |_t: i32, _: &CpuSet, _: &str| {
            calls += 1;
            false
        };
        assert!(cache.task_apply(100, 100, "com.a", "RenderThread", &c, &mut apply));
        assert!(cache.task_apply(100, 100, "com.a", "Worker-1", &c, &mut apply));
        assert_eq!(calls, 2, "线程名变化导致结果不同应重新分配");
    }
}

