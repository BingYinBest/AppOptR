//!

use aya_ebpf::{
    macros::map,
    maps::{Array, HashMap, LruHashMap, RingBuf},
};

pub(crate) const EVENT_FORK: u32 = 1;
pub(crate) const EVENT_EXEC: u32 = 2;
pub(crate) const EVENT_RENAME: u32 = 3;
pub(crate) const EVENT_EXIT: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct TracepointOffsets {
    pub(crate) fork_child_pid: u32,
    pub(crate) fork_child_comm: u32,
    pub(crate) fork_child_tgid: u32,
    pub(crate) rename_newcomm: u32,
}

#[repr(C)]
pub(crate) struct ProcEvent {
    pub(crate) pid: i32,
    pub(crate) tid: i32,
    pub(crate) comm: [u8; 16],
    pub(crate) event_type: u32,
    pub(crate) child_tgid: i32,
}

const MAP_CAPACITY: u32 = 16 << 5;
const APPLIED_CAPACITY: u32 = 8192;

#[map]
static TARGET_COMM_MAP: HashMap<[u8; 15], u32> = HashMap::with_max_entries(MAP_CAPACITY, 0);

#[map]
static APPLIED_MAP: LruHashMap<u32, u64> = LruHashMap::with_max_entries(APPLIED_CAPACITY, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

#[map]
static DROPPED_COUNT: HashMap<u32, u32> = HashMap::with_max_entries(1, 0);

#[map]
static OFFSETS_MAP: Array<TracepointOffsets> = Array::with_max_entries(1, 0);

#[inline(always)]
pub(crate) fn offsets_load() -> Option<TracepointOffsets> {
    OFFSETS_MAP.get(0).and_then(|o| {
        if o.fork_child_pid != 0 {
            Some(*o)
        } else {
            None
        }
    })
}

#[inline(always)]
pub(crate) fn applied_lookup(key: u32) -> bool {
    unsafe { APPLIED_MAP.get(key) }.is_some()
}

#[inline(always)]
pub(crate) fn applied_mark(key: u32) {
    let _ = APPLIED_MAP.insert(key, 0, 0);
}

#[inline(always)]
pub(crate) fn applied_unmark(key: u32) {
    let _ = APPLIED_MAP.remove(key);
}

#[inline(always)]
pub(crate) fn whitelist_matched(comm: &[u8; 16]) -> bool {
    for pos in 0..=1usize {
        let key: [u8; 15] = comm[pos..pos + 15].try_into().unwrap_or([0u8; 15]);
        if unsafe { TARGET_COMM_MAP.get(key) }.is_some() {
            return true;
        }
    }
    false
}

#[inline(always)]
pub(crate) fn submit_event(event: ProcEvent) {
    if let Some(mut entry) = EVENTS.reserve(0) {
        entry.write(event);
        entry.submit(0);
    } else {
        unsafe {
            if let Some(c) = DROPPED_COUNT.get_ptr_mut(&0) {
                *c = (*c).saturating_add(1);
            }
        }
    }
}
