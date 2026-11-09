
use aya_ebpf::{
    helpers::{bpf_get_current_comm, bpf_get_current_pid_tgid},
    programs::TracePointContext,
};

use crate::maps::{
    applied_lookup, applied_mark, applied_unmark, offsets_load, submit_event, whitelist_matched,
    ProcEvent, TracepointOffsets, EVENT_FORK, EVENT_RENAME,
};

struct EventMeta {
    pid: i32,
    tid: i32,
    comm: [u8; 16],
    parent_tid: u32,
    child_tgid: i32,
}

#[inline(always)]
fn fork_parse(ctx: &TracePointContext, offsets: &TracepointOffsets) -> EventMeta {
    let pid_tgid = bpf_get_current_pid_tgid();
    let parent_tid = (pid_tgid & 0xFFFFFFFF) as u32;
    let parent_tgid = (pid_tgid >> 32) as u32;
    let child_pid = unsafe { ctx.read_at::<i32>(offsets.fork_child_pid as usize).unwrap_or(0) };
    let child_comm =
        unsafe { ctx.read_at::<[u8; 16]>(offsets.fork_child_comm as usize).unwrap_or([0u8; 16]) };
    let child_tgid = if offsets.fork_child_tgid != 0 {
        unsafe { ctx.read_at::<i32>(offsets.fork_child_tgid as usize).unwrap_or(0) }
    } else {
        0
    };
    EventMeta {
        pid: parent_tgid as i32,
        tid: child_pid,
        comm: child_comm,
        parent_tid,
        child_tgid,
    }
}

#[inline(always)]
fn exec_parse() -> EventMeta {
    let pid_tgid = bpf_get_current_pid_tgid();
    EventMeta {
        pid: (pid_tgid >> 32) as i32,
        tid: pid_tgid as i32,
        comm: bpf_get_current_comm().unwrap_or_default(),
        parent_tid: 0,
        child_tgid: 0,
    }
}

#[inline(always)]
pub(crate) fn report_named_event(ctx: &TracePointContext, event_type: u32) -> u32 {
    let meta = if event_type == EVENT_FORK {
        let Some(offsets) = offsets_load() else {
            return 0;
        };
        fork_parse(ctx, &offsets)
    } else {
        exec_parse()
    };

    let is_tracked = if event_type == EVENT_FORK {
        applied_lookup(meta.parent_tid)
    } else {
        applied_lookup(meta.tid as u32)
    };

    if !is_tracked && !whitelist_matched(&meta.comm) {
        return 0;
    }

    if event_type == EVENT_FORK && is_tracked {
        applied_mark(meta.tid as u32);
    }

    submit_event(ProcEvent {
        pid: meta.pid,
        tid: meta.tid,
        comm: meta.comm,
        event_type,
        child_tgid: meta.child_tgid,
    });
    0
}

#[inline(always)]
pub(crate) fn report_rename(ctx: &TracePointContext) -> u32 {
    let pid_tgid = bpf_get_current_pid_tgid();
    let tid = (pid_tgid & 0xFFFFFFFF) as u32;
    if tid == 0 {
        return 0;
    }
    let tgid = (pid_tgid >> 32) as u32;

    let tracked = applied_lookup(tid);

    let Some(offsets) = offsets_load() else {
        return 0;
    };
    let new_comm =
        unsafe { ctx.read_at::<[u8; 16]>(offsets.rename_newcomm as usize).unwrap_or([0u8; 16]) };

    if !tracked && !whitelist_matched(&new_comm) {
        return 0;
    }

    submit_event(ProcEvent {
        pid: tgid as i32,
        tid: tid as i32,
        comm: new_comm,
        event_type: EVENT_RENAME,
        child_tgid: 0,
    });
    0
}

#[inline(always)]
pub(crate) fn report_exit() -> u32 {
    let pid_tgid = bpf_get_current_pid_tgid();
    let tid = (pid_tgid & 0xFFFFFFFF) as u32;
    let tgid = (pid_tgid >> 32) as u32;

    if !applied_lookup(tid) {
        return 0;
    }

    applied_unmark(tid);

    submit_event(ProcEvent {
        pid: tgid as i32,
        tid: tid as i32,
        comm: [0u8; 16],
        event_type: crate::maps::EVENT_EXIT,
        child_tgid: 0,
    });
    0
}
