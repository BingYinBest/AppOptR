use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::{lock_ignore_poison, CONFIG_UPDATED, MAX_PKG_LEN, MAX_THREAD_LEN};
use crate::platform::cpuset::{parse_cpu_spec, CpuSet, CpuTopology};

mod parse;
mod reload;
pub use parse::*;
pub use reload::*;

pub static INOTIFY_SUPPORTED: AtomicBool = AtomicBool::new(false);
pub static INOTIFY_FD: AtomicI32 = AtomicI32::new(-1);
pub static INOTIFY_WD: AtomicI32 = AtomicI32::new(-1);
pub static CHECK_INTERVAL: AtomicU64 = AtomicU64::new(2);
pub static FORCE_RELOAD: AtomicBool = AtomicBool::new(false);
pub static LAST_MTIME: AtomicI64 = AtomicI64::new(-1);
pub static CONFIG_FILE: Mutex<String> = Mutex::new(String::new());

pub struct AffinityRule {
    pub pkg: String,
    pub thread: String,
    pub thread_pattern: CString,
    pub is_pattern: bool,
    pub cpus: CpuSet,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DynSpec {
    Elect {
        winner: Box<CpuSet>,
        others: Box<CpuSet>,
    },
    Load {
        high: Box<CpuSet>,
        low: Box<CpuSet>,
        threshold: u32,
    },
}

impl DynSpec {
    pub fn tier_cpus(&self, tier: u8) -> CpuSet {
        match self {
            DynSpec::Elect { winner, others } => {
                if tier == 0 { **winner } else { **others }
            }
            DynSpec::Load { high, low, .. } => {
                if tier == 0 { **high } else { **low }
            }
        }
    }
}

pub struct DynRule {
    pub pkg: String,
    pub thread: String,
    pub thread_pattern: CString,
    pub spec: DynSpec,
    pub group_id: u64,
}

pub fn dyn_group_id(pkg: &str, thread: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in pkg.as_bytes().iter().chain([0u8].iter()).chain(thread.as_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}
#[derive(Debug, PartialEq)]
pub enum ParsedRule {
    Static { pkg: String, thread: String, set: CpuSet },
    Dyn { pkg: String, thread: String, spec: DynSpec },
}

pub type RuleIndex = HashMap<String, Vec<usize>>;
pub type ExactThreadIndex = HashMap<String, HashMap<String, Vec<usize>>>;

pub struct AppConfig {
    pub rules: Vec<AffinityRule>,
    pub pkgs: HashSet<String>,
    pub has_thread_rules: HashSet<String>,
    pub topo: CpuTopology,
    pub rules_by_pkg: RuleIndex,
    pub exact_thread_rules: ExactThreadIndex,
    pub dyn_rules: Vec<DynRule>,
    pub dyn_by_pkg: RuleIndex,
    pub has_dyn_rules: HashSet<String>,
    pub cpuset_dirs: Mutex<HashMap<CpuSet, String>>,
    pub comm_prefixes: HashSet<String>,
    pub comm_suffixes: HashSet<String>,
}

pub static CURRENT_CONFIG: Mutex<Option<Arc<AppConfig>>> = Mutex::new(None);

pub static PARSE_FAILS: AtomicUsize = AtomicUsize::new(0);
fn parse_rule_entry(
    pkg: &str,
    thread: &str,
    cpus: &str,
    topo: &CpuTopology,
) -> Option<ParsedRule> {
    if pkg.is_empty()
        || pkg.len() >= MAX_PKG_LEN
        || thread.len() >= MAX_THREAD_LEN
        || pkg.bytes().chain(thread.bytes()).any(|b| b < 0x20 || b == 0x7f)
    {
        return None;
    }
    if let Some(spec) = dyn_spec_try(cpus, topo) {
        return Some(ParsedRule::Dyn {
            pkg: pkg.to_string(),
            thread: thread.to_string(),
            spec,
        });
    }
    let set = parse_cpu_spec(cpus, topo);
    if !spec_like(cpus) || set.count() == 0 {
        return None;
    }
    Some(ParsedRule::Static {
        pkg: pkg.to_string(),
        thread: thread.to_string(),
        set,
    })
}
struct RuleParser<'a> {
    topo: &'a CpuTopology,
    rules: Vec<ParsedRule>,
    fails: usize,
    cur_pkg: String,
    pending_pkg: String,
    in_block: bool,
}

impl<'a> RuleParser<'a> {
    fn new(topo: &'a CpuTopology) -> Self {
        Self {
            topo,
            rules: Vec::new(),
            fails: 0,
            cur_pkg: String::new(),
            pending_pkg: String::new(),
            in_block: false,
        }
    }

