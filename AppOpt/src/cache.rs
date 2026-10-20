use std::collections::{HashMap, HashSet};

use crate::apply_affinity::affinity_set;
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

/// 双模式共用进程缓存，eBPF 事件驱动增量维护，proc 模式触发全量重建
pub struct ProcCache {
    pub tasks: HashMap<i32, TaskEntry>,
    /// 动态规则（按负载选举/阈值）运行时状态
    pub dyn_state: DynState,
}

impl ProcCache {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::with_capacity(64),
            dyn_state: DynState::new(),
        }
    }

    pub fn clear(&mut self) {
        self.tasks.clear();
        self.dyn_state.tids.clear();
        self.dyn_state.winners.clear();
    }

    pub fn task_del(&mut self, tid: i32) {
        self.tasks.remove(&tid);
    }

    /// 批量删除属于任一消失 pid 的全部 tid 条目（进程退出/pid 复用），避免逐 pid 遍历
    pub fn remove_pkgs(&mut self, pids: &HashSet<i32>) {
        self.tasks.retain(|_, e| !pids.contains(&e.pid));
    }

    /// 仅保留属于 pid 且仍存活于 task 目录的 tid（线程退出清理）
    pub fn retain_pkg_tids(&mut self, pid: i32, live: &HashSet<i32>) {
        self.tasks.retain(|tid, e| e.pid != pid || live.contains(tid));
    }

    /// comm 匹配包名，线程名时回退主线程条目
    pub fn pkg_lookup_comm(&self, pid: i32, comm: &str, cfg: &AppConfig) -> Option<String> {
        comm_to_pkg(pid, comm, cfg).or_else(|| self.tasks.get(&pid).map(|e| e.pkg.clone()))
    }

    /// 计算并应用线程亲和性，保护已有线程规则绑定防止降级
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
        let result = if let Some((_, spec)) = dyn_group_of(cfg, pkg, thread_name) {
            Some(dyn_resolve(self, tid, spec, cfg))
        } else {
            thread_affinity(pkg, thread_name, cfg)
        };
        let Some(result) = result else {
            return false;
        };

        if !result.is_thread_rule && self.tasks.get(&tid).is_some_and(|old| old.is_thread_rule) {
            return true;
        }

        // 结果与缓存一致时跳过重复分配，事件路径（EXEC/RENAME 等）对已绑定线程的重复应用归零
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
            self.tasks.remove(&tid);
            return false;
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
        true
    }

    /// 遍历 tasks 应用亲和性，返回 dead_tids 供 eBPF 调用方清理 APPLIED_MAP
    pub fn affinity_sync(&mut self, topo: &CpuTopology) -> Vec<i32> {
        let dead_tids: Vec<i32> = self
            .tasks
            .iter()
            .filter_map(|(tid, e)| {
                if affinity_set(*tid, &e.cpus, &e.cpuset_dir, topo) {
                    Some(*tid)
                } else {
                    None
                }
            })
            .collect();
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
            cpuset_dirs: Mutex::new(HashSet::new()),
        }
    }

    fn dyn_cfg() -> AppConfig {
        // 含包级静态规则，验证非动态组线程走静态 fallback
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

        // 重复事件不应重新分配
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


