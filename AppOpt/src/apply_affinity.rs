use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::FileExt;

use crate::{MAX_PKG_LEN, MAX_THREAD_LEN};
use crate::config::AppConfig;
use crate::cpuset::{base_cpuset, CpuSet, CpuTopology};
fn read_proc_file<'a>(pid: i32, suffix: &str, buf: &'a mut [u8]) -> Option<&'a [u8]> {
    let mut path = [0u8; 32];
    let mut cur = std::io::Cursor::new(&mut path[..]);
    write!(cur, "/proc/{}/{}", pid, suffix).ok()?;
    let len = cur.position() as usize;
    let path_str = std::str::from_utf8(&path[..len]).ok()?;
    let file = fs::File::open(path_str).ok()?;
    let n = file.read_at(buf, 0).ok()?;
    (n > 0).then_some(&buf[..n])
}

pub(crate) fn read_cmdline(pid: i32) -> Option<String> {
    let mut buf = [0u8; MAX_PKG_LEN];
    let bytes = read_proc_file(pid, "cmdline", &mut buf)?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let cmdline = std::str::from_utf8(&bytes[..end]).ok()?;
    let name = cmdline.rsplit('/').next().unwrap_or(cmdline);
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

pub(crate) fn tid_comm(tid: i32) -> Option<String> {
    let mut buf = [0u8; MAX_THREAD_LEN];
    let bytes = read_proc_file(tid, "comm", &mut buf)?;
    let end = bytes
        .iter()
        .position(|&b| b == 0 || b == b'\n')
        .unwrap_or(bytes.len());
    let name = std::str::from_utf8(&bytes[..end]).ok()?;
    Some(name.trim().to_string())
}

pub(crate) fn task_tids(pid: i32) -> Option<Vec<i32>> {
    let mut path_buf = [0u8; 32];
    let mut cur = std::io::Cursor::new(&mut path_buf[..]);
    write!(cur, "/proc/{}/task", pid).ok()?;
    let len = cur.position() as usize;
    let task_path = std::str::from_utf8(&path_buf[..len]).unwrap();
    let task_dir = fs::read_dir(task_path).ok()?;
    Some(
        task_dir
            .flatten()
            .filter_map(|tent| tent.file_name().to_string_lossy().parse::<i32>().ok())
            .collect(),
    )
}
pub fn affinity_set(
    tid: i32,
    cpus: &CpuSet,
    cpuset_dir: &str,
    topo: &CpuTopology,
) -> bool {
    if let Some(curr) = CpuSet::get_affinity(tid)
        && curr == *cpus {
            return false;
        }
    if topo.cpuset_enabled {
        let base = base_cpuset();
        let mut path = [0u8; 256];
        let mut cur = std::io::Cursor::new(&mut path[..]);
        if cpuset_dir.is_empty() {
            let _ = write!(cur, "{}/tasks", base);
        } else {
            let _ = write!(cur, "{}/{}/tasks", base, cpuset_dir);
        }
        let len = cur.position() as usize;
        let path_str = std::str::from_utf8(&path[..len]).unwrap_or_default();
        if let Err(e) = fs::OpenOptions::new()
            .append(true)
            .open(path_str)
            .and_then(|mut f| writeln!(f, "{}", tid))
        {
            eprintln!("警告: cpuset 迁移 tid={} 到 {} 失败 ({})", tid, path_str, e);
        }
    }
    if let Err(e) = cpus.set_affinity(tid) {
        return e.raw_os_error() == Some(libc::ESRCH);
    }

    false
}
pub fn affinity_sync_batch<'a>(
    items: impl Iterator<Item = (i32, &'a CpuSet, &'a str)>,
    topo: &CpuTopology,
) -> Vec<i32> {
    let mut to_move: Vec<(i32, &CpuSet, &str)> = Vec::new();
    for (tid, cpus, dir) in items {
        if CpuSet::get_affinity(tid) != Some(*cpus) {
            to_move.push((tid, cpus, dir));
        }
    }
    if topo.cpuset_enabled && !to_move.is_empty() {
        let mut groups: std::collections::HashMap<&str, Vec<i32>> = std::collections::HashMap::new();
        for (tid, _, dir) in &to_move {
            groups.entry(dir).or_default().push(*tid);
        }
        let base = base_cpuset();
        for (dir, tids) in &groups {
            let mut path = [0u8; 256];
            let mut cur = std::io::Cursor::new(&mut path[..]);
            if dir.is_empty() {
                let _ = write!(cur, "{}/tasks", base);
            } else {
                let _ = write!(cur, "{}/{}/tasks", base, dir);
            }
            let len = cur.position() as usize;
            let Some(path_str) = std::str::from_utf8(&path[..len]).ok() else {
                continue;
            };
            match fs::OpenOptions::new().append(true).open(path_str) {
                Ok(mut f) => {
                    for tid in tids {
                        let _ = writeln!(f, "{}", tid);
                    }
                }
                Err(e) => eprintln!("警告: cpuset 迁移 {} 失败 ({})", path_str, e),
            }
        }
    }
    to_move
        .into_iter()
        .filter_map(|(tid, cpus, _)| {
            if let Err(e) = cpus.set_affinity(tid) {
                (e.raw_os_error() == Some(libc::ESRCH)).then_some(tid)
            } else {
                None
            }
        })
        .collect()
}
pub(crate) fn proc_snapshot() -> Option<HashSet<i32>> {
    let entries = fs::read_dir("/proc").ok()?;
    Some(
        entries
            .flatten()
            .filter_map(|ent| ent.file_name().to_string_lossy().parse::<i32>().ok())
            .collect(),
    )
}
pub(crate) fn proc_walk(
    cfg: &AppConfig,
    filter: impl Fn(i32) -> bool,
    mut f: impl FnMut(i32, &str, bool),
) -> (usize, i32) {
    let Some(entries) = fs::read_dir("/proc").ok() else { return (0, 0) };
    let mut count = 0;
    let mut total: i32 = 0;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
        total += 1;
        if !filter(pid) { continue; }
        let Some(pkg) = read_cmdline(pid).or_else(|| tid_comm(pid)) else { continue };
        if !cfg.pkgs.contains(&pkg) { continue; }
        f(pid, &pkg, cfg.has_thread_rules.contains(&pkg));
        count += 1;
    }
    (count, total)
}
