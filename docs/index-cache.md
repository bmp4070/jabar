# Startup fast-path: caching the built index

## The problem, measured

`discover_index` (`crates/jabar-server/src/server.rs`) hands `bazel-bin` to
`SymbolIndex::from_dir` (`crates/symbol-index/src/lib.rs`) before the server can
answer `initialize`, so the editor blocks on everything it does. On Salesforce
core that is **~5 minutes**. The cost splits in two, and `from_dir` conflates
them:

1. **The walk.** `from_dir` recurses `bazel-bin` with a `read_dir` +
   `symlink_metadata` on every entry, pruning only `*.scip-targetroot` and
   `*.semanticdb`. Core's `bazel-bin` holds **2.68M files**; the walk alone is
   tens of seconds to minutes.
2. **The parse + build.** For each of **6,876** `*.scip` shards it does a
   `std::fs::read`, a protobuf decode, then builds the maps — **3.13M
   definitions** plus references, per-file occurrences, symbol interning, and a
   per-document sort. This is the larger half.

Both costs depend only on the shards on disk. Nothing about them is
per-session, so paying them on every start is waste: the same bytes produce the
same index until the shards change.

A shard-path *manifest* (cache the list of `*.scip` paths, skip the walk) removes
only cost 1. To remove cost 2 as well, the **built index itself** has to be
cached — serialized once, read back in one pass, with no protobuf and no map
construction.

## The approach

Cache the built index under `.jabar/index/` (already git-ignored in target
repos). On start, read it back in one pass instead of walking and parsing; on a
miss, fall back to the walk exactly as today and write the cache for next time.

```
.jabar/index/
  index.bin     the serialized index (format below — open decision)
  stamp.json    what it was built from, for validation
```

The cache is an **optimisation, never a source of truth.** A miss, an
unreadable file, a format-version bump, or a failed decode all fall back to
`from_dir`. It can never make jabar answer *wrong*, only answer *stale* — and
staleness is bounded (see below).

`discover_index` becomes: try the cache, else walk `bazel-bin`, else walk
`.jabar/index` for hand-dropped shards. On any successful walk, write the cache.
The dir returned for the watcher to watch is always the shard directory
(`bazel-bin`), taken from `stamp.json` on a cache hit — never `.jabar/index`
itself.

## Freshness

The cache is only as current as the shards it was built from, so it needs a
staleness story. Three mechanisms, cheapest first:

- **A stamp, checked in milliseconds.** `stamp.json` records a format version,
  the shard directory, `git rev-parse HEAD` (**measured: 4ms** on core), and the
  build's shard/definition counts and timestamp. A format mismatch or a moved
  workspace is caught here without touching the shards. `git status --porcelain`
  is **not** consulted — it is **28s** on core even with fsmonitor, far too slow
  for the handshake, so uncommitted rebuilds are invisible to the stamp.
- **The watcher, during the session.** `watcher` already fires `Change::Index`
  when shards change on disk and `reload_index` rebuilds. Any rebuild *while
  jabar runs* is caught here; `reload_index` will also rewrite the cache.
- **A background refresh, off the hot path.** The stamp cannot see a rebuild
  that happened on the same commit while jabar was down. So on a cache hit,
  after serving from the cache instantly, kick a background thread that re-walks
  the shards and swaps the fresh index in when it lands (reusing the
  `adopt_index` machinery). Skip it when the cache is only seconds old, to keep
  rapid restarts from thrashing. This converges to fresh within one refresh
  instead of blocking startup on one.

The net contract: **instant startup, never stale for more than one background
refresh, never wrong.** For the primary consumer — an agent navigating code — a
minutes-stale index that self-heals beats a 5-minute stall or an honest "no
index" error, which is the tradeoff the README's Next steps already argues for.

## Open decision: the serialization format

Caching the built index means writing its data out and reading it back. The
question is *how much* of `SymbolIndex` to serialize, and how much serde surface
that puts on this crate's public API. Three options:

1. **Whole-struct serde.** Derive `Serialize`/`Deserialize` on every index type
   (`Range`, `PositionEncoding`, `SymbolKind`, `Definition`, `Reference`,
   `Occurrence`, `SymbolIndex`) and `bincode` the lot. Least code (~30 lines).
   Costs: serde on seven public types, and the blob persists the derived lookup
   maps (`by_name`, `by_symbol`, `by_path`, `implementors`, `symbol_ids`) that
   are redundant with `definitions` — roughly double the necessary size, and the
   on-disk layout is now coupled to the internal map representation.

2. **Persist the source of truth, rebuild the maps (recommended).** Serialize
   only what cannot be recomputed — `definitions`, `references`, `occurrences`,
   `symbol_names`, `shards` — into a private `Snapshot` type, and rebuild
   `by_name`/`by_symbol`/`by_path`/`implementors`/`symbol_ids` on load by
   replaying `insert`/`intern_symbol`. serde stays on the plain value types
   (`Definition`, `Reference`, `Occurrence`, `Range`, and the two enums); the
   `SymbolIndex` container and its map layout stay private and free to change
   without breaking old caches. Roughly half the blob, and the rebuild pass is
   in-memory — still far below the protobuf parse it replaces. Costs: more code
   than option 1, and a `to_snapshot`/`from_snapshot` pair in `symbol-index`.

3. **Shard manifest only.** Cache the list of shard paths, no serde anywhere,
   re-parse the protobufs each start. Removes the walk but not the parse:
   startup ~5min → ~3min, not seconds. Weakest win; kept here only as the
   floor.

Option 2 is the recommendation: it confines serde to genuine value types, keeps
the cache format decoupled from the index's internal representation, and halves
the on-disk size, for a modest amount of extra code. Whichever format is chosen,
`index.bin` carries a magic + format version so an incompatible file is rejected
rather than misread, and is written to a temp file and renamed so a crash
mid-write cannot leave a half-written cache.

## Status

Not yet implemented. This note records the design and the open format decision;
the earlier README "Next steps" entry is the higher-level version of the same
plan.
