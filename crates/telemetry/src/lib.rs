//! Detecting when the server answers *wrongly*, not just slowly.
//!
//! Ordinary language-server instrumentation measures latency, because a human
//! notices a slow response and re-runs it. An agent does not re-run anything.
//! It takes the first answer as ground truth and writes code on that basis, so
//! the failure that costs most here is the one a latency histogram cannot see:
//! a fast, confident, wrong answer.
//!
//! The sharpest case is the empty result. `workspaceSymbol` returning nothing
//! because the index has not finished building is, on the wire, identical to
//! `workspaceSymbol` returning nothing because the symbol does not exist. The
//! client cannot tell those apart, and one of them makes it delete code that is
//! actually referenced. So [`Outcome::Empty`] carries an [`EmptyReason`], and
//! exactly one reason — [`EmptyReason::NoMatch`] — is healthy. Everything else
//! is the server saying "none" when it means "I do not know".
//!
//! That is the whole idea: make misbehaviour a thing the type system asks about
//! at every call site, rather than something to be inferred from logs later.
//!
//! # What leaves the process
//!
//! Nothing, on its own. [`Telemetry`] aggregates in memory and the caller
//! decides what to do with [`Telemetry::health`].
//!
//! The split matters because this runs over proprietary source. [`Health`]
//! holds counts, durations and operation kinds — no paths, no symbol names, no
//! file contents — so it is safe to hand to someone else. The per-query
//! `tracing` events are the opposite: they carry enough context to debug with,
//! which is exactly what makes them unsafe to ship anywhere. Keep that boundary
//! if this ever grows a reporting endpoint.

use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use serde::Serialize;

/// Something the server was asked to do.
///
/// The first nine are the client-visible operations; the rest are internal
/// steps worth timing separately, because a slow `workspaceSymbol` caused by a
/// slow BSP query is a different problem from a slow index lookup.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Op {
    WorkspaceSymbol,
    GoToDefinition,
    FindReferences,
    Hover,
    DocumentSymbol,
    GoToImplementation,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
    /// Resolving a file to its owning Bazel target.
    BspInverseSources,
    /// Fetching a target's sources, classpath, or run environment.
    BspTargetQuery,
    /// Building or loading the shallow global index.
    IndexBuild,
    /// Loading a focus target's transitive slice.
    SliceLoad,
}

impl Op {
    pub const ALL: [Op; 13] = [
        Op::WorkspaceSymbol,
        Op::GoToDefinition,
        Op::FindReferences,
        Op::Hover,
        Op::DocumentSymbol,
        Op::GoToImplementation,
        Op::PrepareCallHierarchy,
        Op::IncomingCalls,
        Op::OutgoingCalls,
        Op::BspInverseSources,
        Op::BspTargetQuery,
        Op::IndexBuild,
        Op::SliceLoad,
    ];

    /// Whether a client sees this operation's result directly. A wrong answer
    /// to one of these reaches the agent; a wrong answer to an internal step
    /// only reaches it after being laundered through one of these.
    pub fn is_client_facing(self) -> bool {
        !matches!(self, Op::BspInverseSources | Op::BspTargetQuery | Op::IndexBuild | Op::SliceLoad)
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Op::WorkspaceSymbol => "workspaceSymbol",
            Op::GoToDefinition => "goToDefinition",
            Op::FindReferences => "findReferences",
            Op::Hover => "hover",
            Op::DocumentSymbol => "documentSymbol",
            Op::GoToImplementation => "goToImplementation",
            Op::PrepareCallHierarchy => "prepareCallHierarchy",
            Op::IncomingCalls => "incomingCalls",
            Op::OutgoingCalls => "outgoingCalls",
            Op::BspInverseSources => "bsp/inverseSources",
            Op::BspTargetQuery => "bsp/targetQuery",
            Op::IndexBuild => "index/build",
            Op::SliceLoad => "slice/load",
        };
        f.write_str(s)
    }
}

