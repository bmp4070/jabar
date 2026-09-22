# Monolith measurement results — jabar vs. Salesforce core

First counted run of the harness ([bench.rs](../crates/jabar-server/src/bench.rs)
+ [tools/bench-client](../tools/bench-client), driven per
[monolith-measurement-runbook.md](monolith-measurement-runbook.md)) against a
live checkout of the Salesforce core monorepo.

> **Status: partial.** Startup (cache-miss + cache-hit), steady-state
> responsiveness, memory, and cache footprint are measured. Refresh scenarios,
> auto-indexing (scip-java aspect), cold-storage startup, and the full
> correctness query corpus are **not yet run** — see [Not yet measured](#not-yet-measured).

## What was tested

| | |
| --- | --- |
| Date | 2026-09-22 |
| Repo | `/opt/workspace/core-public/core` |
| HEAD | `121f5979cb112` (60026185, `266/p4/266-main`, freshly pulled to latest) |
| jabar | `bench/monolith-measurement-harness`, `--release` |
| Host | Linux, 122 GiB RAM, no cgroup memory cap; warm storage (shared host) |
| Mode | `index.auto=false` — measured from the SCIP shards already on disk (no aspect build) |

## Size of the monolith under test

This is the reported-scale monolith the protocol was written for.

| Dimension | Value |
| --- | --- |
| SCIP shards (`bazel-bin`) | **6,876** |
| Definitions | **3,130,763** |
| References | **39,434,571** |
| Occurrences | **42,567,085** |
| SCIP on disk | ≈5.4 GiB |
| Built index cache (`index.bin`) | **10.14 GiB** (10,892,558,559 B) + 949 KB manifest |
| In-memory index (steady-state resident) | ≈16.7 GiB |

These cache size, startup, and memory numbers describe the version 1 snapshot.
The later version 2 format interns reference paths and removes the duplicated
symbol and path strings from each reference row. It requires a new counted run;
the values below are retained as the comparison baseline.

Counts are identical across the cache-miss and cache-hit runs (3,130,763
definitions both times) — a first, if narrow, **correctness-parity** signal
between a freshly-decoded index and one restored from cache.

## Startup

Median of the runs below; each is a single sample (not yet the ≥10/≥30 the
protocol asks for — see caveats).

| Boundary | Cache miss (storage warm) | Cache hit (storage warm) |
| --- | --- | --- |
| `cache.key` | 2.2 ms | 2.4 ms |
| `cache.read` | 0.02 ms (miss) | **57.41 s** (hit) |
| `shards.decode` | **118.04 s** | — (skipped) |
| `initialize.total` | 118.04 s | 57.42 s |
| `watcher.start` | 0.20 ms | 0.18 ms |
| **client spawn → first `workspace/symbol`** | **118.24 s** | **57.46 s** |
| Peak RSS | **66.9 GiB** | **62.0 GiB** |

The cache **halves** cold start (118 s → 57 s) but does **not** make startup
interactive: the hit path is dominated by deserializing the 10.14 GiB MessagePack
snapshot (~57 s ≈ 190 MB/s effective — deserialize-bound, not I/O-bound, since
the file was warm in page cache). See [Findings](#findings).

## Steady-state responsiveness

From the `probe` workload — open-loop probes over 210 s against the static
(unchanging) shard set, so this isolates query cost, not refresh cost.

| Method | cadence | count | p50 | p95 | max | errors |
| --- | --- | --- | --- | --- | --- | --- |
| `jabar/status` | 50 ms | 4,200 | 0.20 ms | 0.26 ms | 8.35 ms | 0 |
| `workspace/symbol` ("Account") | 1 s | 210 | **167.0 ms** | **289.9 ms** | 313.4 ms | 0 |

`jabar/status` is effectively free. **`workspace/symbol` is the expensive
operation at monolith scale** — p50 167 ms, p95 290 ms — which exceeds the
protocol's 250 ms p95 responsiveness target. (Single query string; a broader
corpus is needed before treating this as the definitive number.)

## Event-loop health

`event_loop.heartbeat`, 20 ms cadence, armed only under bench:

| count | p50 lateness | p95 | p99 | max |
| --- | --- | --- | --- | --- |
| 8,735 | 0.085 ms | 0.096 ms | 158 ms | 309 ms |

Steady-state the loop is extremely healthy (99% of beats < 0.1 ms late). The
p99/max tail comes from the transition out of the blocking decode and
concurrent cache-write I/O; a few beats exceed the 100 ms (and one the 250 ms)
gate around that window. **Note:** heartbeats are *not* recorded during the
~118 s decode itself — the synchronous decode blocks the event loop entirely,
so no beats are serviced (see finding 1).

## Findings

1. **Cold startup blocks the event loop for the full decode (~118 s).**
   `shards.decode` runs synchronously inside `initialize`, so the server is
   unresponsive — no heartbeats serviced, no requests answered — for ~2 minutes
   on a cold cache. This is the dominant startup cost.

2. **Cache-hit startup is still ~57 s and deserialize-bound.** Restoring the
   10.14 GiB snapshot only halves cold start. The cost is rmp-serde
   deserialization of 3.1M defs + 39.4M refs + 42.6M occurrences, not disk I/O.
   The built-index cache helps but does not deliver a near-instant warm start at
   this scale.

3. **`workspace/symbol` is the slow query (p50 167 ms / p95 290 ms).** Over a
   3.1M-definition index, symbol search exceeds the 250 ms p95 target, while
   `jabar/status` stays sub-millisecond.

4. **Peak RSS (~62–67 GiB) is ~3.7× the resident index (~17 GiB).** Both the
   decode and cache-load paths spike memory well above the steady-state
   footprint. The first run did not collect allocation or stage-specific RSS
   evidence, so it does not establish how much came from one-shard protobuf
   decoding, collection growth, duplicated reference strings, cache writing,
   or generation overlap. Sizing must use the observed peak until a fresh
   process and stage-specific rerun separates those causes.

5. **Harness gap — the `startup` workload cut off the async `cache.write`.**
   It shuts down immediately after the first query, killing the server
   mid-write; the first attempt left a partial `-tmp` generation with no
   `CURRENT` and no cache published. Worked around by using `probe` (keeps the
   server alive) to persist the cache. **Fix:** add a hold/quiesce step to
   `startup` so it waits for cache publication before shutdown — already listed
   as an open item in the runbook.

## Recommended fixes

In priority order, mapped to the findings above. Product/engineering changes
(1–4) are proposals, not yet implemented; (5) is harness hygiene.

1. **Make the index build asynchronous (finding 1).** Return from `initialize`
   immediately and build the index on a background thread; report readiness via
   `jabar/status` (`indexLoaded`) and progress notifications. Requests that
   arrive before the index is ready get a fast "still indexing" answer instead of
   a ~2-minute stall. This is the single biggest responsiveness win — it removes
   the event-loop block regardless of cold/warm.

2. **Cut warm-start deserialization (finding 2).** The 10.14 GiB version 1
   MessagePack snapshot takes ~57 s to deserialize. Version 2 first compacts the
   39.4M reference rows: the containing map supplies their symbol and one path
   table replaces per-row path strings. It keeps streaming deserialization and
   rejects version 1 before loading, avoiding an old-plus-new conversion peak.
   Measure that change before choosing an mmap-friendly layout, lazy sections,
   or a split hot index. Target: seconds, not ~1 min.

3. **Add a persisted name index for `workspace/symbol` (finding 3).** p95 ~290 ms
   over 3.1M definitions suggests a scan. Precompute and persist a prefix/trigram
   or FST name index in the cache so symbol search is a lookup, and cap/stream
   results. Target: p95 < 250 ms.

4. **Reduce peak memory on both paths (finding 4).** Peak ~67 GiB vs ~17 GiB
   resident. The loader already handles shards one at a time and streams cache
   input into the final index. Version 2 reduces the final reference rows from
   roughly 72 to 24 bytes before string payload and allocator savings. Rerun
   cache load in a fresh process and add stage-specific memory evidence before
   selecting the next allocation target.

5. **Make `startup` wait for cache publication (finding 5).** Add a
   `--hold-secs`/quiesce step so the workload polls `jabar/status` (or waits)
   until `cache.write` publishes `CURRENT` before shutting down. Then
   cache-miss → publish → cache-hit is a single workload, no `probe` workaround.

Alongside these, raise sample counts to the protocol's targets (≥10 miss / ≥30
hit) before publishing any number as a gate result.

## Method / caveats

- Server phase events via `JABAR_BENCH_LOG`; client JSON from `jabar-bench`;
  peak RSS via `/usr/bin/time -v`. Client and server correlate by `run_id`.
- Percentiles are nearest-rank. Sample sizes are **single startup samples** and
  one 210 s probe window — below the protocol's ≥10 (miss) / ≥30 (hit) startup
  repetitions. Treat these as first-pass magnitudes, not published figures.
- `workspace/symbol` used one query ("Account"); no fixed multi-query corpus yet.
- Storage was warm (shared host); the cold-storage startup variant is deferred.

## Not yet measured

- Startup repeated to the protocol's sample counts (≥10 miss / ≥30 hit) with
  p50/range.
- The 12 refresh and lifecycle scenarios (shard mutation, incremental build,
  branch switch, watcher-error → unverified, explicit reload, reclamation, and
  superseded cache writes) and their reload/heartbeat behaviour.
- Auto-indexing: the scip-java aspect build (`index.build`), and fresh-build vs
  cache-hit correctness parity beyond raw counts.
- Cold-storage (page-cache-dropped) startup.
- A fixed correctness query corpus compared normalized in-private.
- Invalid-cache and configuration-miss fallbacks against core.
