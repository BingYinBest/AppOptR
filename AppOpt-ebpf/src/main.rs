//!
//!

#![no_std]
#![no_main]
#![allow(linker_messages)]
#![allow(dead_code)]

mod events;
mod maps;

use aya_ebpf::{
    macros::tracepoint,
    programs::TracePointContext,
};

use crate::maps::{EVENT_EXEC, EVENT_FORK};

#[tracepoint(name = "sched_process_fork", category = "sched")]
fn sched_process_fork(ctx: TracePointContext) -> u32 {
    events::report_named_event(&ctx, EVENT_FORK)
}

#[tracepoint(name = "sched_process_exec", category = "sched")]
fn sched_process_exec(ctx: TracePointContext) -> u32 {
    events::report_named_event(&ctx, EVENT_EXEC)
}

#[tracepoint(name = "task_rename", category = "task")]
fn task_rename(ctx: TracePointContext) -> u32 {
    events::report_rename(&ctx)
}

#[tracepoint(name = "sched_process_exit", category = "sched")]
fn sched_process_exit(_ctx: TracePointContext) -> u32 {
    events::report_exit()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
