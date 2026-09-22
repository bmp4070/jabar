# Monolith startup and memory measurement plan

This protocol measures whether the built-index cache makes Jabar usable on a
large Java monolith without moving the cost into memory or background reloads.
It is designed for the reported Salesforce-core scale of 2.68M files under
`bazel-bin`, 6,876 SCIP shards, and 3.13M definitions, but does not depend on
publishing repository paths or symbol names.

The first run establishes a baseline. Results must include raw samples and the
exact Jabar revision; a single stopwatch result is not evidence for a latency or
memory claim.

## Questions to answer

1. How long does a cache hit take from process spawn to the `initialize`
   response and to the first correct LSP answer?
2. How much time is spent validating metadata, reading and decoding the cache,
   scanning `bazel-bin`, decoding SCIP, building lookup maps, starting the
   watcher, reconciling shards, and writing a cache generation?
3. What are steady and peak resident memory for a cache hit, a cache miss, cache
   publication, an unchanged reconciliation, and a changed-shard reload?
4. During a reload, how much memory is consumed while the old and new indexes
   coexist, how quickly is the retired index reclaimed, and how long does the
   LSP event loop stop answering?
5. Do explicit `jabar/loadIndex` requests and cache publication remain
   responsive and memory-bounded when work is superseded?
6. Does a cache-loaded index return the same answers and counts as a freshly
   built index?

## Benchmark inputs

Run against the checked-in fixture and one representative monolith. Pin and
record:

- release-build Jabar binary hash and Jabar, scip-java, and aspect revisions;
- repository HEAD, dirty/clean state, target patterns, and Bazel output base;
- Bazel, JDK, OS, kernel, filesystem, and architecture versions;
- CPU model/core count, physical RAM, storage type, and available disk space;
- power mode and whether the host was otherwise idle;
- shard count and bytes, definitions, references, occurrences, indexed target
  count, cache bytes, and coverage state.

The end-to-end corpus must preserve the condition this optimization addresses.
Use a frozen real output tree, or a preserved/synthetic tree with the same
non-SCIP entry count, directory fan-out, symlinks, and `.scip-targetroot` and
`.semanticdb` exclusions. Copying only the 6,876 shards removes the 2.68M-entry
walk and is valid only for separately labelled decode/format microbenchmarks.
Do not race an active Bazel build unless that race is the workload under test.

Keep full reproducibility metadata privately, including HEAD, paths, target
patterns, and machine identity. Public artifacts use an opaque dataset ID and
an allowlist of aggregate fields defined below. Do not publish proprietary
paths, symbol names, source, raw protocol responses, or unsanitized commands.

## Required instrumentation

Add structured timing events around these boundaries before collecting the
monolith results:

| Event | Starts | Ends |
| --- | --- | --- |
| `startup.total` | process entry | first correct query response |
| `initialize.total` | initialize request received | initialize response sent |
| `cache.key` | cache-key construction | workspace/HEAD/output-tree key ready |
| `cache.read` | `CURRENT` open | checksum verified and `SymbolIndex` decoded |
| `shards.scan` | recursive scan begins | sorted shard metadata ready |
| `shards.decode` | first shard read | complete lookup index ready |
| `watcher.start` | watcher construction begins | watcher ready or failed |
| `cache.write` | snapshot serialization begins | generation published or abandoned |
| `reconcile.scan` | background scan begins | metadata comparison completes |
| `reload.build` | changed shards begin loading | replacement index ready |
| `reload.swap` | result reaches the event loop | replacement becomes queryable |
| `explicit_load.build` | `jabar/loadIndex` is accepted | index and watcher are ready or the request fails |
| `index.reclaim` | an index is retired or rejected | its destructor completes on the reclamation worker |
| `cache.queue` | a generation is offered for publication | its write starts or it is superseded |
| `event_loop.heartbeat` | scheduled benchmark heartbeat | event loop handles it |

Each event records a run ID, monotonic duration, outcome, cache hit/miss,
generation, shard/definition/reference/occurrence counts, and bytes where
applicable. It must not record source paths or symbol strings. The benchmark
client also timestamps process spawn, initialize request/response, initialized
notification, first `jabar/status`, and the first fixed `workspace/symbol`
response. This separates server work from client and process-launch overhead.

The heartbeat runs only in benchmark instrumentation, on a monotonic 20ms
schedule. Record the intended deadline, handling time, and scheduling lateness;
never silently discard missed deadlines. A long stall may encode consecutive
misses as a lossless run-length record, but analysis must expand every scheduled
deadline when calculating percentiles and maxima. Measure the instrumentation
overhead against a build without the heartbeat before using it for a gate.

Build a small benchmark client under `tools/` that speaks LSP over stdio and
writes one JSON object per run. It should fail a run if the expected capability,
status fields, result count, or sentinel symbol is missing. Parsing logs by hand
is reserved for diagnosis rather than the reported measurement path. Run Jabar
with `index.auto=false` for cache-load measurements so a Bazel child cannot be
mistaken for server startup work; measure automatic indexing separately.

## Startup workloads

Keep these cache states separate in results:

| Workload | Preparation | What it measures |
| --- | --- | --- |
| Cache miss, storage warm | Remove only `.jabar/index/cache`; leave shards and OS page cache intact | Tree walk, SCIP decode, map construction, and asynchronous cache publication |
| Cache hit, storage warm | Start from a verified published generation; restart only Jabar | Normal editor restart |
| Cache hit, storage cold | Use a dedicated host reboot or an explicitly documented safe page-cache reset | Snapshot I/O without warm filesystem pages |
| Invalid cache | Corrupt/truncate a copied generation or change its version/key | Validation and shard fallback |
| Configuration miss | Change target patterns or output base | Key invalidation and correct fallback |

Never describe a process restart as a cold-storage run. Do not drop filesystem
caches on a shared developer machine. Run the correctness query, then keep the
process alive through its initial reconciliation, any cache publication, and a
further 30-second quiescent interval. Record 30 seconds after the first query
and 30 seconds after background work as separate memory points. A run stopped
before background completion is incomplete and cannot contribute a process
peak. Set a background-completion timeout before counted runs from an untimed
pilot (twice its duration, capped at 30 minutes); record a timeout as a failed
run instead of silently shortening its lifetime.

Collect at least 30 cache-hit samples for each storage state and report p50,
p95, maximum, median absolute deviation, and every raw sample. Use and name one
percentile estimator consistently (nearest-rank is the default here, making p95
the 29th ordered value at n=30) and report a bootstrap confidence interval.
Thirty samples are a screening run, not a stable tail characterization; collect
at least 100 if the confidence interval crosses a gate or the result is within
10% of one. Collect at least 10 expensive cache-miss samples and report p50,
range, and raw samples rather than a p95. Alternate A/B cache-format runs instead
of running every sample for one format first, so thermal and machine drift do
not favor one format. Do not remove outliers; annotate externally explained
events and show the result with and without them.

## Refresh and responsiveness workloads

Run each scenario at least ten times. Begin scenarios 1–8 and 10–12 from a
cache hit; scenario 9 explicitly uses a complete uncached/manual index:

1. An unchanged periodic reconciliation.
2. One modified shard with the same HEAD.
3. A representative incremental aspect build touching many shards.
4. Removal of the final shard, followed by restoration.
5. A branch/HEAD change while reconciliation is running.
6. Two shard notifications while a reload is already running.
7. A corrupt shard and a corrupt cache generation.
8. An injected watcher overflow/error with unchanged HEAD and shards; verify the
   current index remains queryable while it moves from unverified back to
   verified.
9. A branch/HEAD change with an uncached index; wait through at least one
   periodic interval and verify old shards are not reloaded until a subsequent
   shard event supplies evidence of a new build.
10. An explicit `jabar/loadIndex` of a complete manual index while probes
    continue; verify the request remains pending until the replacement and its
    watcher are ready, then verify the replacement is queryable.
11. An explicit or watcher-triggered full generation that becomes stale before
    completion; verify the old index remains queryable, the stale generation is
    rejected, and reclamation does not pause the event loop.
12. At least three successive changed generations while cache output is
    deliberately throttled; verify there is at most one active cache write and
    one latest pending write, superseded serialization stops promptly, and the
    final published cache matches the queryable generation.

While each scenario runs, use an open-loop client to schedule a lightweight
`jabar/status` request every 50ms, a fixed definition request every second, and a
fixed symbol request every second. Begin probes at least five seconds before the
change and continue through refresh completion (or timeout) and 30 seconds of
quiescence. Record intended and actual send time, receive time,
outstanding-request count, timeout, errors, stale/unverified state transitions,
refresh duration, and time until the new generation becomes queryable. Continue
sending on the original schedule when a response is late, and count every
timeout, so a stall cannot hide requests through coordinated omission. Request
latency measures the client-visible effect; the benchmark heartbeat measures
event-loop scheduling lateness and supplies the pause metric. Bound retained
request payloads to keep the harness from exhausting memory during a long stall;
every scheduled request that cannot be sent because of that bound is still
recorded as a timeout.

Calculate status, definition, and symbol latency distributions separately for
each run and each scenario. Report the per-run summaries and the aggregate for
each method within a scenario. Do not pool methods, scenarios, or runs when
deciding whether a gate passes: the higher status request rate and a long fast
run must not hide slow real queries or a slow refresh run.

## Peak and steady memory

Run the harness outside the measured process and sample the direct Jabar PID.
Use the operating system's high-water RSS counter for the authoritative Jabar
process-lifetime peak:

- Linux: `/usr/bin/time -v` (`Maximum resident set size`) and `/proc/<pid>` for
  direct-process samples. When available, also use a dedicated cgroup's
  `memory.current`, `memory.peak`, and `memory.events`.
- macOS: `/usr/bin/time -l` (`maximum resident set size`).

Also sample direct-process RSS every 100ms to correlate memory with phases.
Report the sampled maximum as a lower bound: it can miss a short peak. The
process high-water mark cannot be assigned to one phase when events overlap, so
report overlapping phase intervals instead of guessing its cause. Record the
sampler's monotonic clock beside the structured events. Report units explicitly
because Linux `time -v` reports KiB while macOS `time -l` reports bytes. Do not
combine numbers from different operating systems into one percentile series.