    fn add(&mut self, pkg: &str, thread: &str, cpus: &str) {
        match parse_rule_entry(pkg, thread, cpus, self.topo) {
            Some(rule) => self.rules.push(rule),
            None => self.fails += 1,
        }
    }

    fn parse_in_block(&mut self, line: &str) {
        if close_like(line) {
            self.in_block = false;
            self.cur_pkg.clear();
            return;
        }
        match split_rule_line(line) {
            Some((thread, cpus, closed)) => {
                let pkg = self.cur_pkg.clone();
                self.add(&pkg, thread, cpus);
                if closed {
                    self.in_block = false;
                    self.cur_pkg.clear();
                }
            }
            None => {
                self.fails += 1;
                if line.contains('}') {
                    self.in_block = false;
                    self.cur_pkg.clear();
                }
            }
        }
    }

    fn parse_outer_line(&mut self, line: &str) {
        match parse_outer(line) {
            OuterLine::Single {
                pkg,
                thread,
                cpus,
                open,
            } => {
                if !self.pending_pkg.is_empty() {
                    self.fails += 1;
                }
                self.pending_pkg.clear();
                self.add(pkg, thread, cpus);
                if open {
                    self.cur_pkg = pkg.to_string();
                    self.in_block = true;
                }
            }
            OuterLine::Rule { pkg, cpus, open } => {
                if !self.pending_pkg.is_empty() {
                    self.fails += 1;
                }
                self.add(pkg, "", cpus);
                if open {
                    self.cur_pkg = pkg.to_string();
                    self.in_block = true;
                }
                self.pending_pkg.clear();
            }
            OuterLine::BareOpen { pkg } => {
                let owner = if !pkg.is_empty() {
                    if !self.pending_pkg.is_empty() {
                        self.fails += 1;
                    }
                    pkg.to_string()
                } else {
                    self.pending_pkg.clone()
                };
                if owner.is_empty() {
                    self.fails += 1;
                    return;
                }
                self.cur_pkg = owner;
                self.pending_pkg.clear();
                self.in_block = true;
            }
            OuterLine::Pending { pkg } => {
                if !self.pending_pkg.is_empty() {
                    self.fails += 1;
                }
                self.pending_pkg = pkg.to_string();
            }
            OuterLine::Junk => {
                self.fails += 1;
                self.pending_pkg.clear();
            }
        }
    }
}

fn parse_rules(content: &str, topo: &CpuTopology) -> (Vec<ParsedRule>, usize) {
    let mut parser = RuleParser::new(topo);
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        if parser.in_block {
            parser.parse_in_block(line);
        } else {
            parser.parse_outer_line(line);
        }
    }

    if parser.in_block || !parser.pending_pkg.is_empty() {
        parser.fails += 1;
    }