/// How an operation ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Outcome {
    /// Results were produced.
    ///
    /// `total` is how many exist, `returned` how many were sent. They differ
    /// when the response was trimmed to a token budget, and the gap is exactly
    /// what the client has to be told about — a silently trimmed list makes an
    /// agent conclude it has seen every call site when it has seen the first
    /// twenty.
    Answered { returned: usize, total: usize },
    /// No results, with the reason the caller must supply.
    Empty { reason: EmptyReason },
    /// The operation could not be completed. Honest, and much safer than a
    /// wrong answer — the client retries or tells the user.
    Failed { failure: Failure },
    /// Superseded before it finished. Not a fault.
    Cancelled,
}

impl Outcome {
    /// Convenience for the common "here are all of them" case.
    pub fn answered(count: usize) -> Outcome {
        Outcome::Answered { returned: count, total: count }
    }

    /// True when results were withheld from the client.
    pub fn is_truncated(&self) -> bool {
        matches!(self, Outcome::Answered { returned, total } if returned < total)
    }

    /// True when the server told the client "nothing" while not actually
    /// knowing. This is the number that matters most in [`Health`].
    pub fn is_misleading(&self) -> bool {
        matches!(self, Outcome::Empty { reason } if !reason.is_healthy())
    }
}

/// Why an operation produced nothing.
///
/// Only [`EmptyReason::NoMatch`] is a real answer. The others are the server
/// not knowing, which the client cannot distinguish on the wire and so must be
/// counted separately here.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EmptyReason {
    /// Searched, found nothing. The only healthy empty.
    NoMatch,
    /// The shallow global index was still building.
    IndexNotReady,
    /// The index is loaded, but was built from source older than what is on
    /// disk now — so a symbol the client just wrote is genuinely absent from it.
    ///
    /// Distinct from [`EmptyReason::NoMatch`] precisely because an agent's
    /// first move after writing a file is to ask about what it wrote.
    IndexStale,
    /// The answer lives in a target that was not loaded.
    TargetNotLoaded,
    /// The file is not in any target and was never indexed.
    FileNotIndexed,
    /// The path is one the server does not handle — a jar entry it cannot read,
    /// or a scheme it does not know.
    UnsupportedPath,
}

impl EmptyReason {
    pub const ALL: [EmptyReason; 6] = [
        EmptyReason::NoMatch,
        EmptyReason::IndexNotReady,
        EmptyReason::IndexStale,
        EmptyReason::TargetNotLoaded,
        EmptyReason::FileNotIndexed,
        EmptyReason::UnsupportedPath,
    ];

    /// Whether an empty result for this reason was a truthful answer.
    pub fn is_healthy(self) -> bool {
        matches!(self, EmptyReason::NoMatch)
    }

    fn slot(self) -> usize {
        match self {
            EmptyReason::NoMatch => 0,
            EmptyReason::IndexNotReady => 1,
            EmptyReason::IndexStale => 2,
            EmptyReason::TargetNotLoaded => 3,
            EmptyReason::FileNotIndexed => 4,
            EmptyReason::UnsupportedPath => 5,
        }
    }
}

/// Why an operation could not be completed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Failure {
    /// The build server did not answer, or answered with an error.
    BuildServer,
    /// A file could not be read.
    Io,
    /// A jar or jimage entry could not be decoded.
    ArchiveDecode,
    /// A handler panicked and was caught.
    Panic,
    /// The request named a position or path that does not exist.
    BadRequest,
    /// The index needed to answer was not loaded, or was stale.
    ///
    /// Distinct from [`EmptyReason::IndexNotReady`]: that one records having
    /// *answered* empty while not knowing, which is the damaging case. This one
    /// records having refused, which is the correct behaviour and still worth
    /// counting — a client asking before the index is ready is a startup
    /// ordering problem, not a lie.
    IndexUnavailable,
    /// Something else, deliberately coarse so the summary stays path-free.
    Internal,
}

