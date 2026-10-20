use std::ffi::CString;
use std::fmt::Write as _;
use std::fs;
use std::io;

use std::sync::RwLock;

pub const CPU_SETSIZE: usize = 1024;
pub const CPU_WORD_BITS: usize = 64;
pub const CPU_WORDS: usize = CPU_SETSIZE / CPU_WORD_BITS;
static BASE_CPUSET_PATH: RwLock<Option<String>> = RwLock::new(None);

pub const DEFAULT_CPUSET_NAME: &str = "AppOpt";
pub fn set_base_cpuset(name: &str) {
    if name.is_empty() || name.contains('/') {
        return;
    }
    *BASE_CPUSET_PATH.write().unwrap() = Some(format!("/dev/cpuset/{}", name));
}

pub fn base_cpuset() -> String {
    BASE_CPUSET_PATH
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(|| format!("/dev/cpuset/{DEFAULT_CPUSET_NAME}"))
}

const _: () = assert!(std::mem::size_of::<CpuSet>() == std::mem::size_of::<libc::cpu_set_t>());

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuSet {
    pub bits: [u64; CPU_WORDS],
}

impl std::fmt::Debug for CpuSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CpuSet({})", self.to_range_string())
    }
}

impl CpuSet {
    pub fn new() -> Self {
        CpuSet::default()
    }

    pub fn set(&mut self, cpu: usize) {
        if cpu < CPU_SETSIZE {
            self.bits[cpu / CPU_WORD_BITS] |= 1u64 << (cpu % CPU_WORD_BITS);
        }
    }

    pub fn is_set(&self, cpu: usize) -> bool {
        cpu < CPU_SETSIZE && self.bits[cpu / CPU_WORD_BITS] & (1u64 << (cpu % CPU_WORD_BITS)) != 0
    }

    pub fn count(&self) -> usize {
        self.bits.iter().map(|&b| b.count_ones() as usize).sum()
    }

    pub fn or(&mut self, other: &CpuSet) {
        for (d, &s) in self.bits.iter_mut().zip(other.bits.iter()) {
            *d |= s;
        }
    }

    pub fn to_range_string(self) -> String {
        let mut result = String::new();
        let mut start: Option<usize> = None;
        let mut end: Option<usize> = None;
        let mut first = true;

        for (word_idx, &word) in self.bits.iter().enumerate() {
            if word == 0 {
                if start.is_some() {
                    push_range(&mut result, start, end, &mut first);
                    start = None;
                    end = None;
                }
                continue;
            }
            let base = word_idx * CPU_WORD_BITS;
            for bit in 0..CPU_WORD_BITS {
                if word & (1u64 << bit) != 0 {
                    let cpu = base + bit;
                    if start.is_none() {
                        start = Some(cpu);
                        end = Some(cpu);
                    } else if end.is_some_and(|e| cpu == e + 1) {
                        end = Some(cpu);
                    } else {
                        push_range(&mut result, start, end, &mut first);
                        start = Some(cpu);
                        end = Some(cpu);
                    }
                }
            }
        }
        push_range(&mut result, start, end, &mut first);
        result
    }

    pub fn get_affinity(tid: i32) -> Option<CpuSet> {
        let mut curr = CpuSet::new();
        let ret = unsafe {
            libc::sched_getaffinity(
                tid,
                std::mem::size_of::<CpuSet>(),
                &mut curr as *mut CpuSet as *mut libc::cpu_set_t,
            )
        };
        if ret == -1 {
            None
        } else {
            Some(curr)
        }
    }

