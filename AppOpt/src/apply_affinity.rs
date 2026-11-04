use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write as _;
use std::os::unix::fs::FileExt;

use crate::config::AppConfig;
use crate::cpuset::{tasks_file, tasks_path, CpuSet, CpuTopology};
use crate::{MAX_PKG_LEN, MAX_THREAD_LEN};
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

pub(crate) fn tid_tgid(tid: i32) -> Option<i32> {
    let mut buf = [0u8; 1024];
    let bytes = read_proc_file(tid, "status", &mut buf)?;
    let text = std::str::from_utf8(bytes).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Tgid:") {
            return rest.trim().parse().ok();
        }
    }
    None
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
/// 将线程写入 cpuset tasks 文件（单次 write，O_APPEND 下写入原子）
fn cpuset_attach(tid: i32, cpuset_dir: &str) {
    let path = tasks_path(cpuset_dir);
    let Some(file) = tasks_file(&path) else {
        eprintln!("警告: 打开 cpuset tasks 文件 {} 失败", path);
        return;
    };
    let line = format!("{}\n", tid);
    let mut sink: &File = file.as_ref();
    if let Err(e) = sink.write_all(line.as_bytes()) {
        eprintln!("警告: cpuset 迁移 tid={} 到 {} 失败 ({})", tid, path, e);
    }
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
        cpuset_attach(tid, cpuset_dir);
    }
    if let Err(e) = cpus.set_affinity(tid) {
        if e.raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        eprintln!("警告: 设置 tid={} 亲和性失败 ({})", tid, e);
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
        for (dir, tids) in &groups {
            let path = tasks_path(dir);
            let Some(file) = tasks_file(&path) else {
                eprintln!("警告: 打开 cpuset tasks 文件 {} 失败", path);
                continue;
            };
            let mut buf = String::with_capacity(tids.len() * 8);
            for tid in tids {
                buf.push_str(&tid.to_string());
                buf.push('\n');
            }
            let mut sink: &File = file.as_ref();
            if let Err(e) = sink.write_all(buf.as_bytes()) {
                eprintln!("警告: cpuset 迁移 {} 失败 ({})", path, e);
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
