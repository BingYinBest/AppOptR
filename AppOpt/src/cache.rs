use std::collections::{HashMap, HashSet};

use crate::apply_affinity::affinity_sync_batch;
use crate::config::AppConfig;
use crate::cpuset::{CpuSet, CpuTopology};
use crate::dynamic::{dyn_group_of, dyn_resolve, DynState};
use crate::rule_match::{comm_to_pkg, thread_affinity, AffinityResult};

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
    /// 动态组成员表，键为动态规则组的稳定标识（包名 + 线程模式派生），不随配置重载变化
    pub dyn_members: HashMap<u64, HashSet<i32>>,
    /// tid -> 所属动态组标识
    pub tid_group: HashMap<i32, u64>,
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
            self.detach_tid(e.pid, tid);
            self.tid_group_remove(tid);
        }
    }

    /// 从 pid 的线程列表摘除 tid；列表空了就整体删除该 pid，避免索引残留空壳
    fn detach_tid(&mut self, pid: i32, tid: i32) {
        let mut empty = false;
        if let Some(tids) = self.by_pid.get_mut(&pid) {
            tids.retain(|&t| t != tid);
            empty = tids.is_empty();
        }
        if empty {
            self.by_pid.remove(&pid);
        }
    }

    fn tid_group_remove(&mut self, tid: i32) {
        if let Some(gid) = self.tid_group.remove(&tid)
            && let Some(set) = self.dyn_members.get_mut(&gid)
        {
            set.remove(&tid);
            if set.is_empty() {
                self.dyn_members.remove(&gid);
            }
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

    /// 线程名只在配置了线程级/动态规则时才需要读取，普通包级规则直接忽略
    fn thread_name_for<'a>(&self, pkg: &str, comm: &'a str, cfg: &AppConfig) -> &'a str {
        if cfg.has_thread_rules.contains(pkg) || cfg.has_dyn_rules.contains(pkg) {
            comm
        } else {
            ""
        }
    }

    /// 结果是否需要真正下发：线程级绑定不被包级规则降级，结果未变化则短路
    fn needs_update(&self, tid: i32, pid: i32, pkg: &str, result: &AffinityResult) -> bool {
        let Some(old) = self.tasks.get(&tid) else {
            return true;
        };
        if !result.is_thread_rule && old.is_thread_rule {
            return false;
        }
        // pid 变化说明 tid 被复用，必须重建索引，否则任务会挂在已退出的进程上
        old.pid != pid
            || old.pkg != pkg
            || old.cpus != result.cpus
            || old.cpuset_dir != result.cpuset_dir
            || old.is_thread_rule != result.is_thread_rule
    }

    fn commit(&mut self, tid: i32, pid: i32, pkg: &str, comm: &str, result: AffinityResult) {
        if let Some(old_pid) = self.tasks.get(&tid).map(|old| old.pid)
            && old_pid != pid
        {
            self.detach_tid(old_pid, tid);
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
    }

    /// 同步线程所属的动态组。
    /// 配置重载后线程可能改匹配到别的动态组，即使亲和性结果未变也必须重新归属。
    fn sync_group(&mut self, tid: i32, group: Option<u64>) {
        match group {
            Some(gid) => {
                if self.tid_group.get(&tid) != Some(&gid) {
                    self.tid_group_remove(tid);
                    self.dyn_members.entry(gid).or_default().insert(tid);
                    self.tid_group.insert(tid, gid);
                }
            }
            None => self.tid_group_remove(tid),
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
        let thread_name = self.thread_name_for(pkg, comm, cfg);
        let (result, group) = match dyn_group_of(cfg, pkg, thread_name) {
            Some((gid, spec)) => (Some(dyn_resolve(self, tid, spec, cfg)), Some(gid)),
            None => (thread_affinity(pkg, thread_name, cfg), None),
        };
        let Some(result) = result else {
            return false;
        };

        if self.needs_update(tid, pid, pkg, &result) {
            if apply_fn(tid, &result.cpus, &result.cpuset_dir) {
                self.task_del(tid);
                return false;
            }
            self.commit(tid, pid, pkg, comm, result);
        }
        self.sync_group(tid, group);
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

    use crate::config::{dyn_group_id, pattern_has_meta, AffinityRule, DynRule, DynSpec};
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
            is_pattern: pattern_has_meta(thread),
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
            group_id: dyn_group_id("com.a", "Thread-*"),
        });
        c.dyn_by_pkg.entry("com.a".to_string()).or_default().push(0);
        c.has_dyn_rules.insert("com.a".to_string());
        c
    }

    #[test]
    fn task_del_clears_empty_pid_index() {
        let c = cfg(vec![rule("com.a", "", &[0, 1])]);
        let mut cache = ProcCache::new();
        let apply = |_t: i32, _: &CpuSet, _: &str| false;
        assert!(cache.task_apply(100, 100, "com.a", "", &c, apply));
        assert!(cache.by_pid.contains_key(&100));
        cache.task_del(100);
        assert!(
            !cache.by_pid.contains_key(&100),
            "线程移除后不应残留空 pid 索引"
        );
    }

    #[test]
    fn task_apply_reindexes_when_pid_changes() {
        let c = cfg(vec![rule("com.a", "", &[0, 1])]);
        let mut cache = ProcCache::new();
        let apply = |_t: i32, _: &CpuSet, _: &str| false;
        assert!(cache.task_apply(100, 100, "com.a", "", &c, apply));
        assert!(cache.task_apply(100, 200, "com.a", "", &c, apply));
        assert_eq!(cache.by_pid.get(&100), None);
        assert_eq!(cache.by_pid.get(&200).map(Vec::len), Some(1));
    }

    #[test]
    fn dyn_group_membership_follows_config_change() {
        let first = dyn_cfg();
        let mut cache = ProcCache::new();
        let apply = |_t: i32, _: &CpuSet, _: &str| false;
        assert!(cache.task_apply(100, 100, "com.a", "Thread-9", &first, apply));
        let gid_old = first.dyn_rules[0].group_id;
        assert!(cache.dyn_members.get(&gid_old).is_some_and(|s| s.contains(&100)));

        // 配置重载后同一线程改匹配到另一个动态组，归属必须跟随迁移
        let mut second = dyn_cfg();
        second.dyn_rules[0].thread = "Worker-*".to_string();
        second.dyn_rules[0].thread_pattern = CString::new("Worker-*").unwrap();
        second.dyn_rules[0].group_id = dyn_group_id("com.a", "Worker-*");
        let gid_new = second.dyn_rules[0].group_id;
        assert!(cache.task_apply(100, 100, "com.a", "Worker-1", &second, apply));
        assert!(cache.dyn_members.get(&gid_new).is_some_and(|s| s.contains(&100)));
        assert!(!cache.dyn_members.get(&gid_old).is_some_and(|s| s.contains(&100)));
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