/// An assumption the server is supposed to uphold, checked at runtime.
///
/// These fire on the classes of bug that produce plausible-looking wrong
/// answers: a range that is off by a character on a non-ASCII file, a location
/// pointing into a file the VFS has since changed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Invariant {
    /// A byte offset landed inside a multi-byte character. The offset is wrong,
    /// and every column derived from it will be too.
    OffsetNotOnCharBoundary,
    /// A range extended past the end of the file it names.
    OffsetOutOfBounds,
    /// A range whose start came after its end.
    RangeInverted,
    /// A result referenced a file the VFS does not know.
    UnknownFile,
    /// A response claimed to return more results than exist.
    ReturnedExceedsTotal,
    /// An answer was produced while the VFS still held undrained writes, so it
    /// was computed from a state the client had already moved past.
    StaleRead,
}

/// One completed operation.
#[derive(Clone, Debug)]
pub struct QueryRecord {
    pub op: Op,
    pub outcome: Outcome,
    pub duration: Duration,
    /// The VFS revision the answer was computed from.
    pub vfs_revision: u64,
    /// Whether the VFS held undrained writes when the answer was produced. True
    /// means the answer predates something the client already did.
    pub stale: bool,
}

impl QueryRecord {
    pub fn new(op: Op, outcome: Outcome, duration: Duration) -> QueryRecord {
        QueryRecord { op, outcome, duration, vfs_revision: 0, stale: false }
    }

    pub fn at_revision(mut self, revision: u64) -> QueryRecord {
        self.vfs_revision = revision;
        self
    }

    /// Marks the answer as computed while writes were still pending.
    pub fn stale(mut self, stale: bool) -> QueryRecord {
        self.stale = stale;
        self
    }
}

/// Times an operation and records it on drop unless defused.
///
/// Recording on drop is deliberate: a handler that panics or returns early
/// still produces a record, so a spike in unrecorded work cannot hide a bug.
pub struct InFlight<'a> {
    telemetry: &'a Telemetry,
    op: Op,
    started: Instant,
    revision: u64,
    stale: bool,
    outcome: Option<Outcome>,
}

impl<'a> InFlight<'a> {
    /// Finishes with an explicit outcome.
    pub fn finish(mut self, outcome: Outcome) {
        self.outcome = Some(outcome);
    }

    /// Notes the VFS revision this answer is being computed from.
    pub fn at_revision(&mut self, revision: u64) {
        self.revision = revision;
    }

    /// Marks the answer as computed while writes were still pending, so it
    /// predates something the client has already done.
    ///
    /// The guard-based flow is how every handler reports, so without this the
    /// stale-read signal in [`Health`] could never fire — it would exist only
    /// for callers building a [`QueryRecord`] by hand, which nothing does.
    pub fn mark_stale(&mut self, stale: bool) {
        self.stale = stale;
    }

    /// Records a failure without consuming the guard.
    ///
    /// For the error paths that return early: `finish` takes `self`, which a
    /// `?` or an early `return` inside a closure cannot always satisfy.
    pub fn mark_failed(&mut self, failure: Failure) {
        self.outcome = Some(Outcome::Failed { failure });
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        // No explicit outcome means the handler unwound or returned without
        // saying what happened. That is itself a fault worth counting.
        let outcome = self.outcome.take().unwrap_or(Outcome::Failed { failure: Failure::Panic });
        let record = QueryRecord {
            op: self.op,
            outcome,
            duration: self.started.elapsed(),
            vfs_revision: self.revision,
            stale: self.stale,
        };
        self.telemetry.record(record);
    }
}

const SAMPLE_WINDOW: usize = 8192;

#[derive(Default)]
struct OpStats {
    total: u64,
    answered: u64,
    truncated: u64,
    withheld: u64,
    empty: [u64; 6],
    failed: FxHashMap<Failure, u64>,
    cancelled: u64,
    stale: u64,
    /// Most recent durations in microseconds, as a ring. Percentiles describe
    /// this window rather than all time, which is what you want when asking
    /// whether the server is misbehaving *now*.
    samples: Vec<u32>,
    next: usize,
}

impl OpStats {
    fn push_sample(&mut self, micros: u32) {
        if self.samples.len() < SAMPLE_WINDOW {
            self.samples.push(micros);
        } else {
            self.samples[self.next] = micros;
            self.next = (self.next + 1) % SAMPLE_WINDOW;
        }
    }

