use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use crate::{lock_ignore_poison, CONFIG_UPDATED, MAX_PKG_LEN, MAX_THREAD_LEN};
use crate::cpuset::{parse_cpu_spec, CpuSet, CpuTopology};

pub static INOTIFY_SUPPORTED: AtomicBool = AtomicBool::new(false);
pub static INOTIFY_FD: AtomicI32 = AtomicI32::new(-1);
pub static INOTIFY_WD: AtomicI32 = AtomicI32::new(-1);
pub static CHECK_INTERVAL: AtomicU64 = AtomicU64::new(2);
pub static FORCE_RELOAD: AtomicBool = AtomicBool::new(false);
pub static CONFIG_FILE: Mutex<String> = Mutex::new(String::new());

pub struct AffinityRule {
    pub pkg: String,
    pub thread: String,
    pub thread_pattern: CString,
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
}
#[derive(Debug, PartialEq)]
pub enum ParsedRule {
    Static { pkg: String, thread: String, cpus: String },
    Dyn { pkg: String, thread: String, spec: DynSpec },
}

pub struct AppConfig {
    pub rules: Vec<AffinityRule>,
    pub pkgs: HashSet<String>,
    pub has_thread_rules: HashSet<String>,
    pub topo: CpuTopology,
    pub rules_by_pkg: HashMap<String, Vec<usize>>,
    pub exact_thread_rules: HashMap<String, HashMap<String, Vec<usize>>>,
    pub dyn_rules: Vec<DynRule>,
    pub dyn_by_pkg: HashMap<String, Vec<usize>>,
    pub has_dyn_rules: HashSet<String>,
    pub cpuset_dirs: Mutex<HashSet<String>>,
}

pub static CURRENT_CONFIG: Mutex<Option<Arc<AppConfig>>> = Mutex::new(None);

pub static PARSE_FAILS: AtomicUsize = AtomicUsize::new(0);
pub fn spec_like(s: &str) -> bool {
    let mut any = false;
    for part in s.split(',') {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        any = true;
        let ok = matches!(t, "e-core" | "m-core" | "p-core" | "hp-core" | "all-core")
            || t.bytes().all(|b| b.is_ascii_digit())
            || t.split_once('-').is_some_and(|(a, b)| {
                !a.is_empty()
                    && !b.is_empty()
                    && a.bytes().all(|b| b.is_ascii_digit())
                    && b.bytes().all(|b| b.is_ascii_digit())
            });
        if !ok {
            return false;
        }
    }
    any
}
pub fn pattern_has_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

pub fn comment_at(s: &str) -> Option<usize> {
    let mut prev_ws = false;
    for (i, c) in s.char_indices() {
        if prev_ws && (c == '#' || (c == '/' && s[i..].starts_with("//"))) {
            return Some(s[..i].trim_end().len());
        }
        prev_ws = c.is_whitespace();
    }
    None
}

pub fn strip_comment(s: &str) -> &str {
    &s[..comment_at(s).unwrap_or(s.len())]
}

pub fn split_rule_line(p: &str) -> Option<(&str, &str, bool)> {
    let p = strip_comment(p);
    fn kv(s: &str) -> Option<(&str, &str)> {
        s.rfind('=')
            .map(|eq| (s[..eq].trim(), s[eq + 1..].trim()))
            .filter(|(k, _)| !k.is_empty())
    }
    p.match_indices('}')
        .find_map(|(cb, _)| kv(&p[..cb]).map(|(k, v)| (k, v, true)))
        .or_else(|| kv(p).map(|(k, v)| (k, v, false)))
}

pub fn close_like(p: &str) -> bool {
    p.strip_prefix('}').is_some_and(|r| {
        r.is_empty() || r.starts_with(char::is_whitespace) || r.starts_with('#') || r.starts_with("//")
    })
}

pub fn split_single_line(body: &str) -> Option<(&str, &str, &str)> {
    let eq = body.rfind('=')?;
    let cpus = body[eq + 1..].trim();
    let left = body[..eq].trim_end().strip_suffix('}')?.trim_end();
    let ob = left.find('{')?;
    let (pkg, thread) = (left[..ob].trim(), left[ob + 1..].trim());
    (!pkg.is_empty() && !thread.is_empty()).then_some((pkg, thread, cpus))
}