    (parser.rules, parser.fails)
}
fn split_rules(parsed: Vec<ParsedRule>) -> (Vec<AffinityRule>, Vec<DynRule>) {
    let mut rules = Vec::new();
    let mut dyn_rules = Vec::new();
    for rule in parsed {
        match rule {
            ParsedRule::Static { pkg, thread, set } => {
                let is_pattern = pattern_has_meta(&thread);
                rules.push(AffinityRule {
                    pkg,
                    thread_pattern: CString::new(thread.as_str()).unwrap_or_default(),
                    is_pattern,
                    cpus: set,
                    thread,
                });
            }
            ParsedRule::Dyn { pkg, thread, spec } => {
                let group_id = dyn_group_id(&pkg, &thread);
                dyn_rules.push(DynRule {
                    thread_pattern: CString::new(thread.as_str()).unwrap_or_default(),
                    pkg,
                    thread,
                    spec,
                    group_id,
                });
            }
        }
    }
    (rules, dyn_rules)
}

fn build_rules_index(rules: &[AffinityRule]) -> (RuleIndex, ExactThreadIndex) {
    let mut rules_by_pkg: RuleIndex = HashMap::new();
    let mut exact_thread_rules: ExactThreadIndex = HashMap::new();
    for (i, rule) in rules.iter().enumerate() {
        rules_by_pkg
            .entry(rule.pkg.clone())
            .or_default()
            .push(i);
        if !rule.thread.is_empty() && !rule.is_pattern {
            exact_thread_rules
                .entry(rule.pkg.clone())
                .or_default()
                .entry(rule.thread.clone())
                .or_default()
                .push(i);
        }
    }
    (rules_by_pkg, exact_thread_rules)
}

fn build_comm_keys(pkgs: &HashSet<String>) -> (HashSet<String>, HashSet<String>) {
    let mut prefixes = HashSet::new();
    let mut suffixes = HashSet::new();
    for pkg in pkgs {
        if pkg.len() <= 15 {
            continue;
        }
        if let Some(prefix) = pkg.get(..15) {
            prefixes.insert(prefix.to_string());
        }
        if let Some(suffix) = pkg.get(pkg.len() - 15..) {
            suffixes.insert(suffix.to_string());
        }
    }
    (prefixes, suffixes)
}

