# Jabar and Metals v2 side-by-side evaluation plan

## Purpose

This plan compares Jabar with
[Metals v2](https://github.com/scalameta/metals/tree/main-v2) on the same large
Java Bazel checkout. It is designed to answer where each server is useful,
correct, fast, and operationally affordable. It must not turn differences in
feature scope or repository state into performance claims.

The comparison is Java-only. Metals also supports Scala and a broader LSP
surface; those capabilities are recorded as product differences rather than
scored as Java benchmark failures for Jabar.

The initial hypothesis is that Jabar may provide predictable repository-wide
navigation from compiler-produced SCIP shards, explicit readiness and result
accounting, and a simpler serving process. Metals v2 may provide much faster
initial source indexing, useful navigation before a successful build, richer
live-file semantics, and lower index storage. The measurements must be allowed
to reject either hypothesis.

## Questions

1. How soon can each server return a correct workspace symbol, definition, and
   reference answer from a clean checkout and from a warmed workspace?
2. How accurate and complete are those answers for ordinary source, generated
   source, annotation-processed code, external dependencies, and duplicate or
   overlapping build ownership?
3. What latency, CPU, I/O, disk, steady RSS, and peak RSS does each mode require?
4. What happens after an unsaved edit, save, Bazel rebuild, generated-source
   change, branch switch, incomplete build, and corrupted cache?
5. Can an automated caller distinguish complete, truncated, stale, partial, and
   unavailable answers?
6. Which results depend on build metadata, and which are available from source
   discovery alone?

## Revisions and disclosure

Pin every counted run to immutable revisions and retain the binaries used:

- Jabar commit, Rust toolchain, scip-java commit or release, and installed
  aspect digest;
- Metals `main-v2` commit, launcher artifact, JVM, and all command-line and
  workspace settings;
- repository commit and dirty state;
- Bazel/Bazelisk version, startup options, target patterns, output base, and
  relevant environment variables;
- operating system, kernel, architecture, CPU, physical and cgroup memory,
  filesystem, storage, and power settings.

The first reviewed Metals baseline was commit
`96bf7b01bc6d51823322830dd54c6ef8c5bddf4f`. Refresh that pin before executing
the study. The current Jabar cache format is version 3; the existing monolith
numbers are version-2 historical evidence and must not be reported as results
for the current comparison.

Use one frozen checkout and one host for both tools. Public results use an
opaque dataset name and aggregate values. Keep repository paths, symbol names,
source, raw LSP payloads, and proprietary build metadata private.

## State and cache matrix

Record and control these state dimensions independently. “Cold” without a
dimension is not a valid label.

| Dimension | Cold/reset state | Warm/preserved state |
| --- | --- | --- |
| Tool state | No Jabar built index or Metals MBT/Turbine/SemanticDB state | Current tool-native persistent state |
| Build outputs | No Bazel output tree or generated outputs | Outputs from the pinned successful build |
| Build/action cache | Empty local action cache; remote-cache policy fixed and disclosed | Existing local/remote action cache retained |
| Build daemon | `bazel shutdown` before the run; new daemon belongs to the measured cgroup | Existing daemon explicitly retained and attributed |
| Dependencies and launchers | Empty only in a separately labelled provisioning test | Preinstalled launchers, JDK, artifacts, and external repositories |
| OS storage cache | Dedicated reboot or documented safe page-cache reset | Page cache left warm |
| Process-local query state | New LSP process with no compiler/query caches in memory | Primed by the specified query workload |

The main clean-checkout track uses cold tool state, a disclosed build/output
state, warm provisioned dependencies, and warm storage unless its label says
otherwise. Run a separate full-provisioning experiment if download and tool
installation cost matters. Record preparation time and bytes outside the timed
LSP interval instead of hiding that work. Use benchmark-owned output bases and
cache directories for resettable state; never clear a shared developer or
remote cache to prepare a run.

## Comparison tracks

No single startup state is fair to both architectures. Report these tracks
separately.

### Track A: clean-checkout usefulness

Start from the same clean Git checkout with no Jabar SCIP shards/cache and no
Metals MBT index/cache.

Run two labelled variants. **A1 developer-warm-build** preserves provisioned
dependencies and the disclosed Bazel action/output-cache state while removing
tool indexes. **A2 cold-build** also removes build outputs and the local action
cache, shuts down Bazel, and applies the same declared remote-cache policy to
both tools. Neither variant clears the OS page cache unless it is separately
labelled as cold storage.

- Jabar runs with automatic indexing enabled and a predeclared target set.
- Metals runs with its normal build-free MBT source indexing. Do not perform a
  manual build sync before the readiness measurement.
- Record time to the first validated source-only answers.
- Continue until each applicable build-aware sentinel passes or the
  preregistered readiness deadline expires, recording each transition
  separately. Classify unsupported sentinels rather than waiting indefinitely.

This track measures onboarding and recovery. It is expected to exercise very
different work, so it does not isolate serving-engine performance.

### Track B: prepared steady-state navigation

Prepare each server's normal persistent artifacts from the same repository
revision:

- Jabar: a complete, freshly generated SCIP shard set and a published version-3
  built-index cache;
- Metals: a current MBT index plus explicitly recorded Turbine, SemanticDB,
  build metadata, and classpath state needed by the selected Bazel mode.

Restart only the language server between samples. Measure warm and cold-storage
variants separately. This is the main comparison for repeated editor and agent
sessions.

### Track C: edits and repository movement

Begin from the prepared state, then apply identical scripted changes. Include
open-buffer edits, saved edits, generated outputs, Bazel rebuilds, and Git
branch movement. Measure both response quality during the transition and time
to a new correct steady state.

## Readiness definitions

Record tool-native readiness, but use a shared definition for comparisons.

**Initial source intelligence** is reached when all required source-only
sentinels pass:

- non-empty exact or common-substring workspace-symbol result containing the
  expected declaration;
- go-to-definition to the expected workspace source location;
- references containing the minimum independently validated location set.

**Build-aware intelligence** is reached when the source sentinels plus generated
source, annotation-processor, external dependency, and target-boundary
sentinels pass.

**Stable readiness** additionally requires background work to quiesce for 30
seconds without changing the answers, emitting index/build progress, starting a
new child process, restarting a compiler, changing tracked persistent artifacts,
or publishing a new index generation. Also record CPU during this observation
window; set its idle threshold from a pilot before counted runs. If quiescence
does not occur by its deadline, classify it as an outcome rather than extending
the run silently.

Time starts immediately before process creation. Report at least:

- process to `initialize` response;
- process to first source intelligence;
- process to first build-aware intelligence;
- process to stable readiness.

Jabar's `jabar/status` and benchmark phase events may explain its transitions,
but cannot define the shared finish line. Metals progress notifications and
logs are treated the same way. The LSP probes are authoritative.

Use a readiness corpus that is disjoint from the counted query corpus so
readiness probes do not pre-warm measured symbols. Send readiness probes
serially at a fixed cadence with at most one in flight. Choose the readiness and
quiescence deadlines from untimed pilots before counted runs: twice the slowest
valid pilot, capped at 30 minutes unless the environment requires a documented
exception.

## Query corpus and correctness oracle

Freeze the corpus before counted runs. Do not select only queries that one
server already answers.

Use at least 100 Java symbols, stratified across:

- classes, interfaces, enums, constructors, methods, and fields;
- reference-count buckets: 0, 1–10, 11–100, 101–1,000, 1,001–5,000, and over
  5,000 so Jabar's response cap is exercised;
- same-package and cross-package relationships;
- cross-target and distant reverse dependencies;
- generated and annotation-processed sources;
- external/JDK symbols;
- overloaded, overridden, implemented, and duplicate-name declarations;
- non-ASCII source and identifiers;
- symbols affected by each edit/refresh scenario.

For each symbol, store expected declaration locations, a validated minimum
reference set, whether exhaustive references are known, and applicable
implementations or callers. Build the oracle from a fresh successful Bazel
build, source inspection, and independent adjudication. A Jabar SCIP result or
a Metals result alone cannot be its own ground truth.

Freeze the comparable source and symbol semantics with the corpus:

- the core universe is checked-in Java workspace source visible to both tools;
- generated source, dependency source, JDK symbols, and sources outside the
  selected Bazel target set are separate strata;
- constructor invocations and type references are scored separately;
- references to the exact declared method are the common override metric;
  override-family expansion is a separate capability result;
- a generated or dependency location counts only when that stratum is enabled
  and the client can open the returned URI;
- record any additional files indexed by one tool outside the comparable
  universe instead of treating the larger scope as either false positives or
  free recall.

Normalize responses before comparison:

- canonical workspace-relative paths;
- UTF-16 line and column positions;
- stable sorting and duplicate removal;
- definitions separated from references;
- standard LSP responses separated from tool-specific completeness metadata.

Preserve raw result count, duplicate count, invalid/unopenable location count,
and out-of-universe count before normalization. Normalization must not conceal a
server-side duplication or location-validity problem.

Record precision and recall where exhaustive ground truth exists. Else record
validated hits, false positives, known misses, returned count, claimed total,
timeouts, and whether the server disclosed incompleteness.

## Required workloads

| ID | Workload | Preparation | Required observations |
| --- | --- | --- | --- |
| A1 | Clean tool state, warm build state | Remove both tools' indexes; preserve disclosed Bazel/dependency state | Source and build-aware readiness, CPU, I/O, peak RSS, subprocesses, disk created |
| A2 | Clean tool and build state | Also clear outputs/local action cache and stop Bazel | Same as A1 plus build/index production cost |
| B1 | Warm restart | OS cache warm; tool cache current | Readiness phases and 30-second quiescent RSS |
| B2 | Cold-storage restart | Dedicated reboot or safe cache reset | Same as B1; never label a process restart as cold storage |
| B3 | Invalid cache | Corrupt a copied cache/index | Detection, fallback, answer correctness, recovery time |
| Q1 | Workspace symbols | Fixed exact/prefix/fuzzy corpus | p50/p95/p99/max latency, ranking, recall, cap disclosure |
| Q2 | Definitions | Fixed source positions | Latency, correct destination set, stale-position behavior |
| Q3 | References | Stratified reference corpus | Latency, precision/recall, timeout/partial behavior, totals and truncation |
| Q4 | Implementations | Interface and override corpus | Latency and correct implementation set |
| Q5 | Call hierarchy | Common supported subset | Caller/callee accuracy; report unsupported semantic cases rather than scoring them silently |
| C1 | Unsaved edit | Change declarations and usages in open buffers | Time and correctness before save; stale-answer handling |
| C2 | Saved edit | Save the C1 changes without building | Answer transition and disclosure |
| C3 | Incremental build | Rebuild affected Bazel targets | Build contention and time to correct refreshed answers |
| C4 | Generated source | Change generator input and rebuild | Discovery and replacement correctness |
| C5 | Branch switch | Switch between commits with moved/deleted symbols | Old-answer rejection and convergence time |
| C6 | Broken state | Introduce syntax and build failures | Useful fallback, diagnostics if supported, navigation correctness |
| S1 | Sustained agent session | Mixed queries for 30 minutes | Tail latency, RSS growth, cache growth, failures, false empty answers |
| S2 | Concurrent clients/requests | Fixed bounded concurrency levels | Throughput, tail latency, queueing, memory, cancellation behavior |

Run the common nine navigation operations where both servers support equivalent
semantics. Record completion, diagnostics, signature help, semantic tokens,
testing, debugging, Scala, and MCP as Metals product capabilities outside the
scored common surface.

For every query workload, report two modes:

- **first touch:** the measured symbol is not used by readiness or priming;
  use one primary measured symbol per fresh LSP process so in-memory compiler
  and SemanticDB caches cannot carry between first-touch samples;
- **repeated:** run a fixed, disclosed primer, then issue the measured workload
  at a fixed arrival rate.

Record memory once at stable readiness and again after the same Q1–Q4 query mix
for both servers. This captures lazy allocations that startup-only memory misses.
General fuzzy or abbreviation search is a separate Q1 capability measurement;
shared readiness requires only matching exact/substring behavior.

## Tool modes

### Jabar

Measure at least:

1. `index.auto=true` from a clean checkout;
2. `index.auto=false` from fresh shards with no built-index cache;
3. `index.auto=false` from a verified version-3 cache.

Pin target patterns and output base. Preserve `jabar/status`, `jabar/references`,
and `JABAR_BENCH_LOG` output as diagnostic artifacts. Validate that loaded
shards represent the intended target set before treating reference totals as
repository totals.

### Metals v2

Measure at least:

1. clean MBT source indexing without prior build sync;
2. warm MBT startup without build sync;
3. warm MBT startup with the selected open-source Bazel metadata/import state;
4. any BSP-backed mode only as a separately labelled configuration.

Pin settings for Java symbol loading, generated sources, build import, compiler
timeouts, heap, garbage collector, and worker parallelism. Do not use
Databricks' private production BSP in results described as reproducible from
open source.

## Common harness

Extend or wrap `jabar-bench` with server adapters rather than copying its
Jabar-specific readiness assumptions into the Metals path. The common driver
must:

1. launch either command with a clean, recorded environment;
2. speak LSP over stdio and capture timestamps at the same boundaries;
3. answer server-to-client requests and retain progress notifications;
4. poll tool-specific status only for diagnostics;
5. run disjoint validated readiness and measurement corpora;
6. keep requests bounded in memory and preserve timeout/partial outcomes;
7. shut down cleanly and classify each request as correct, incorrect,
   partial-disclosed, partial-undisclosed, unavailable, unsupported, timeout, or
   protocol error;
8. emit one schema-versioned JSON record per run.

These classifications are measured outcomes and do not by themselves invalidate
a run. Mark a run invalid only for a harness failure, uncontrolled environment
change, or violated preparation protocol. Retain unsupported and unavailable
answers when calculating availability and time-to-correct-answer.

On Linux, launch each counted run in a dedicated cgroup v2 containing the LSP
and every Bazel, scip-java, JVM compiler, and helper process it creates. This
keeps reparented and daemonized processes attributable. Record
`memory.current`, `memory.peak`, `memory.events`, swap, CPU, and I/O. Start with
no unrelated process in that cgroup and terminate or account for every member
before reuse. On a platform without cgroup isolation, stop pre-existing Bazel
daemons and report process-group plus explicitly tracked persistent-process
memory; do not present that fallback as equivalent accounting.

Also sample processes every 100 ms. Record RSS, proportional set size where
supported, virtual memory, CPU time, read/write bytes, file count, thread count,
and command category. Call the largest observed value the **sampled peak**; do
not sum per-process high-water marks from different times. Attribute
Bazel/scip-java and JVM compiler processes separately while also reporting the
cgroup total.

RSS (resident set size) is the virtual memory currently backed by physical RAM;
it includes heap, native allocations, stacks, loaded code, and resident mapped
files. Report simultaneous process-tree sampled peak RSS, phase peaks, and RSS
after the 30-second quiescent interval. Summing RSS can double-count shared
pages, so also report proportional set size (PSS) on platforms that expose it.
Use cgroup `memory.peak` for the memory-budget gate because it also captures
charges not fully represented by RSS/PSS. JVM heap size and Rust allocator
statistics may be diagnostic fields, but are not substitutes for cgroup and
resident-memory measurements. Measure sampler overhead against an otherwise
identical run.

Store raw private LSP traces only when required for diagnosis. The normal
artifact should contain normalized counts and hashes rather than source text or
symbol names.

## Sampling and run order

- Perform untimed pilot runs first; discard them after fixing the protocol.
- Collect at least 10 clean/cold expensive samples and 30 warm samples per mode.
- Use at least 30 repetitions per query for p50/p95 summaries, aggregating first
  within the predefined symbol/refcount strata. Treat p99 as descriptive until
  a stratum has at least 1,000 observations.
- Randomize or alternate tool order within blocks to reduce thermal and host
  drift. Do not run both servers concurrently except in the explicit contention
  workload.
- Wait for stable readiness and the quiescent interval before ending a startup
  run.
- Report every failure and timeout. Never remove an outlier without preserving
  it and a documented external cause.
- Report raw samples, p50, p90, p95, p99, max, median absolute deviation, and
  bootstrap confidence intervals where sample size permits.

Repeated requests within one process are not independent. Bootstrap independent
process runs and query/symbol clusters, retaining within-run order, rather than
treating every request as an independent sample.

Define concurrency as either multiple in-flight requests over one stdio LSP
session or multiple server processes; report those as separate experiments.
For S1/S2, predeclare arrival rate, request mix, payload retention, concurrency,
and cancellation behavior. Send the same offered work to both tools so a faster
server does not receive more requests merely because it completed sooner.

## Result schema

Each run records:

- schema version, run ID, timestamp, tool/mode and immutable revisions;
- anonymized dataset ID and repository state;
- host and build configuration;
- cache/storage state and preparation commands;
- readiness timestamps and validation outcomes;
- per-operation returned/expected/total counts and latency;
- completeness, truncation, timeout, stale, and error indicators;
- request classification and successful-correct-answer rate;
- process-tree resource series and phase peaks;
- persistent bytes created by category;
- references to sanitized logs and normalized response hashes.

Publish under `benchmarks/comparison/<date>-<dataset>/`:

```text
environment.json
corpus-summary.json
runs.jsonl
summary.md
charts/
failures/
```

The summary must keep clean-checkout, source-ready, build-aware, and prepared
steady-state results separate. It must state when semantics differ and avoid a
single composite winner score.

## Decision criteria

Before running, agree on minimum usable gates rather than deriving them from the
winner. Provisional gates for discussion:

- zero wrong definitions in the fixed corpus;
- zero silently stale positional answers;
- no fast empty response accepted when the server is unready or timed out;
- p95 prepared definition latency below 250 ms;
- p95 prepared workspace-symbol latency below 250 ms;
- reference answers either meet the validated set or expose their partial state;
- at least 99% successful correct answers for applicable prepared-state common
  queries, with time-to-correct-answer reported for every workload;
- honest unsupported or unavailable responses reported separately from both
  incorrect answers and successful availability;
- S1 final five-minute median memory no more than the preregistered absolute and
  relative growth threshold above its minute-10 baseline;
- no cgroup `memory.peak`, swap, or OOM event above the host/cgroup budget
  selected before the run.

Select a tool by workload after reviewing the evidence. A likely outcome is not
one universal winner: Metals may be the better interactive IDE while Jabar may
be the better precomputed navigation service for an agent. That conclusion is
valid only if the common-host results demonstrate it.

## Execution phases

### Phase 1: freeze protocol and corpus

- [ ] Pin both revisions and document build/install commands.
- [ ] Record the repository and host manifest.
- [ ] Build and independently validate the query corpus.
- [ ] Decide target coverage and Bazel metadata modes.
- [ ] Pre-register timeouts and resource gates.
- [ ] Freeze query semantics, comparable source universe, and the complete
      state/reset matrix.

### Phase 2: build the neutral harness

- [ ] Extract reusable LSP transport and workload code from `jabar-bench`.
- [ ] Add Jabar and Metals lifecycle/readiness adapters.
- [ ] Add response normalization and correctness validation.
- [ ] Add process-tree CPU, memory, I/O, and disk sampling.
- [ ] Add cgroup isolation/accounting and measure observer overhead.
- [ ] Prove on a public fixture that wrong, partial, stale, and timed-out answers
      fail or are classified correctly.

### Phase 3: pilot

- [ ] Run every workload once on the public fixture.
- [ ] Run A1, A2, B1, Q1–Q3, C1, and C3 once on the target monolith.
- [ ] Fix instrumentation and protocol ambiguities before counted runs.
- [ ] Freeze scripts and schema.

### Phase 4: counted runs

- [ ] Collect randomized startup and query blocks.
- [ ] Run edit, refresh, failure, sustained, and contention scenarios.
- [ ] Validate artifact completeness and response hashes after every block.

### Phase 5: review and publish

- [ ] Generate summaries from `runs.jsonl`, never from hand-copied logs.
- [ ] Review correctness disagreements before calculating aggregate claims.
- [ ] Mark every planned workload pass, fail, invalid, or deferred with a reason.
- [ ] Publish sanitized raw samples, charts, limitations, and exact reproduction
      commands.

## External baselines

The Databricks report provides useful plausibility checks, not a comparison
baseline. It reports a 936 MB uncompressed MBT index, a 22-second clean index
build on 32 cores, a 5-second index parse, production TTII p50 of 8.7 seconds,
and fuzzy-symbol p50 of 10 ms on its 26-million-line monorepo. Different source
mix, hardware, caches, build metadata, semantics, and workloads prevent direct
comparison with Jabar's historical measurements.

References:

- [Databricks Metals v2 architecture and measurements](https://www.databricks.com/blog/open-sourcing-metals-v2-databricks-java-and-scala-language-server-multi-million-line-codebases)
- [Metals v2 source](https://github.com/scalameta/metals/tree/main-v2)
- [Metals v2 documentation](https://metals-lsp.org/)
- [Jabar monolith measurement protocol](monolith-measurements.md)
- [Jabar monolith measurement results](monolith-measurement-results.md)