    fn percentiles(&self) -> (u32, u32, u32) {
        if self.samples.is_empty() {
            return (0, 0, 0);
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let at = |p: f64| {
            // Nearest-rank, clamped so p100 cannot index past the end.
            let rank = (p * sorted.len() as f64).ceil() as usize;
            sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
        };
        (at(0.50), at(0.95), at(0.99))
    }
}

#[derive(Default)]
struct Inner {
    ops: FxHashMap<Op, OpStats>,
    violations: FxHashMap<Invariant, usize>,
}

/// Aggregates operation outcomes in memory.
///
/// Cheap enough to call on every request: one mutex acquisition and a few
/// counter bumps, against work measured in milliseconds.
pub struct Telemetry {
    inner: Mutex<Inner>,
    started: Instant,
}

impl Default for Telemetry {
    fn default() -> Telemetry {
        Telemetry::new()
    }
}

impl Telemetry {
    pub fn new() -> Telemetry {
        Telemetry { inner: Mutex::new(Inner::default()), started: Instant::now() }
    }

    /// Starts timing an operation. The returned guard records on drop.
    pub fn start(&self, op: Op) -> InFlight<'_> {
        InFlight {
            telemetry: self,
            op,
            started: Instant::now(),
            revision: 0,
            stale: false,
            outcome: None,
        }
    }

    /// Records a completed operation.
    pub fn record(&self, record: QueryRecord) {
        let micros = record.duration.as_micros().min(u32::MAX as u128) as u32;

        // Emitted before aggregating so the raw stream is available under
        // RUST_LOG even if the summary is never read. This event may carry more
        // detail than `Health` does; see the module docs on what may leave.
        tracing::debug!(
            op = %record.op,
            outcome = ?record.outcome,
            micros,
            revision = record.vfs_revision,
            stale = record.stale,
            "query",
        );
        if record.outcome.is_misleading() {
            tracing::warn!(
                op = %record.op,
                outcome = ?record.outcome,
                "answered empty without knowing; the client cannot tell this from a real absence",
            );
        }

        let mut inner = self.lock();
        let stats = inner.ops.entry(record.op).or_default();
        stats.total += 1;
        stats.push_sample(micros);
        if record.stale {
            stats.stale += 1;
        }
        match record.outcome {
            Outcome::Answered { returned, total } => {
                stats.answered += 1;
                if returned < total {
                    stats.truncated += 1;
                    stats.withheld += (total - returned) as u64;
                }
            }
            Outcome::Empty { reason } => stats.empty[reason.slot()] += 1,
            Outcome::Failed { failure } => *stats.failed.entry(failure).or_default() += 1,
            Outcome::Cancelled => stats.cancelled += 1,
        }
    }

    /// Records a broken assumption. Never panics: a language server that dies
    /// on a bad range is worse than one that reports it and carries on.
    pub fn record_violation(&self, invariant: Invariant) {
        tracing::warn!(?invariant, "invariant violated");
        *self.lock().violations.entry(invariant).or_default() += 1;
    }

    /// Checks a byte range against the text it indexes, recording any problem.
    ///
    /// Returns whether the range was sound, so a caller can decline to send a
    /// result it cannot vouch for.
    pub fn check_span(&self, text: &str, start: usize, end: usize) -> bool {
        if let Err(violation) = check_span(text, start, end) {
            self.record_violation(violation);
            return false;
        }
        true
    }

    /// A point-in-time summary. Contains no paths, symbol names or file
    /// contents, and is safe to hand to someone outside this machine.
    pub fn health(&self) -> Health {
        let inner = self.lock();
        let mut ops: Vec<OpHealth> = inner
            .ops
            .iter()
            .map(|(&op, stats)| {
                let (p50, p95, p99) = stats.percentiles();
                let misleading: u64 = EmptyReason::ALL
                    .iter()
                    .filter(|r| !r.is_healthy())
                    .map(|r| stats.empty[r.slot()])
                    .sum();
                OpHealth {
                    op,
                    total: stats.total,
                    answered: stats.answered,
                    empty_no_match: stats.empty[EmptyReason::NoMatch.slot()],
                    misleading_empty: misleading,
                    truncated: stats.truncated,
                    results_withheld: stats.withheld,
                    failed: stats.failed.values().sum(),
                    cancelled: stats.cancelled,
                    stale_reads: stats.stale,
                    p50_micros: p50,
                    p95_micros: p95,
                    p99_micros: p99,
                }
            })
            .collect();
        ops.sort_by_key(|it| it.op);

        let mut violations: Vec<ViolationCount> = inner
            .violations
            .iter()
            .map(|(&invariant, &count)| ViolationCount { invariant, count })
            .collect();
        violations.sort_by_key(|it| it.invariant);

        let concerns = detect_concerns(&ops, &violations);
        Health {
            uptime_millis: self.started.elapsed().as_millis() as u64,
            ops,
            violations,
            concerns,
        }
    }