pub enum OuterLine<'a> {
    Rule { pkg: &'a str, cpus: &'a str, open: bool },
    BareOpen { pkg: &'a str },
    Pending { pkg: &'a str },
    Single { pkg: &'a str, thread: &'a str, cpus: &'a str, open: bool },
    Junk,
}

pub fn parse_outer(p: &str) -> OuterLine<'_> {
    let p = strip_comment(p);
    let (open, body) = match p.strip_suffix('{') {
        Some(b) => (true, b.trim_end()),
        None => (false, p),
    };
    if let Some((pkg, thread, cpus)) = split_single_line(body) {
        return OuterLine::Single { pkg, thread, cpus, open };
    }
    if !open && close_like(body) {
        return OuterLine::Junk;
    }
    match body.rfind('=') {
        Some(eq) => {
            let (pkg, cpus) = (body[..eq].trim(), body[eq + 1..].trim());
            if cpus.is_empty() {
                return if open {
                    OuterLine::BareOpen { pkg }
                } else {
                    OuterLine::Pending { pkg }
                };
            }
            OuterLine::Rule { pkg, cpus, open }
        }
        None => {
            if open {
                OuterLine::BareOpen { pkg: body }
            } else {
                OuterLine::Pending { pkg: body }
            }
        }
    }
}

fn add_rule(rules: &mut Vec<AffinityRule>, topo: &CpuTopology, pkg: &str, thread: &str, cpus_spec: &str) {
    let set = parse_cpu_spec(cpus_spec, topo);
    rules.push(AffinityRule {
        pkg: pkg.to_string(),
        thread: thread.to_string(),
        thread_pattern: CString::new(thread).unwrap_or_default(),
        cpus: set,
    });
}
pub fn dyn_spec_try(cpus: &str, topo: &CpuTopology) -> Option<DynSpec> {
    let s = cpus.trim();
    if let Some(inner) = s.strip_prefix("elect(").and_then(|r| r.strip_suffix(')')) {
        let mut parts = inner.split('|').map(str::trim).filter(|p| !p.is_empty());
        let a = parts.next()?;
        let b = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let winner = parse_cpu_spec(a, topo);
        let others = parse_cpu_spec(b, topo);
        if winner.count() == 0 || others.count() == 0 {
            return None;
        }
        return Some(DynSpec::Elect {
            winner: Box::new(winner),
            others: Box::new(others),
        });
    }
    if let Some(inner) = s.strip_prefix("load(").and_then(|r| r.strip_suffix(')')) {
        let mut parts = inner.split('|').map(str::trim).filter(|p| !p.is_empty());
        let a = parts.next()?;
        let b = parts.next()?;
        let t = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let threshold: u32 = t.parse().ok().filter(|n| (1..=100).contains(n))?;
        let high = parse_cpu_spec(a, topo);
        let low = parse_cpu_spec(b, topo);
        if high.count() == 0 || low.count() == 0 {
            return None;
        }
        return Some(DynSpec::Load {
            high: Box::new(high),
            low: Box::new(low),
            threshold,
        });
    }
    None
}
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
    if !spec_like(cpus) || parse_cpu_spec(cpus, topo).count() == 0 {
        return None;
    }
    Some(ParsedRule::Static {
        pkg: pkg.to_string(),
        thread: thread.to_string(),
        cpus: cpus.to_string(),
    })
}
fn parse_rules(content: &str, topo: &CpuTopology) -> (Vec<ParsedRule>, usize) {
    let mut rules: Vec<ParsedRule> = Vec::new();
    let mut fail_cnt: usize = 0;
    let mut cur_pkg = String::new();
    let mut pending_pkg = String::new();
    let mut in_block = false;

    for line in content.lines() {
        let p = line.trim();
        if p.is_empty() || p.starts_with('#') || p.starts_with("//") {
            continue;
        }

        if in_block {
            if close_like(p) {
                in_block = false;
                cur_pkg.clear();
                continue;
            }
            match split_rule_line(p) {
                Some((thread, cpus, closed)) => {
                    match parse_rule_entry(&cur_pkg, thread, cpus, topo) {
                        Some(r) => rules.push(r),
                        None => fail_cnt += 1,
                    }
                    if closed {
                        in_block = false;
                        cur_pkg.clear();
                    }
                }
                None => {
                    fail_cnt += 1;
                    if p.contains('}') {
                        in_block = false;
                        cur_pkg.clear();
                    }
                }
            }
            continue;
        }

        match parse_outer(p) {
            OuterLine::Single { pkg, thread, cpus, open } => {
                if !pending_pkg.is_empty() {
                    fail_cnt += 1;
                }
                pending_pkg.clear();
                match parse_rule_entry(pkg, thread, cpus, topo) {
                    Some(r) => rules.push(r),
                    None => fail_cnt += 1,
                }
                if open {
                    cur_pkg = pkg.to_string();
                    in_block = true;
                }
            }
            OuterLine::Rule { pkg, cpus, open } => {
                if !pending_pkg.is_empty() {
                    fail_cnt += 1;
                }
                match parse_rule_entry(pkg, "", cpus, topo) {
                    Some(r) => rules.push(r),
                    None => fail_cnt += 1,
                }
                if open {
                    cur_pkg = pkg.to_string();
                    in_block = true;
                }
                pending_pkg.clear();
            }
            OuterLine::BareOpen { pkg } => {
                let owner = if !pkg.is_empty() {
                    if !pending_pkg.is_empty() {
                        fail_cnt += 1;
                    }
                    pkg.to_string()
                } else {
                    pending_pkg.clone()
                };
                if owner.is_empty() {
                    fail_cnt += 1;
                    continue;
                }
                cur_pkg = owner;
                pending_pkg.clear();
                in_block = true;
            }
            OuterLine::Pending { pkg } => {
                if !pending_pkg.is_empty() {
                    fail_cnt += 1;
                }
                pending_pkg = pkg.to_string();
            }
            OuterLine::Junk => {
                fail_cnt += 1;
                pending_pkg.clear();
            }
        }
    }

    if in_block || !pending_pkg.is_empty() {
        fail_cnt += 1;
    }

    (rules, fail_cnt)
}
pub fn load_config(
    config_file: &str,
    topo: &CpuTopology,
    last_mtime: &mut i64,
) -> Option<AppConfig> {
    let metadata = fs::metadata(config_file).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;

    if *last_mtime == mtime && *last_mtime != -1 {
        return None;
    }

    let content = fs::read_to_string(config_file).ok()?;

    let (parsed, fail_cnt) = parse_rules(&content, topo);

    let mut rules: Vec<AffinityRule> = Vec::new();
    let mut dyn_rules: Vec<DynRule> = Vec::new();
    for parsed in parsed {
        match parsed {
            ParsedRule::Static { pkg, thread, cpus } => {
                add_rule(&mut rules, topo, &pkg, &thread, &cpus);
            }
            ParsedRule::Dyn { pkg, thread, spec } => dyn_rules.push(DynRule {
                thread_pattern: CString::new(thread.as_str()).unwrap_or_default(),
                pkg,
                thread,
                spec,
            }),
        }
    }

    *last_mtime = mtime;
    PARSE_FAILS.store(fail_cnt, Ordering::Relaxed);

    let mut pkgs: HashSet<String> = rules.iter().map(|r| r.pkg.clone()).collect();
    pkgs.extend(dyn_rules.iter().map(|r| r.pkg.clone()));
    let has_thread_rules: HashSet<String> = rules
        .iter()
        .filter(|r| !r.thread.is_empty())
        .map(|r| r.pkg.clone())
        .collect();
    let has_dyn_rules: HashSet<String> = dyn_rules.iter().map(|r| r.pkg.clone()).collect();
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
    let mut dyn_by_pkg: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, r) in dyn_rules.iter().enumerate() {
        dyn_by_pkg.entry(r.pkg.clone()).or_default().push(i);
    }
    let num_rules = rules.len() + dyn_rules.len();

    println!("配置文件解析完成，共加载 {} 条规则", num_rules);
    if fail_cnt > 0 {
        eprintln!("警告: {} 条规则因格式无效被跳过", fail_cnt);
    }

    Some(AppConfig {
        rules,
        pkgs,
        has_thread_rules,
        topo: topo.clone(),
        rules_by_pkg,
        exact_thread_rules,
        dyn_rules,
        dyn_by_pkg,
        has_dyn_rules,
        cpuset_dirs: Mutex::new(HashSet::new()),
    })
}