Cgroup `memory.current`/`memory.peak` includes charged page cache and descendant
processes and is not RSS. Label it **workload cgroup memory**, place only Jabar
and intentional children in that cgroup, and report it separately from direct
Jabar RSS. Automatic-indexing runs must identify Bazel/scip-java child memory
rather than attributing their cgroup total to the in-process symbol index.

For each workload report:

- RSS immediately before index load;
- RSS after the first correct query and after 30 seconds idle;
- peak RSS during cache decode or shard decode;
- peak RSS during cache serialization;
- pre-reload steady RSS, peak while old and new indexes coexist, and RSS 30
  seconds after the swap;
- peak and post-reclamation RSS for successful swaps and rejected stale
  generations;
- peak RSS during a burst of superseded cache writes, including active and
  pending generation counts;
- swap activity, OOM/cgroup events, and major page faults;
- bytes per definition and per occurrence, using both steady and peak RSS;
- snapshot bytes divided by aggregate SCIP bytes.

The allocator may retain memory after the old index is dropped, so the
post-swap value is a separate result rather than proof of a leak. If RSS remains
high, repeat the scenario under a heap profiler on a reduced but representative
shard set. Profiler runs are diagnostic and must not be mixed with uninstrumented
latency samples.

## Correctness comparison

For every cache format, compare a cache-hit process with a fresh shard-loaded
process using the same immutable inputs. Require equal shard, definition,
reference, and occurrence counts. Run a fixed corpus containing exact, prefix,
and substring symbol searches; definitions; high-fan-out references;
implementations; document symbols; and incoming/outgoing calls. Compare
normalized complete responses inside the private environment. Publish
per-operation parity, result counts, and mismatch counts. If durable private
comparison artifacts are required, use a keyed digest whose key and raw
responses are not published; a plain hash of predictable symbol data is not a
confidentiality boundary. A format with faster but different answers fails.

## Provisional acceptance gates

These gates apply to the named representative monolith. Before collecting the
first counted run, record the host GiB caps and, for containers/cgroups, the
effective memory limit. Later changes create a new revision of the criteria;
they do not replace the original pass/fail result:

- storage-warm cache-hit `initialize` p95 is under 10s and at least 5x faster
  than the cache-miss median;
- storage-warm cache-hit time from process spawn to the first correct query has
  p95 under 12s;
- an unchanged reconciliation does not increase steady RSS by more than 5%;
- cache-hit peak RSS is no more than 1.5x RSS measured after background work
  completes and the process is quiescent for 30 seconds;
- changed-shard reload peak RSS is no more than 2.25x pre-reload steady RSS;
- direct Jabar steady RSS is at most 25% and refresh peak RSS at most 40% of
  effective memory (the lower of host physical RAM and a cgroup/container
  limit), with no swap growth or OOM events;
- in every refresh scenario and counted run, the separate `jabar/status`,
  definition, and symbol response-latency p95 values are each under 250ms;
  `event_loop.heartbeat` scheduling-lateness p95 is under 100ms, and no
  heartbeat is more than 250ms late;
- responsiveness probes have no timeout or protocol error;
- cache and shard-built query results and index counts are identical;
- no cache corruption, stale worker result, or failed refresh replaces a valid
  generation.

Relative memory gates make comparisons possible before choosing a standard
developer host. Pre-registering GiB caps still matters because a ratio alone can
leave too little memory for Bazel and the editor.

## Result artifacts and completion checklist

Commit non-sensitive results under
`benchmarks/monolith/<date>-<jabar-revision>/`:

- `environment.json`: public schema containing opaque dataset/workload IDs,
  tool versions, aggregate tree/shard/index counts, hardware class, effective
  memory limit, and no paths or repository identifiers;
- `runs.jsonl`: timings, aggregate counts, memory values, outcomes, and opaque
  IDs only; raw protocol payloads remain in access-controlled storage;
- `summary.md`: tables for latency, memory, correctness, and failures;
- a sanitized benchmark client invocation and any instrumentation
  patch/configuration;
- charts generated from `runs.jsonl`, with units and sample counts on every
  axis or caption.

The measurement work is complete when:

- [ ] phase instrumentation and the LSP benchmark client are committed;
- [ ] fixture runs prove the harness catches wrong counts and corrupt caches;
- [ ] all startup and refresh workloads have the required samples;
- [ ] peak and steady RSS are captured with phase-correlated samples;
- [ ] the current whole-index cache is validated against the gates;
- [ ] correctness parity and index counts match the shard-loaded baseline;
- [ ] sanitized raw-run, aggregate, and environment artifacts are published;
- [ ] each acceptance gate is marked pass/fail with evidence;
- [ ] failures produce a follow-up issue and owner rather than being omitted.

A later format-selection experiment has its own prerequisite: implement a
value-snapshot prototype, then compare it with the accepted whole-index cache on
identical shards using the decode/format subset of this protocol. That experiment
does not block publishing measurements for the current implementation.

**Status:** planned. No representative-monolith latency or peak-memory results
have been collected for the current cache implementation.