    /// Recovers from a poisoned lock rather than propagating the panic. The
    /// counters are independent, so a torn update loses a count and nothing
    /// worse — cheaper than taking the server down with it.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Validates a byte range against the text it indexes.
///
/// The boundary check is the one that catches encoding bugs: on a file like the
/// fixture's `Messages.java`, a UTF-16 offset used as a byte offset lands in the
/// middle of a character and every column after it is wrong.
pub fn check_span(text: &str, start: usize, end: usize) -> Result<(), Invariant> {
    if start > end {
        return Err(Invariant::RangeInverted);
    }
    if end > text.len() {
        return Err(Invariant::OffsetOutOfBounds);
    }
    if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return Err(Invariant::OffsetNotOnCharBoundary);
    }
    Ok(())
}

/// Per-operation counters and latencies. No user data.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpHealth {
    pub op: Op,
    pub total: u64,
    pub answered: u64,
    /// Empty answers that were truthful.
    pub empty_no_match: u64,
    /// Empty answers the client will read as "does not exist".
    pub misleading_empty: u64,
    pub truncated: u64,
    /// Total results trimmed away across all truncated responses.
    pub results_withheld: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub stale_reads: u64,
    pub p50_micros: u32,
    pub p95_micros: u32,
    pub p99_micros: u32,
}

impl OpHealth {
    /// Share of answers that told the client "nothing" without knowing.
    pub fn misleading_empty_rate(&self) -> f64 {
        if self.total == 0 { 0.0 } else { self.misleading_empty as f64 / self.total as f64 }
    }

    pub fn failure_rate(&self) -> f64 {
        if self.total == 0 { 0.0 } else { self.failed as f64 / self.total as f64 }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViolationCount {
    pub invariant: Invariant,
    pub count: usize,
}

/// A detected problem, rather than a number to interpret.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Concern {
    /// The server is telling clients that things do not exist when it simply
    /// has not looked. The most damaging failure mode available to it.
    MisleadingEmpties { op: Op, count: u64, rate: f64 },
    /// Answers were computed from state the client had already moved past.
    StaleReads { op: Op, count: u64 },
    /// A range or reference did not survive validation.
    BrokenInvariant { invariant: Invariant, count: usize },
    /// Results are being withheld often enough that clients are probably acting
    /// on partial data.
    HeavyTruncation { op: Op, rate: f64, withheld: u64 },
    /// The operation is failing outright.
    Failing { op: Op, rate: f64 },
}

impl Concern {
    /// Lower sorts first. Ranked by how badly the client is misled, not by how
    /// loud the symptom is: a wrong answer outranks an outright failure,
    /// because a failure is at least visible to the client.
    pub fn severity(&self) -> u8 {
        match self {
            Concern::MisleadingEmpties { .. } => 0,
            Concern::BrokenInvariant { .. } => 1,
            Concern::StaleReads { .. } => 2,
            Concern::HeavyTruncation { .. } => 3,
            Concern::Failing { .. } => 4,
        }
    }
}

/// Thresholds above which a rate stops being noise. Deliberately low for the
/// correctness signals — one misleading empty per thousand is still an agent
/// being told something untrue.
const MISLEADING_EMPTY_THRESHOLD: f64 = 0.001;
const FAILURE_THRESHOLD: f64 = 0.05;
const TRUNCATION_THRESHOLD: f64 = 0.25;

fn detect_concerns(ops: &[OpHealth], violations: &[ViolationCount]) -> Vec<Concern> {
    let mut concerns = Vec::new();

    for op in ops {
        // Any misleading empty on a client-facing operation is worth surfacing;
        // internal steps get the rate threshold, since a not-yet-built index
        // legitimately reports IndexNotReady during startup.
        let rate = op.misleading_empty_rate();
        let worth_reporting = if op.op.is_client_facing() {
            op.misleading_empty > 0
        } else {
            rate > MISLEADING_EMPTY_THRESHOLD
        };
        if worth_reporting {
            concerns.push(Concern::MisleadingEmpties {
                op: op.op,
                count: op.misleading_empty,
                rate,
            });
        }
        if op.stale_reads > 0 {
            concerns.push(Concern::StaleReads { op: op.op, count: op.stale_reads });
        }
        if op.failure_rate() > FAILURE_THRESHOLD {
            concerns.push(Concern::Failing { op: op.op, rate: op.failure_rate() });
        }
        if op.answered > 0 {
            let truncation_rate = op.truncated as f64 / op.answered as f64;
            if truncation_rate > TRUNCATION_THRESHOLD {
                concerns.push(Concern::HeavyTruncation {
                    op: op.op,
                    rate: truncation_rate,
                    withheld: op.results_withheld,
                });
            }
        }
    }

    for v in violations {
        concerns.push(Concern::BrokenInvariant { invariant: v.invariant, count: v.count });
    }

    concerns.sort_by_key(|it| it.severity());
    concerns
}

/// A path-free, shareable summary of how the server is behaving.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub uptime_millis: u64,
    pub ops: Vec<OpHealth>,
    pub violations: Vec<ViolationCount>,
    /// Detected problems, worst first. Empty means nothing tripped a threshold.
    pub concerns: Vec<Concern>,
}