pub fn config_loader() {
    let name = CString::new("ConfigLoader").unwrap();
    unsafe {
        libc::pthread_setname_np(libc::pthread_self(), name.as_ptr());
    }

    let mut last_mtime: i64 = -1;

    loop {
        let interval = CHECK_INTERVAL.load(Ordering::Relaxed).max(1);
        if FORCE_RELOAD.swap(false, Ordering::AcqRel) {
            last_mtime = -1;
            if INOTIFY_SUPPORTED.load(Ordering::Acquire) {
                let fd = INOTIFY_FD.load(Ordering::Acquire);
                inotify_rewatch(fd);
            }
            config_reload(&mut last_mtime);
        }
        if INOTIFY_SUPPORTED.load(Ordering::Acquire) {
            inotify_handle(interval, &mut last_mtime);
        } else {
            config_reload(&mut last_mtime);
            thread::sleep(Duration::from_secs(interval));
        }
    }
}

pub fn init_inotify(config_file: &str) {
    let inotify_fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    if inotify_fd < 0 {
        println!("inotify初始化失败，使用轮询模式");
        return;
    }
    let cfg_cstr = match CString::new(config_file) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("错误: 配置文件路径包含非法字符，使用轮询模式");
            unsafe { libc::close(inotify_fd); }
            return;
        }
    };
    let wd = unsafe {
        libc::inotify_add_watch(
            inotify_fd,
            cfg_cstr.as_ptr(),
            libc::IN_CLOSE_WRITE | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF,
        )
    };
    if wd >= 0 {
        INOTIFY_SUPPORTED.store(true, Ordering::Release);
        INOTIFY_FD.store(inotify_fd, Ordering::Release);
        INOTIFY_WD.store(wd, Ordering::Release);
        println!("启用inotify监控配置文件变更");
    } else {
        unsafe {
            libc::close(inotify_fd);
        }
        println!("inotify初始化失败，使用轮询模式");
    }
}

