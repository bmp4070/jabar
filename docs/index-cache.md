# Built-index cache for large Bazel workspaces

## Current path and observed cost

After answering LSP `initialize`, one startup worker resolves the output tree
and first attempts to load the keyed built-index snapshot. On a miss or rejected
snapshot, the validated-shard fallback stats the tree, reads every `.scip`
shard, decodes protobuf, and builds lookup maps. The first counted
representative run used a tree of a couple million files with 6,876 shards and
3.13M definitions. On the measured host, version 2 took about 100 seconds on the
shard-decode path and about 22 seconds on a warm cache hit. These are limited
samples on one workspace; see
[`monolith-measurement-results.md`](monolith-measurement-results.md) for the
environment and caveats.

A shard-path manifest skips the walk but still decodes every shard. A
concatenated `index.scip` also needs protobuf decoding and lookup construction.
Neither is the complete startup fast path. Persisting the built `SymbolIndex`
can remove those costs, subject to measuring deserialization, allocations,
memory, and map reconstruction. A cached index does **not** prove that Bazel
outputs match current source files.

## Cache and provenance

Store cache files below `.jabar/index/cache/`, leaving the existing
`.jabar/index/*.scip` fallback for hand-dropped shards. Ensure `.jabar/` is
ignored by the target workspace. Each complete generation contains:

- `index.bin`: a versioned built-index snapshot with a magic header and checksum.
- `manifest.json`: currently stores schema version, canonical workspace and
  output-tree paths, Bazel/configuration identity, target patterns, scip-java
  path, git HEAD, snapshot bytes/checksum, and shard paths with size/mtime
  fingerprints. Extend it with aspect/scip-java versions, build completion time,
  build outcome, and indexed/failed/skipped/unknown target counts when coverage
  reporting lands.

The cache root also contains `CURRENT`, an atomically replaced pointer to the
published generation.

Write a generation in a temporary directory, flush it, then publish `CURRENT`
last. On a crash, the previous complete generation stays usable. Reject corrupt
or incompatible files and mismatched workspace, output base, configuration, or
HEAD. The generation ID prevents a late worker from replacing an index loaded
after a newer build or branch switch. Hashing all shard bytes at startup would
reintroduce the walk and reads, so fingerprint validation happens when producing
the manifest and during background reconciliation, not on the handshake path.

For builds Jabar initiates, create the manifest from the aspect's output list as
part of the build. A Bazel build performed outside Jabar may change outputs
without updating this manifest; support an explicit `jabar/refresh` or
equivalent build hook, plus a background reconciliation scan. A same-HEAD build,
uncommitted sources, or an external build while Jabar was stopped cannot be
detected from HEAD alone. Treat a loaded cache's shard state as **unverified**
until reconciliation. Reconciliation only verifies the cache against the shards;
only a completed build can establish which sources those shards cover. Surface
both states and cache age in `jabar/status`, and do not claim that a matching
stamp proves source freshness. If the source tree has moved to another HEAD,
follow the existing branch-switch behavior and do not serve the old snapshot as
current.

## Load and refresh path

1. Reply to `initialize`, then use one startup worker to resolve Bazel output,
   inspect `CURRENT`, validate metadata, and load `index.bin`. A cache miss uses
   shard discovery on that worker. `jabar/status` exposes `indexLoading`,
   `startupState`, and `startupError`. Queries before readiness return
   `RequestFailed` with `IndexNotReady`; they do not return empty results.
2. Cached sessions now avoid the recursive `bazel-bin` watch and use bounded
   periodic reconciliation. Add a watch on a small build-published manifest or
   generation pointer, or an explicit refresh trigger, to reduce detection
   latency for external builds. A watcher overflow/error must mark the index
   unverified and schedule a scan.
3. Startup cache decode, cold shard loading, optional `index.auto` build,
   watcher setup, refresh scans, and cache writes run on workers. Completed
   generations return to the event loop for publication. Startup cannot overlap
   an explicit load or refresh, and stale or cancelled results are discarded.
4. Rapid refresh changes are coalesced and progress/errors are reported. Add
   rate limiting for repeated failures and expose cache age. Keep the prior
   index while refreshing when its provenance still matches. Measure peak RSS
   while old and new snapshots coexist. A refresh that fails or never runs does
   not impose a bound on staleness.

## Serialization decision

Benchmark at least two formats on the same representative shard set:

| Format | Expected tradeoff |
| --- | --- |
| Whole built index | Larger file and coupling to internal maps; avoids rebuilding them on load. |
| Value snapshot (`definitions`, `references`, `occurrences`, symbol names, shard metadata) | Smaller file; reconstructs maps for millions of entries and may allocate heavily. |

Record serialized bytes, write time, warm and cold read time, map rebuild time,
peak and steady RSS, and query parity with the shard-built index. Choose only
after measuring; the snapshot is not assumed to take seconds or half the space.
Version the format and preserve a safe fallback to shards. Test corruption,
partial writes, stale generations, same-HEAD rebuilds, output-base changes,
branch switches, and a refresh finishing after a newer generation.

## Completion criteria

Run the protocol in
[`docs/monolith-measurements.md`](monolith-measurements.md), including its
cache-state definitions, phase timings, peak/steady RSS collection, response
probes during reload, normalized response comparisons, raw artifacts, and
acceptance gates. Accept the cache only when the warm path meets those gates and
produces the same answers as a fresh shard load. `docs/monolith-roadmap.md`
tracks the other requirements for large repositories.

**Status:** the first implementation persists the complete built index with a
versioned MessagePack snapshot, checksum, configuration/HEAD key, exact shard
metadata, atomic generation pointer, and bounded old-generation cleanup. A hit
loads before scanning `bazel-bin`; reconciliation and reload run on worker
threads, cached sessions avoid a recursive output-tree watcher, and
`jabar/status` distinguishes a cache hit from verified shard metadata. Cache
format version 2 stores each reference as a compact row and interns its file
path instead of serializing an owned symbol and path for every reference. The
surrounding map supplies the symbol. Cache reads validate the path ids before
publishing the index, and version 1 snapshots fall back to shard loading rather
than being converted in memory. Benchmark events record snapshot bytes,
reference/occurrence counts, and distinct reference paths on cache hits.

The version 2 layout still uses streaming MessagePack. It is an incremental
reduction in bytes, allocations, and resident state ahead of any mmap or lazy
format decision. Cache misses still scan and decode serially inside the startup
worker. Version 2 has initial representative-monolith measurements; the required
repetitions, refresh workloads, normalized query-response comparison,
source-build freshness, indexing coverage, and CI distribution work remain open.
