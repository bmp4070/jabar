# Built-index cache for large Bazel workspaces

## Current path and observed cost

`discover_index` calls `SymbolIndex::from_dir(bazel-bin)` before answering LSP
`initialize`. The loader stats the tree, reads every `.scip` shard, decodes
protobuf, and builds lookup maps. On Salesforce core, the reported input is a
2.68M-file tree with 6,876 shards and 3.13M definitions. The reported walk is
~46s and total initialization ~5 minutes. These are observations from one
workspace, not repeatable benchmarks yet; record the commands, hardware, cache
state, shard bytes, and timing breakdown before committing to a format.

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
- `manifest.json`: schema version, generation ID, canonical workspace path,
  Bazel output base and configuration identity, target patterns, aspect and
  scip-java versions, git HEAD at build time, completion time, and the exact
  shard paths and fingerprints included in the snapshot. Include build outcome
  and indexed/failed/skipped/unknown target counts when coverage reporting lands.

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

1. On startup, resolve workspace and Bazel configuration, inspect `CURRENT`,
   validate the generation metadata, and load `index.bin`. Measure this path
   against a defined warm-start budget. A cache miss retains the existing shard
   discovery path until a separate cold-start design is implemented; it may
   still block `initialize` for minutes on a large workspace.
2. Replace the recursive `bazel-bin` watch with a watch on the small published
   manifest or generation pointer. An external build that does not publish it
   needs an explicit refresh trigger or a bounded periodic reconciliation. A
   watcher overflow/error marks the index unverified and schedules a scan.
3. Run shard scans, protobuf parsing, snapshot loading, and any Bazel build on
   workers. Send completed results to the server's event loop. `adopt_index`
   and `reload_index` are currently synchronous; moving only the initial read
   to a thread would leave the loop blocked during later reloads. Apply a result
   only if its workspace, configuration, HEAD, and generation still match.
4. Coalesce rapid changes, rate-limit failed refreshes, and report progress and
   errors. Keep the prior index while refreshing when its provenance still
   matches; report its unverified age. Measure peak RSS while old and new
   snapshots coexist. A refresh that fails or never runs does not impose a
   bound on staleness.

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

Publish a repeatable benchmark for `initialize` p50/p95, time to first usable
query (including watcher setup), cache validation, deserialization, cold miss,
reload pause, peak RSS, and disk size on both the fixture and a representative
monolith. Set latency and memory budgets from the
baseline, then accept the cache only when the warm path meets them and produces
the same answers as a fresh shard load. `docs/monolith-roadmap.md` tracks the
other requirements for large repositories.

**Status:** design only. The current server still scans `bazel-bin` before
`initialize` and reloads synchronously.