    pub fn set_affinity(&self, tid: i32) -> io::Result<()> {
        let ret = unsafe {
            libc::sched_setaffinity(
                tid,
                std::mem::size_of::<CpuSet>(),
                self as *const CpuSet as *const libc::cpu_set_t,
            )
        };
        if ret == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

fn push_range(s: &mut String, start: Option<usize>, end: Option<usize>, first: &mut bool) {
    if let (Some(lo), Some(hi)) = (start, end) {
        if !*first {
            s.push(',');
        }
        if lo == hi {
            let _ = write!(s, "{}", lo);
        } else {
            let _ = write!(s, "{}-{}", lo, hi);
        }
        *first = false;
    }
}

pub fn parse_cpu_ranges(spec: &str, present: Option<&CpuSet>) -> CpuSet {
    let mut set = CpuSet::new();
    if spec.is_empty() {
        return set;
    }
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (lo, hi) = if let Some(pos) = part.find('-') {
            let a: usize = part[..pos].parse().ok().unwrap_or(usize::MAX);
            let b: usize = part[pos + 1..].parse().ok().unwrap_or(a);
            if a == usize::MAX {
                continue;
            }
            if a > b {
                (b, a)
            } else {
                (a, b)
            }
        } else {
            let a: usize = part.parse().ok().unwrap_or(usize::MAX);
            if a == usize::MAX {
                continue;
            }
            (a, a)
        };
        for i in lo..=hi.min(CPU_SETSIZE - 1) {
            if let Some(present) = present
                && !present.is_set(i) {
                    continue;
                }
            set.set(i);
        }
    }
    set
}
pub fn parse_cpu_spec(spec: &str, topo: &CpuTopology) -> CpuSet {
    let mut set = CpuSet::new();
    if spec.is_empty() {
        return set;
    }
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let seg = match part {
            "e-core" => &topo.e_core,
            "m-core" => &topo.m_core,
            "p-core" => &topo.p_core,
            "hp-core" => &topo.hp_core,
            "all-core" => &topo.present_cpus,
            _ => {
                set.or(&parse_cpu_ranges(part, Some(&topo.present_cpus)));
                continue;
            }
        };
        set.or(seg);
    }
    set
}
pub(crate) fn create_cpuset_dir(path: &str, cpus: &str, mems: &str) -> bool {
    let c_path = CString::new(path).expect("cpuset path 受控输入，无 NUL");
    let ret = unsafe { libc::mkdir(c_path.as_ptr(), 0o755) };
    if ret != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return false;
        }
        if cpuset_file_eq(&format!("{}/cpus", path), cpus)
            && cpuset_file_eq(&format!("{}/mems", path), mems)
        {
            return true;
        }
    }
    if unsafe { libc::chmod(c_path.as_ptr(), 0o755) } != 0 {
        return false;
    }
    if unsafe { libc::chown(c_path.as_ptr(), 0, 0) } != 0 {
        return false;
    }
    let cpus_path = format!("{}/cpus", path);
    if fs::write(&cpus_path, cpus).is_err() {
        return false;
    }
    let mems_path = format!("{}/mems", path);
    fs::write(&mems_path, mems).is_ok()
}
fn cpuset_file_eq(path: &str, want: &str) -> bool {
    fs::read_to_string(path)
        .ok()
        .map(|cur| parse_cpu_ranges(&cur, None) == parse_cpu_ranges(want, None))
        .unwrap_or(false)
}
pub(crate) fn remove_cpuset_dir(name: &str) -> bool {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return false;
    }
    let root = "/dev/cpuset";
    let root_c = CString::new(root).expect("常量字符串无 NUL");
    if unsafe { libc::access(root_c.as_ptr(), libc::F_OK) } != 0 {
        return true;
    }
    let path = format!("{}/{}", root, name);
    if fs::metadata(&path).is_err() {
        return true;
    }
    remove_tree(&path);
    fs::metadata(&path).is_err()
}
fn remove_tree(dir: &str) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir())
                && let Some(p) = entry.path().to_str()
            {
                remove_tree(p);
            }
        }
    }
    if let Ok(tasks) = fs::read_to_string(format!("{}/tasks", dir)) {
        for tid in tasks.split_whitespace() {
            let _ = fs::write("/dev/cpuset/tasks", format!("{}\n", tid));
        }
    }
    let _ = fs::remove_dir(dir);
}

