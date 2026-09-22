//! Opt-in structured measurement events for the monolith benchmark protocol.
//!
//! Off unless `JABAR_BENCH_LOG` names a file. When it does, each instrumented
//! boundary appends one JSON object per line, and the benchmark client in
//! `tools/bench-client` reads that stream instead of parsing logs — the
//! separation `docs/monolith-measurements.md` asks for.
//!
//! The field set is fixed and numeric. An event can carry a duration, an
//! outcome drawn from compile-time constants, counts, and byte sizes; it has no
//! field that can hold a source path or a symbol string. That is the privacy
//! rule from the measurement plan, enforced by the type rather than by
//! reviewer vigilance.
//!
//! When disabled every entry point is a single `OnceLock` read and returns
//! immediately, so leaving the calls in a release build costs effectively
//! nothing.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

static RECORDER: OnceLock<Option<Recorder>> = OnceLock::new();

struct Recorder {
    /// Process-relative zero, so `t_ns` is comparable across events in a run
    /// without depending on wall-clock skew.
    process_start: Instant,
    run_id: String,
    seq: AtomicU64,
    out: Mutex<BufWriter<File>>,
}

/// Wires up benchmarking from the environment, if requested.
///
/// Idempotent and cheap. Call it once at process entry so the process-relative
/// clock and `startup.*` durations count from the true start rather than from
/// the first instrumented boundary.
pub fn init() {
    let _ = recorder();
}

/// True when a bench log is configured. Used to arm the event-loop heartbeat
/// only under measurement, so production never wakes the loop every 20ms.
pub fn enabled() -> bool {
    recorder().is_some()
}

fn recorder() -> Option<&'static Recorder> {
    RECORDER
        .get_or_init(|| {
            let path = std::env::var_os("JABAR_BENCH_LOG")?;
            let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
            let run_id = std::env::var("JABAR_BENCH_RUN")
                .unwrap_or_else(|_| format!("{}-{}", std::process::id(), unix_millis()));
            Some(Recorder {
                process_start: Instant::now(),
                run_id,
                seq: AtomicU64::new(0),
                out: Mutex::new(BufWriter::new(file)),
            })
        })
        .as_ref()
}

fn unix_millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Starts a timer, or `None` when benchmarking is off so the caller pays
/// nothing. Pair it with [`finish`].
pub fn start() -> Option<Instant> {
    recorder().map(|_| Instant::now())
}

/// Records the elapsed time since [`start`] under `name`, with `fields`.
///
/// A no-op when benchmarking is off or `since` is `None`, so an instrumented
/// boundary compiles to a branch and a couple of moves in a release build.
pub fn finish(name: &'static str, since: Option<Instant>, fields: Fields) {
    if let (Some(rec), Some(since)) = (recorder(), since) {
        rec.emit(name, since.elapsed().as_nanos(), fields);
    }
}

/// Records a point event with no duration — a heartbeat tick, say, whose
/// interesting quantity is `lateness_ns` rather than how long handling took.
pub fn mark(name: &'static str, fields: Fields) {
    if let Some(rec) = recorder() {
        rec.emit(name, 0, fields);
    }
}

impl Recorder {
    fn emit(&self, name: &'static str, dur_ns: u128, fields: Fields) {
        let event = Event {
            name,
            run_id: &self.run_id,
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            t_ns: self.process_start.elapsed().as_nanos(),
            dur_ns,
            fields,
        };
        // A poisoned lock or a serialization failure must never take the
        // session down: this is measurement, not a load-bearing path. Flush per
        // line so a killed process still leaves complete records behind.
        if let Ok(mut out) = self.out.lock()
            && serde_json::to_writer(&mut *out, &event).is_ok()
        {
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
    }
}

#[derive(Serialize)]
struct Event<'a> {
    name: &'a str,
    run_id: &'a str,
    seq: u64,
    /// Nanoseconds since process start.
    t_ns: u128,
    /// Duration of the measured span, or 0 for a point event.
    dur_ns: u128,
    #[serde(flatten)]
    fields: Fields,
}

/// The complete, fixed vocabulary an event may carry.
///
/// Every field is numeric, boolean, or a `&'static str` outcome. There is
/// deliberately no free-form string, no path, and no symbol: the type makes it
/// impossible to leak repository contents into a bench log.
#[derive(Default, Serialize)]
pub struct Fields {
    /// A compile-time outcome label, e.g. `"ok"`, `"miss"`, `"empty"`, `"error"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shards: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definitions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrences: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Event-loop scheduling lateness for a heartbeat, in nanoseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lateness_ns: Option<u128>,
}