impl Health {
    /// Whether anything looks wrong.
    pub fn is_healthy(&self) -> bool {
        self.concerns.is_empty()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("Health contains no unserializable values")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn healthy_server_reports_no_concerns() {
        let t = Telemetry::new();
        for _ in 0..100 {
            t.record(QueryRecord::new(Op::GoToDefinition, Outcome::answered(1), ms(2)));
        }
        t.record(QueryRecord::new(
            Op::WorkspaceSymbol,
            Outcome::Empty { reason: EmptyReason::NoMatch },
            ms(1),
        ));

        let health = t.health();
        assert!(health.is_healthy(), "unexpected concerns: {:?}", health.concerns);
        assert_eq!(health.ops.len(), 2);
    }

    #[test]
    fn a_single_misleading_empty_is_reported() {
        // The whole point. One "no results" that really meant "I had not
        // finished indexing" is enough to make an agent delete live code.
        let t = Telemetry::new();
        for _ in 0..999 {
            t.record(QueryRecord::new(Op::WorkspaceSymbol, Outcome::answered(3), ms(1)));
        }
        t.record(QueryRecord::new(
            Op::WorkspaceSymbol,
            Outcome::Empty { reason: EmptyReason::IndexNotReady },
            ms(1),
        ));

        let health = t.health();
        assert!(!health.is_healthy());
        let Concern::MisleadingEmpties { op, count, .. } = &health.concerns[0] else {
            panic!("expected misleading empties first, got {:?}", health.concerns);
        };
        assert_eq!((*op, *count), (Op::WorkspaceSymbol, 1));
    }

    #[test]
    fn honest_empties_are_not_concerns() {
        let t = Telemetry::new();
        for _ in 0..50 {
            t.record(QueryRecord::new(
                Op::FindReferences,
                Outcome::Empty { reason: EmptyReason::NoMatch },
                ms(1),
            ));
        }
        let health = t.health();
        assert!(health.is_healthy(), "{:?}", health.concerns);
        assert_eq!(health.ops[0].empty_no_match, 50);
        assert_eq!(health.ops[0].misleading_empty, 0);
    }

    #[test]
    fn concerns_rank_wrong_answers_above_failures() {
        // A failure is visible to the client and it can retry. A confident
        // wrong answer is not, so it has to sort first.
        let t = Telemetry::new();
        for _ in 0..10 {
            t.record(QueryRecord::new(
                Op::Hover,
                Outcome::Failed { failure: Failure::BuildServer },
                ms(1),
            ));
        }
        t.record(QueryRecord::new(
            Op::GoToDefinition,
            Outcome::Empty { reason: EmptyReason::TargetNotLoaded },
            ms(1),
        ));

        let concerns = t.health().concerns;
        assert!(matches!(concerns[0], Concern::MisleadingEmpties { .. }), "{concerns:?}");
        assert!(concerns.iter().any(|c| matches!(c, Concern::Failing { .. })));
    }

    #[test]
    fn truncation_is_counted_and_surfaced() {
        let t = Telemetry::new();
        for _ in 0..10 {
            t.record(QueryRecord::new(
                Op::FindReferences,
                Outcome::Answered { returned: 20, total: 5000 },
                ms(5),
            ));
        }
        let health = t.health();
        assert_eq!(health.ops[0].truncated, 10);
        assert_eq!(health.ops[0].results_withheld, 49_800);
        assert!(matches!(
            health.concerns.iter().find(|c| matches!(c, Concern::HeavyTruncation { .. })),
            Some(Concern::HeavyTruncation { withheld: 49_800, .. })
        ));
    }

    #[test]
    fn stale_reads_are_surfaced() {
        let t = Telemetry::new();
        t.record(
            QueryRecord::new(Op::FindReferences, Outcome::answered(2), ms(1))
                .at_revision(7)
                .stale(true),
        );
        let health = t.health();
        assert_eq!(health.ops[0].stale_reads, 1);
        assert!(health.concerns.iter().any(|c| matches!(c, Concern::StaleReads { count: 1, .. })));
    }

    #[test]
    fn unfinished_operations_count_as_faults() {
        // A handler that unwinds must not vanish from the record.
        let t = Telemetry::new();
        {
            let _guard = t.start(Op::Hover);
        }
        let health = t.health();
        assert_eq!(health.ops[0].failed, 1, "dropping without an outcome should record a fault");
    }

    #[test]
    fn in_flight_records_the_given_outcome() {
        let t = Telemetry::new();
        {
            let mut guard = t.start(Op::DocumentSymbol);
            guard.at_revision(42);
            guard.finish(Outcome::answered(9));
        }
        let health = t.health();
        assert_eq!(health.ops[0].answered, 1);
        assert_eq!(health.ops[0].failed, 0);
    }

    #[test]
    fn span_checks_catch_encoding_mistakes() {
        // Mirrors fixtures/megarepo Messages.java: the emoji is outside the BMP,
        // so a UTF-16 offset used as a byte offset lands mid-character.
        let text = "🔁 retrying…";
        assert_eq!(check_span(text, 0, 4), Ok(()), "4 bytes is the whole emoji");
        assert_eq!(check_span(text, 0, 2), Err(Invariant::OffsetNotOnCharBoundary));
        assert_eq!(check_span(text, 0, text.len() + 1), Err(Invariant::OffsetOutOfBounds));
        assert_eq!(check_span(text, 5, 4), Err(Invariant::RangeInverted));
        assert_eq!(check_span("", 0, 0), Ok(()), "an empty file has one valid empty span");
    }

    #[test]
    fn checked_spans_are_recorded_and_do_not_panic() {
        let t = Telemetry::new();
        assert!(t.check_span("héllo", 0, 1));
        assert!(!t.check_span("héllo", 0, 2), "byte 2 splits the é");

        let health = t.health();
        assert_eq!(health.violations.len(), 1);
        assert_eq!(health.violations[0].invariant, Invariant::OffsetNotOnCharBoundary);
        assert!(matches!(health.concerns[0], Concern::BrokenInvariant { count: 1, .. }));
    }

    #[test]
    fn percentiles_are_ordered_and_survive_one_sample() {
        let t = Telemetry::new();
        t.record(QueryRecord::new(Op::Hover, Outcome::answered(1), ms(7)));
        let one = &t.health().ops[0];
        assert_eq!((one.p50_micros, one.p95_micros, one.p99_micros), (7_000, 7_000, 7_000));

        let t = Telemetry::new();
        for i in 1..=100u64 {
            t.record(QueryRecord::new(Op::Hover, Outcome::answered(1), ms(i)));
        }
        let many = &t.health().ops[0];
        assert!(many.p50_micros <= many.p95_micros && many.p95_micros <= many.p99_micros);
        assert_eq!(many.p50_micros, 50_000);
        assert_eq!(many.p99_micros, 99_000);
    }

    #[test]
    fn sample_window_is_bounded() {
        let t = Telemetry::new();
        for _ in 0..(SAMPLE_WINDOW * 2 + 5) {
            t.record(QueryRecord::new(Op::Hover, Outcome::answered(1), ms(1)));
        }
        let health = t.health();
        assert_eq!(health.ops[0].total as usize, SAMPLE_WINDOW * 2 + 5, "counts stay exact");
        assert_eq!(t.lock().ops[&Op::Hover].samples.len(), SAMPLE_WINDOW, "samples stay bounded");
    }

    #[test]
    fn summary_carries_no_user_data() {
        // Health is the shareable half. If a path or symbol name can reach it,
        // the boundary described in the module docs has been broken.
        let t = Telemetry::new();
        t.record(QueryRecord::new(Op::WorkspaceSymbol, Outcome::answered(3), ms(1)));
        t.check_span("class Grüße {}", 0, 8);

        let json = t.health().to_json();
        assert!(!json.contains("Grüße"), "file contents leaked into the summary: {json}");
        assert!(!json.contains('/'), "a path may have leaked into the summary: {json}");
        assert!(json.contains("workspaceSymbol"));
    }

    #[test]
    fn cancelled_work_is_not_a_fault() {
        let t = Telemetry::new();
        for _ in 0..20 {
            t.record(QueryRecord::new(Op::FindReferences, Outcome::Cancelled, ms(1)));
        }
        let health = t.health();
        assert_eq!(health.ops[0].cancelled, 20);
        assert!(health.is_healthy(), "{:?}", health.concerns);
    }

    #[test]
    fn index_not_ready_is_tolerated_on_internal_steps() {
        // The index legitimately reports "not ready" while it builds. That must
        // not drown out the client-facing signal it would be on a real query.
        let t = Telemetry::new();
        t.record(QueryRecord::new(
            Op::IndexBuild,
            Outcome::Empty { reason: EmptyReason::IndexNotReady },
            ms(1),
        ));
        for _ in 0..2000 {
            t.record(QueryRecord::new(Op::IndexBuild, Outcome::answered(1), ms(1)));
        }
        assert!(t.health().is_healthy(), "{:?}", t.health().concerns);
    }

    #[test]
    fn a_guard_can_report_a_stale_read() {
        // The gap this closes: the guard flow is how every handler reports, so
        // without mark_stale the stale-read signal could never fire in practice.
        let t = Telemetry::new();
        {
            let mut guard = t.start(Op::WorkspaceSymbol);
            guard.at_revision(9);
            guard.mark_stale(true);
            guard.finish(Outcome::answered(1));
        }
        let health = t.health();
        assert_eq!(health.ops[0].stale_reads, 1);
        assert!(health.concerns.iter().any(|c| matches!(c, Concern::StaleReads { .. })));
    }

    #[test]
    fn a_guard_can_report_a_failure_without_being_consumed() {
        let t = Telemetry::new();
        {
            let mut guard = t.start(Op::IndexBuild);
            guard.mark_failed(Failure::Io);
        }
        assert_eq!(t.health().ops[0].failed, 1);
    }

    #[test]
    fn a_stale_index_is_not_a_healthy_empty() {
        // An agent writes a file, then asks about it. "Not in the index" is not
        // "does not exist", and must not be counted as a truthful answer.
        assert!(!EmptyReason::IndexStale.is_healthy());
        let t = Telemetry::new();
        t.record(QueryRecord::new(
            Op::WorkspaceSymbol,
            Outcome::Empty { reason: EmptyReason::IndexStale },
            ms(1),
        ));
        assert_eq!(t.health().ops[0].misleading_empty, 1);
    }
}
