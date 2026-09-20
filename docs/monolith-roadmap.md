# Large Java monolith readiness

Jabar can answer its nine LSP operations from SCIP shards, but its current
startup, refresh, and coverage behavior has not been validated for a workspace
with millions of build outputs. This is an implementation plan, not a claim
that those workloads already work. `docs/phase-1.md` records the earlier
milestones; `docs/index-cache.md` details the proposed cache.

## 1. Establish a repeatable baseline

Use the fixture and at least one representative monolith. Record the exact
Jabar/scip-java/aspect revisions, JDK, Bazel version, output base, target
patterns, host memory/CPU/storage, number and total bytes of shards, build
outcome, and indexed target count. If a proprietary workspace cannot publish
paths or symbols, publish the measurement method and aggregate numbers.

Measure cold and warm runs separately, at least ten process starts each. Split
`initialize` into index discovery, shard reads, protobuf decode, map build,
and response time. Measure time to first usable query including watcher setup,
no-op and changed-shard reloads, event-loop pauses, disk I/O, peak and steady
RSS, and cache artifact size. For
query latency, use representative exact/prefix/substring symbol searches,
definition, high-fan-out references, and call hierarchy requests. Report p50
and p95 with result counts; include an unopened edited file and a generated
source in the correctness corpus.

Provisional acceptance budgets for the representative monolith are warm
`initialize` p95 under 10s, event-loop pauses during reload p95 under 100ms,
and ordinary symbol/definition queries p95 under 250ms. Set separate budgets
for broad search and high-fan-out references after the baseline, along with a
peak RSS and disk budget tied to the target host. Do not present these as
achieved until measured. Preserve answer parity with a fresh shard-built index.

## 2. Make indexing coverage observable

First verify representative ECJ and javac targets with Bazel `aquery`: record
their action mnemonics, `JavaInfo` availability, javac options, JDK/source
level, generated sources, annotation processors, and actual scip-java
outcomes. The bundled aspect currently picks only an action named `Javac` and
passes `compilation.javac_options` to scip-java. A different ECJ action name
could prevent indexing before invalid flags matter.

Put a narrowly tested option translation in the maintained aspect, or upstream
it. Avoid a hand-patch to `.jabar/aspects/`: Jabar overwrites that copy when
running the aspect. Test each excluded option against actual target command
lines; keep flags required for correct symbols and diagnostics. Include ECJ
and javac targets, mixed builds, generated sources, and targets that ECJ can
compile but stock javac rejects. This work does not require a scip-java fork
unless an upstream limitation is demonstrated.

`--keep_going` currently accepts Bazel exit 3 and loads the shards that exist.
Add a build coverage report with attempted, indexed, failed, and skipped
targets, reasons, and the requested target patterns. If Bazel cannot enumerate
all expected targets reliably, report **unknown** coverage instead of implying
completeness. Put the summary in `jabar/status` and the index manifest. An
empty symbol result must be distinguishable from an unindexed or failed target.
Gate this stage on no silent coverage gaps in the representative test set.

## 3. Cut startup cost without weakening provenance

The first implementation of `docs/index-cache.md` publishes the complete built
index with an exact shard manifest and validates workspace/configuration and
format cheaply. Benchmark that whole-index format against a smaller value
snapshot. A concatenated SCIP file or a shard-path list only removes the tree
walk; neither is sufficient for a seconds-scale startup.

Index reconciliation and rebuilds now run off the LSP event loop. Cached
sessions avoid recursively watching `bazel-bin`, periodically reconcile shard
metadata, coalesce updates, and reject stale worker results. Measure the
temporary memory cost of holding old and new indexes and evaluate a build hook
or manifest watch for faster external-build detection. Shard verification is
reported separately from source build freshness: a scan can prove that the
cache matches current shards but cannot prove that the shards match current
source files. A same-HEAD external rebuild cannot be detected from git HEAD
alone.

A cache miss still incurs today's long walk. Decide and test the cold-start
client behavior separately: prebuild/distribute a cache in CI or a developer
bootstrap step, or add a protocol path that loads later and makes capabilities
available predictably. Do not block `initialize` for minutes while claiming a
fast cold start. Gate this stage on the measured startup and reload budgets,
correct branch/configuration invalidation, and parity with fresh shards.

## 4. Keep queries and memory usable at scale

`SymbolIndex::search` currently lowercases and scans every definition for
each query, then sorts all hits. Profile it at millions of definitions and
introduce a suitable short-name lookup structure or bounded candidate search
if it misses the query budget. Measure the index's steady and peak memory,
including duplicated strings, paths, and the cost of a concurrent reload.
Check reference and call-hierarchy fan-out, result caps, file reads, and
sorting so truncation reports the true total and does not hide a match that
should rank first.

Add regression workloads that run queries while a build publishes new shards,
while a branch changes, and while a malformed shard or cache is encountered.
Gate this stage on query latency, memory budget, stable answers, and explicit
partial-result counts rather than just unit tests on the small fixture.

## 5. Close source and build freshness gaps

The overlay covers open files, but out-of-band changes to unopened files still
wait for a build. `BUILD` changes do not yet re-slice affected targets. Define
how filesystem edits, generated outputs, Bazel configuration changes, and
external aspect builds invalidate coverage or trigger a refresh; expose that
state to clients. Validate a concurrent `bazel test` with shared and separate
output bases, including lock time and extra disk/RSS. Keep `//...` optional for
large workspaces and measure scoped target patterns first.

The large-repo milestone is complete only when startup, reload, query, memory,
and coverage measurements meet their stated budgets on a representative
monolith, and the status output explains any stale or missing part of the
index.
