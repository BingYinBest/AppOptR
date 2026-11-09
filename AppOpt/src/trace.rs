//! Optional CSV trace for on-device tuning.
//!
//! Enabled only when `APPOPT_TRACE=<path>` is set before the daemon starts.
//! Every decision that could change a thread's tier is appended as one line,
//! so a real-device run can be replayed and the EWMA thresholds retuned from
//! measured bursts instead of guessed numbers. The sink stops at 16 MiB.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 16 * 1024 * 1024;

struct Sink {
    out: Mutex<(File, u64)>,
}

static SINK: OnceLock<Option<Sink>> = OnceLock::new();

fn sink() -> Option<&'static Sink> {
    SINK.get_or_init(|| {
        let path = std::env::var_os("APPOPT_TRACE")?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len == 0 {
            let _ = file.write_all(b"ts_ms,src,tid,fast,slow,action\n");
        }
        Some(Sink {
            out: Mutex::new((file, len)),
        })
    })
    .as_ref()
}

pub fn enabled() -> bool {
    sink().is_some()
}

pub fn log(src: &str, tid: i32, fast: u32, slow: u32, action: &str) {
    let Some(s) = sink() else { return };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let Ok(mut guard) = s.out.lock() else { return };
    let (file, written) = &mut *guard;
    if *written >= MAX_BYTES {
        return;
    }
    let line = format!("{ts},{src},{tid},{fast},{slow},{action}\n");
    if file.write_all(line.as_bytes()).is_ok() {
        *written += line.len() as u64;
    }
}