#[derive(Clone)]
pub struct CpuTopology {
    pub present_cpus: CpuSet,
    pub present_str: String,
    pub mems_str: String,
    pub cpuset_enabled: bool,
    pub e_core: CpuSet,
    pub m_core: CpuSet,
    pub p_core: CpuSet,
    pub hp_core: CpuSet,
}
fn detect_core_types() -> (CpuSet, CpuSet, CpuSet, CpuSet) {
    let mut groups: Vec<(u64, Vec<usize>)> = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu/cpufreq") {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("policy"))
            {
                continue;
            }
            let freq: u64 = fs::read_to_string(path.join("cpuinfo_max_freq"))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            if freq == 0 {
                continue;
            }
            let cpus: Vec<usize> = fs::read_to_string(path.join("related_cpus"))
                .ok()
                .map(|s| s.split_whitespace().filter_map(|c| c.parse().ok()).collect())
                .unwrap_or_default();
            if cpus.is_empty() {
                continue;
            }
            if let Some(g) = groups.iter_mut().find(|(f, _)| *f == freq) {
                g.1.extend(cpus);
            } else {
                groups.push((freq, cpus));
            }
        }
    }
    groups.sort_by_key(|(f, _)| *f);
    assign_groups(&groups)
}
fn assign_groups(groups: &[(u64, Vec<usize>)]) -> (CpuSet, CpuSet, CpuSet, CpuSet) {
    let mut e = CpuSet::new();
    let mut m = CpuSet::new();
    let mut p = CpuSet::new();
    let mut h = CpuSet::new();
    let n = groups.len();
    for (i, (_, cpus)) in groups.iter().enumerate() {
        let target: &mut CpuSet = if n == 1 || i == 0 {
            &mut e
        } else if i == n - 1 {
            &mut h
        } else if n >= 4 && i == 1 {
            &mut m
        } else {
            &mut p
        };
        for &cpu in cpus {
            target.set(cpu);
        }
    }
    (e, m, p, h)
}
pub fn init_cpu_topo() -> CpuTopology {
    let mut topo = CpuTopology {
        present_cpus: CpuSet::new(),
        present_str: String::new(),
        mems_str: String::new(),
        cpuset_enabled: false,
        e_core: CpuSet::new(),
        m_core: CpuSet::new(),
        p_core: CpuSet::new(),
        hp_core: CpuSet::new(),
    };

    if let Ok(content) = fs::read_to_string("/sys/devices/system/cpu/present") {
        topo.present_str = content.trim().to_string();
    }
    topo.present_cpus = parse_cpu_ranges(&topo.present_str, None);
    let (e, m, p, h) = detect_core_types();
    topo.e_core = e;
    topo.m_core = m;
    topo.p_core = p;
    topo.hp_core = h;

    let cpuset_path = CString::new("/dev/cpuset").expect("常量字符串无 NUL");
    if unsafe { libc::access(cpuset_path.as_ptr(), libc::F_OK) } != 0 {
        return topo;
    }

    let mems = fs::read_to_string("/dev/cpuset/mems")
        .ok()
        .and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        })
        .unwrap_or_else(|| "0".to_string());
    topo.mems_str = mems;

    if create_cpuset_dir(&base_cpuset(), &topo.present_str, &topo.mems_str) {
        topo.cpuset_enabled = true;
    }

    topo
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_of(cpus: &[usize]) -> CpuSet {
        let mut s = CpuSet::new();
        for &c in cpus {
            s.set(c);
        }
        s
    }

    fn topo_with_cores(e: &[usize], m: &[usize], p: &[usize], h: &[usize], present: &[usize]) -> CpuTopology {
        CpuTopology {
            present_cpus: set_of(present),
            present_str: String::new(),
            mems_str: "0".to_string(),
            cpuset_enabled: false,
            e_core: set_of(e),
            m_core: set_of(m),
            p_core: set_of(p),
            hp_core: set_of(h),
        }
    }

    #[test]
    fn range_string_empty() {
        assert_eq!(CpuSet::new().to_range_string(), "");
    }

    #[test]
    fn range_string_single() {
        assert_eq!(set_of(&[3]).to_range_string(), "3");
    }

    #[test]
    fn range_string_contiguous() {
        assert_eq!(set_of(&[0, 1, 2, 3]).to_range_string(), "0-3");
    }

    #[test]
    fn range_string_segments() {
        assert_eq!(set_of(&[0, 1, 4, 5, 6, 9]).to_range_string(), "0-1,4-6,9");
    }

    #[test]
    fn range_string_spans_words() {
        assert_eq!(set_of(&[62, 63, 64, 65]).to_range_string(), "62-65");
    }

    #[test]
    fn parse_ranges_basic() {
        let got = parse_cpu_ranges("0-3,5", None);
        assert_eq!(got, set_of(&[0, 1, 2, 3, 5]));
    }

    #[test]
    fn parse_ranges_reversed() {
        let got = parse_cpu_ranges("5-2", None);
        assert_eq!(got, set_of(&[2, 3, 4, 5]));
    }

    #[test]
    fn parse_ranges_clamped_to_setsize() {
        let got = parse_cpu_ranges(&format!("0-{}", CPU_SETSIZE + 100), None);
        assert_eq!(got.count(), CPU_SETSIZE);
        assert!(got.is_set(CPU_SETSIZE - 1));
        assert!(!got.is_set(CPU_SETSIZE));
    }

    #[test]
    fn parse_ranges_invalid_parts_skipped() {
        assert_eq!(parse_cpu_ranges("x,2,", None), set_of(&[2]));
        assert_eq!(parse_cpu_ranges("", None), CpuSet::new());
    }

    #[test]
    fn parse_ranges_present_filter() {
        let present = set_of(&[0, 1, 2, 3, 8]);
        let got = parse_cpu_ranges("0-8", Some(&present));
        assert_eq!(got, set_of(&[0, 1, 2, 3, 8]));
    }

    #[test]
    fn parse_spec_semantic_cores() {
        let topo = topo_with_cores(&[0, 1], &[], &[2, 3], &[4, 5, 6], &[0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(parse_cpu_spec("e-core", &topo), set_of(&[0, 1]));
        assert_eq!(parse_cpu_spec("p-core", &topo), set_of(&[2, 3]));
        assert_eq!(parse_cpu_spec("hp-core", &topo), set_of(&[4, 5, 6]));
        assert_eq!(parse_cpu_spec("all-core", &topo), set_of(&[0, 1, 2, 3, 4, 5, 6]));
    }

    #[test]
    fn parse_spec_mcore() {
        let topo = topo_with_cores(&[0, 1], &[2, 3], &[4, 5], &[6], &[0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(parse_cpu_spec("m-core", &topo), set_of(&[2, 3]));
        assert_eq!(parse_cpu_spec("e-core,m-core", &topo), set_of(&[0, 1, 2, 3]));
    }

    #[test]
    fn parse_spec_mixed() {
        let topo = topo_with_cores(&[0, 1], &[], &[2, 3], &[4], &[0, 1, 2, 3, 4]);
        let got = parse_cpu_spec("e-core,3-4", &topo);
        assert_eq!(got, set_of(&[0, 1, 3, 4]));
    }

    #[test]
    fn parse_spec_empty() {
        let topo = topo_with_cores(&[0], &[], &[1], &[2], &[0, 1, 2]);
        assert_eq!(parse_cpu_spec("", &topo), CpuSet::new());
    }

    #[test]
    fn assign_groups_tiers() {
        let g = |freq: u64, cpus: &[usize]| (freq, cpus.to_vec());
        let (e, m, p, h) = assign_groups(&[g(1000, &[0, 1, 2, 3])]);
        assert_eq!(e, set_of(&[0, 1, 2, 3]));
        assert_eq!(m, set_of(&[]));
        assert_eq!(p, set_of(&[]));
        assert_eq!(h, set_of(&[]));
        let (e, m, p, h) = assign_groups(&[g(1000, &[0, 1]), g(2500, &[2, 3])]);
        assert_eq!(e, set_of(&[0, 1]));
        assert_eq!(m, set_of(&[]));
        assert_eq!(p, set_of(&[]));
        assert_eq!(h, set_of(&[2, 3]));
        let (e, m, p, h) = assign_groups(&[g(1000, &[0, 1]), g(1800, &[2, 3]), g(2500, &[4])]);
        assert_eq!(e, set_of(&[0, 1]));
        assert_eq!(m, set_of(&[]));
        assert_eq!(p, set_of(&[2, 3]));
        assert_eq!(h, set_of(&[4]));
        let (e, m, p, h) = assign_groups(&[
            g(1000, &[0, 1]),
            g(1800, &[2, 3]),
            g(2500, &[4, 5]),
            g(3200, &[6]),
        ]);
        assert_eq!(e, set_of(&[0, 1]));
        assert_eq!(m, set_of(&[2, 3]));
        assert_eq!(p, set_of(&[4, 5]));
        assert_eq!(h, set_of(&[6]));
        let (e, m, p, h) = assign_groups(&[
            g(1000, &[0]),
            g(1500, &[1]),
            g(2000, &[2]),
            g(2500, &[3]),
            g(3000, &[4]),
        ]);
        assert_eq!(e, set_of(&[0]));
        assert_eq!(m, set_of(&[1]));
        assert_eq!(p, set_of(&[2, 3]));
        assert_eq!(h, set_of(&[4]));
    }

    #[test]
    fn count_and_or() {
        let mut a = set_of(&[0, 1]);
        a.or(&set_of(&[1, 2]));
        assert_eq!(a, set_of(&[0, 1, 2]));
        assert_eq!(a.count(), 3);
    }

    #[test]
    fn cpuset_file_eq_normalizes_ranges() {
        let dir = std::env::temp_dir().join(format!("appopt-cpuset-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("cpus");
        std::fs::write(&f, "0-2\n").unwrap();
        assert!(cpuset_file_eq(f.to_str().unwrap(), "0-2"));
        assert!(cpuset_file_eq(f.to_str().unwrap(), "0,1,2"));
        assert!(!cpuset_file_eq(f.to_str().unwrap(), "0-3"));
        assert!(!cpuset_file_eq(f.to_str().unwrap(), "0,2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_cpuset_dir_rejects_invalid_names() {
        assert!(!remove_cpuset_dir(""));
        assert!(!remove_cpuset_dir("a/b"));
        assert!(!remove_cpuset_dir("/dev/cpuset"));
        assert!(!remove_cpuset_dir(".."));
    }
}