fn disable_inotify(inotify_fd: i32) {
    INOTIFY_SUPPORTED.store(false, Ordering::Release);
    unsafe {
        libc::close(inotify_fd);
    }
    INOTIFY_FD.store(-1, Ordering::Release);
    INOTIFY_WD.store(-1, Ordering::Release);
}

fn config_reload(last_mtime: &mut i64) {
    let Some(cfg) = lock_ignore_poison(&CURRENT_CONFIG).clone() else {
        return;
    };
    let file = lock_ignore_poison(&CONFIG_FILE).clone();
    let Some(new_cfg) = load_config(&file, &cfg.topo, last_mtime) else {
        return;
    };
    {
        let mut guard = lock_ignore_poison(&CURRENT_CONFIG);
        *guard = Some(Arc::new(new_cfg));
    }
    CONFIG_UPDATED.store(true, Ordering::Release);
}

pub fn config_reload_now() {
    let mut mtime: i64 = -1;
    config_reload(&mut mtime);
}

fn inotify_handle(interval: u64, last_mtime: &mut i64) {
    let inotify_fd = INOTIFY_FD.load(Ordering::Acquire);

    let mut pfd = libc::pollfd {
        fd: inotify_fd,
        events: libc::POLLIN,
        revents: 0,
    };

    let timeout_ms = interval.min(10) as libc::c_int * 1000;
    let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };

    if ret < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            return;
        }
        disable_inotify(inotify_fd);
        return;
    } else if ret == 0 {
        return;
    }

    #[repr(align(8))]
    struct InotifyBuf([u8; 4096]);
    let mut buf = InotifyBuf([0u8; 4096]);
    let mut reload_needed = false;
    let mut needs_rewatch = false;
    let hdr = std::mem::size_of::<libc::inotify_event>();

    loop {
        let len = unsafe {
            libc::read(
                inotify_fd,
                buf.0.as_mut_ptr() as *mut libc::c_void,
                buf.0.len(),
            )
        };
        if len <= 0 {
            let err = io::Error::last_os_error();
            let errno = err.raw_os_error();
            if errno == Some(libc::EAGAIN)
                || errno == Some(libc::EWOULDBLOCK)
                || errno == Some(libc::EINTR)
            {
                break;
            }
            disable_inotify(inotify_fd);
            return;
        }

        let mut offset = 0;
        while offset + hdr <= len as usize {
            let event = unsafe { &*(buf.0.as_ptr().add(offset) as *const libc::inotify_event) };
            if event.mask & (libc::IN_CLOSE_WRITE | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0
            {
                reload_needed = true;
                *last_mtime = -1;
                if event.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
                    needs_rewatch = true;
                }
            }
            offset += hdr + event.len as usize;
        }
    }

    if needs_rewatch {
        thread::sleep(Duration::from_secs(interval));
        if !inotify_rewatch(inotify_fd) {
            return;
        }
    }

    if reload_needed {
        config_reload(last_mtime);
    }
}

fn inotify_rewatch(inotify_fd: i32) -> bool {
    let inotify_wd = INOTIFY_WD.load(Ordering::Acquire);
    unsafe {
        libc::inotify_rm_watch(inotify_fd, inotify_wd as _);
    }
    let cfg_cstr = match CString::new(lock_ignore_poison(&CONFIG_FILE).clone()) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("错误: 配置文件路径包含非法字符，降级为轮询模式");
            disable_inotify(inotify_fd);
            return false;
        }
    };
    let new_wd = unsafe {
        libc::inotify_add_watch(
            inotify_fd,
            cfg_cstr.as_ptr(),
            libc::IN_CLOSE_WRITE | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF,
        )
    };

    if new_wd < 0 {
        disable_inotify(inotify_fd);
        return false;
    }
    INOTIFY_WD.store(new_wd, Ordering::Release);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sr(pkg: &str, thread: &str, cpus: &str) -> ParsedRule {
        ParsedRule::Static {
            pkg: pkg.to_string(),
            thread: thread.to_string(),
            cpus: cpus.to_string(),
        }
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
        let content = include_str!("../../module/applist.conf");
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