fn build_config(
    parsed: Vec<ParsedRule>,
    fail_cnt: usize,
    topo: &CpuTopology,
) -> AppConfig {
    let (rules, dyn_rules) = split_rules(parsed);

    let mut pkgs: HashSet<String> = rules.iter().map(|r| r.pkg.clone()).collect();
    pkgs.extend(dyn_rules.iter().map(|r| r.pkg.clone()));
    let has_thread_rules: HashSet<String> = rules
        .iter()
        .filter(|r| !r.thread.is_empty())
        .map(|r| r.pkg.clone())
        .collect();
    let has_dyn_rules: HashSet<String> = dyn_rules.iter().map(|r| r.pkg.clone()).collect();
    let (comm_prefixes, comm_suffixes) = build_comm_keys(&pkgs);
    let (rules_by_pkg, exact_thread_rules) = build_rules_index(&rules);
    let mut dyn_by_pkg: RuleIndex = HashMap::new();
    for (i, rule) in dyn_rules.iter().enumerate() {
        dyn_by_pkg.entry(rule.pkg.clone()).or_default().push(i);
    }

    PARSE_FAILS.store(fail_cnt, Ordering::Relaxed);
    println!(
        "配置文件解析完成，共加载 {} 条规则",
        rules.len() + dyn_rules.len()
    );
    if fail_cnt > 0 {
        eprintln!("警告: {} 条规则因格式无效被跳过", fail_cnt);
    }

    AppConfig {
        rules,
        pkgs,
        has_thread_rules,
        topo: topo.clone(),
        rules_by_pkg,
        exact_thread_rules,
        dyn_rules,
        dyn_by_pkg,
        has_dyn_rules,
        cpuset_dirs: Mutex::new(HashMap::new()),
        comm_prefixes,
        comm_suffixes,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use super::*;

    fn sr(pkg: &str, thread: &str, cpus: &str) -> ParsedRule {
        ParsedRule::Static {
            pkg: pkg.to_string(),
            thread: thread.to_string(),
            set: parse_cpu_spec(cpus, &test_topo()),
        }
    }

    #[test]
    fn dyn_group_id_is_stable_and_distinct() {
        let first = dyn_group_id("com.a", "Thread-*");
        assert_eq!(first, dyn_group_id("com.a", "Thread-*"));
        assert_ne!(first, dyn_group_id("com.a", "Worker-*"));
        assert_ne!(first, dyn_group_id("com.b", "Thread-*"));
    }

    #[test]
    fn forced_reload_ignores_mtime() {
        let path = std::env::temp_dir()
            .join(format!("appopt_cfg_{}.conf", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = fs::remove_file(&path);
        fs::write(&path, "com.a=0-1\n").unwrap();
        let topo = test_topo();

        assert!(load_config(&path, &topo).is_some(), "首次加载应成功");
        assert!(load_config(&path, &topo).is_none(), "mtime 未变应短路");
        assert!(
            load_config_forced(&path, &topo).is_some(),
            "强制重载不应受 mtime 短路影响"
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn spec_like_accepts() {
        assert!(spec_like("e-core"));
        assert!(spec_like("p-core,hp-core"));
        assert!(spec_like("m-core,p-core"));
        assert!(spec_like("all-core"));
        assert!(spec_like("0-3"));
        assert!(spec_like("0-1,5-6"));
        assert!(spec_like("3"));
    }

    #[test]
    fn spec_like_rejects() {
        assert!(!spec_like(""));
        assert!(!spec_like(","));
        assert!(!spec_like("0-"));
        assert!(!spec_like("-3"));
        assert!(!spec_like("abc"));
        assert!(!spec_like("0-3,e-core,e"));
    }

    #[test]
    fn comment_requires_preceding_whitespace() {
        assert_eq!(comment_at("abc # def"), Some(3));
        assert_eq!(comment_at("abc // def"), Some(3));
        assert_eq!(comment_at("abc#def"), None);
        assert_eq!(comment_at("abc//def"), None);
        assert_eq!(comment_at("a\t#b"), Some(1));
        assert_eq!(comment_at("no comment"), None);
    }

    #[test]
    fn strip_comment_trims() {
        assert_eq!(strip_comment("com.a=0-3  # 注释"), "com.a=0-3");
        assert_eq!(strip_comment("com.a=0-3 // 注释"), "com.a=0-3");
        assert_eq!(strip_comment("com.a=0-3"), "com.a=0-3");
    }

    #[test]
    fn split_rule_line_kv() {
        assert_eq!(split_rule_line("RenderThread=6-7"), Some(("RenderThread", "6-7", false)));
        assert_eq!(split_rule_line("RenderThread=6-7}"), Some(("RenderThread", "6-7", true)));
        assert_eq!(split_rule_line(" =x"), None);
        assert_eq!(split_rule_line("x="), Some(("x", "", false)));
    }

    #[test]
    fn close_like_variants() {
        assert!(close_like("}"));
        assert!(close_like("} "));
        assert!(close_like("} # comment"));
        assert!(!close_like("}x"));
        assert!(!close_like("abc}"));
    }

    #[test]
    fn split_single_line_forms() {
        assert_eq!(
            split_single_line("com.example{Render}=6-7}"),
            Some(("com.example", "Render", "6-7}"))
        );
        assert_eq!(split_single_line("com.example{RenderThread=6-7}"), None);
        assert_eq!(split_single_line("com.example=0-3"), None);
        assert_eq!(split_single_line("com.example{RenderThread=6-7"), None);
    }

    #[test]
    fn parse_outer_rule() {
        assert!(matches!(
            parse_outer("com.example=0-3"),
            OuterLine::Rule { pkg: "com.example", cpus: "0-3", open: false }
        ));
        assert!(matches!(
            parse_outer("com.example=0-3 {"),
            OuterLine::Rule { pkg: "com.example", cpus: "0-3", open: true }
        ));
    }

    #[test]
    fn parse_outer_bare_open() {
        assert!(matches!(
            parse_outer("com.example {"),
            OuterLine::BareOpen { pkg: "com.example" }
        ));
    }

    #[test]
    fn parse_outer_pending() {
        assert!(matches!(
            parse_outer("com.example"),
            OuterLine::Pending { pkg: "com.example" }
        ));
    }

    #[test]
    fn parse_outer_single_line() {
        assert!(matches!(
            parse_outer("com.example{Render}=6-7}"),
            OuterLine::Single { pkg: "com.example", thread: "Render", cpus: "6-7}", open: false }
        ));
        assert!(matches!(
            parse_outer("com.example{RenderThread=6-7}"),
            OuterLine::Rule { pkg: "com.example{RenderThread", cpus: "6-7}", open: false }
        ));
    }

    #[test]
    fn parse_outer_junk() {
        assert!(matches!(parse_outer("}"), OuterLine::Junk));
        assert!(matches!(parse_outer("} # x"), OuterLine::Junk));
        assert!(matches!(parse_outer("} // 注释"), OuterLine::Junk));
    }

    fn test_topo() -> CpuTopology {
        let mut present = CpuSet::new();
        for c in 0..8 {
            present.set(c);
        }
        let mut e = CpuSet::new();
        e.set(0);
        e.set(1);
        let mut p = CpuSet::new();
        p.set(2);
        p.set(3);
        let mut hp = CpuSet::new();
        hp.set(4);
        hp.set(5);
        hp.set(6);
        hp.set(7);
        CpuTopology {
            present_cpus: present,
            present_str: "0-7".to_string(),
            mems_str: "0".to_string(),
            cpuset_enabled: false,
            e_core: e,
            m_core: CpuSet::new(),
            p_core: p,
            hp_core: hp,
        }
    }

    #[test]
    fn parse_rules_basic_kv() {
        let (rules, fails) = parse_rules("com.a=0-3\ncom.b=hp-core\n", &test_topo());
        assert_eq!(fails, 0);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], sr("com.a", "", "0-3"));
        assert_eq!(rules[1], sr("com.b", "", "hp-core"));
    }

    #[test]
    fn parse_rules_block_and_threads() {
        let content = "\ncom.a=e-core,p-core {\n    RenderThread=hp-core\n    Thread-1=0-5\n}\ncom.b {\n    RenderThread=2\n}\n";
        let (rules, fails) = parse_rules(content, &test_topo());
        assert_eq!(fails, 0);
        assert_eq!(rules.len(), 4);
        assert_eq!(rules[0], sr("com.a", "", "e-core,p-core"));
        assert_eq!(rules[1], sr("com.a", "RenderThread", "hp-core"));
        assert_eq!(rules[2], sr("com.a", "Thread-1", "0-5"));
        assert_eq!(rules[3], sr("com.b", "RenderThread", "2"));
    }

    #[test]
    fn parse_rules_pending_block() {
        let content = "com.a {\n    Render=6-7\n}\ncom.b\n";
        let (rules, fails) = parse_rules(content, &test_topo());
        assert_eq!(fails, 1);
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn parse_rules_comments_ignored() {
        let content = "# 注释\n// 注释\n\ncom.a=0-1\n";
        let (rules, fails) = parse_rules(content, &test_topo());
        assert_eq!(fails, 0);
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn parse_rules_invalid_skipped() {
        let content = "com.a=x\ncom.b=0-1\ncom.c=\n=5\nx{y=z}\n";
        let (rules, fails) = parse_rules(content, &test_topo());
        assert_eq!(rules.len(), 1);
        assert!(matches!(rules[0], ParsedRule::Static { .. }));
        assert!(fails >= 3);
    }

    #[test]
    fn parse_rules_unclosed_block() {
        let (rules, fails) = parse_rules("com.a {\n    Render=2\n", &test_topo());
        assert_eq!(rules.len(), 1);
        assert_eq!(fails, 1);
    }

    #[test]
    fn parse_rules_open_rule_block() {
        let content = "com.a=0-3 {\n    Render=4\n}\n";
        let (rules, fails) = parse_rules(content, &test_topo());
        assert_eq!(fails, 0);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], sr("com.a", "", "0-3"));
        assert_eq!(rules[1], sr("com.a", "Render", "4"));
    }

    #[test]
    fn parse_rules_thread_too_long_fails() {
        let long = "x".repeat(40);
        let content = format!("com.a={}\ncom.b=0-1\n", long);
        let (rules, fails) = parse_rules(&content, &test_topo());
        assert_eq!(rules.len(), 1);
        assert_eq!(fails, 1);
    }

    #[test]
    fn parse_rules_real_sample() {
        let content = include_str!("../../../module/applist.conf");
        let (rules, fails) = parse_rules(content, &test_topo());
        assert_eq!(fails, 0);
        assert!(!rules.is_empty());
        assert!(rules.iter().any(|r| matches!(
            r,
            ParsedRule::Static { pkg, thread, .. } if pkg == "com.tencent.mm" && thread.is_empty()
        )));
        assert!(rules.iter().any(|r| matches!(
            r,
            ParsedRule::Static { pkg, thread, .. } if pkg == "com.tencent.mm" && thread == "RenderThread"
        )));
        assert!(rules.iter().any(|r| matches!(
            r,
            ParsedRule::Static { pkg, thread, .. } if pkg == "com.tencent.mm:push" && thread.is_empty()
        )));
        assert!(rules.iter().any(|r| matches!(
            r,
            ParsedRule::Static { pkg, thread, .. } if pkg == "com.coolapk.market" && thread == "RenderThread"
        )));
    }

    #[test]
    fn parse_rules_dyn_elect() {
        let content = "com.a {\n    Thread-*=elect(6-7 | 0-5)\n}\n";
        let (rules, fails) = parse_rules(content, &test_topo());
        assert_eq!(fails, 0);
        assert_eq!(rules.len(), 1);
        match &rules[0] {
            ParsedRule::Dyn { pkg, thread, spec } => {
                assert_eq!(pkg, "com.a");
                assert_eq!(thread, "Thread-*");
                match spec {
                    DynSpec::Elect { winner, others } => {
                        assert_eq!(winner.to_range_string(), "6-7");
                        assert_eq!(others.to_range_string(), "0-5");
                    }
                    _ => panic!("应为 Elect 规格"),
                }
            }
            _ => panic!("应为动态规则"),
        }
    }

    #[test]
    fn parse_rules_dyn_load() {
        let content = "com.a {\n    Thread-*=load(7 | 4-6 | 50)\n}\n";
        let (rules, fails) = parse_rules(content, &test_topo());
        assert_eq!(fails, 0);
        assert_eq!(rules.len(), 1);
        match &rules[0] {
            ParsedRule::Dyn { pkg, thread, spec } => {
                assert_eq!(pkg, "com.a");
                assert_eq!(thread, "Thread-*");
                match spec {
                    DynSpec::Load { high, low, threshold } => {
                        assert_eq!(high.to_range_string(), "7");
                        assert_eq!(low.to_range_string(), "4-6");
                        assert_eq!(*threshold, 50);
                    }
                    _ => panic!("应为 Load 规格"),
                }
            }
            _ => panic!("应为动态规则"),
        }
    }

    #[test]
    fn parse_rules_dyn_invalid() {
        let content = "com.a {\n    T=elect(6-7)\n    U=load(7|4-6|0)\n    V=elect(6-7|0-5\n    W=load(7|4-6|101)\n}\n";
        let (rules, fails) = parse_rules(content, &test_topo());
        assert_eq!(rules.len(), 0);
        assert!(fails >= 4);
    }
}

