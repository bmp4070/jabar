# Measuring jabar against the Monolith — runbook

A concrete, repo-specific procedure for executing
[`docs/monolith-measurements.md`](monolith-measurements.md) against the working
copy at `/path/to/monolith`. The protocol doc is the authority on
*what* to measure and the acceptance gates; this runbook records *how* on this
machine, and what the harness added under `crates/jabar-server/src/bench.rs` and
`tools/` gives you.

> **Status:** harness built and validated on a synthetic fixture. No counted
> Monolith runs collected yet. Treat every number below the tooling section as a
> budget, not a result.

## The target, as measured on 2026-09-21

| Property | Value |
| --- | --- |
| Workspace | `/path/to/monolith` |
| `bazel-bin` | → `<bazel-output-base>/execroot/monolith/bazel-out/k8-fastbuild/bin` |
| SCIP shards under `bazel-bin` | 6,876 |
| Total SCIP bytes | ~5.4 GiB |
| `.jabar/` | `aspects/` present; **no `index/` cache** → first run is a clean cache-miss |
| bazel / bazelisk | `/usr/local/bin/bazel`, `~/.nix-profile/bin/bazelisk` |
| JAVA_HOME | `/nix/store/v7ngs49whjqv3jhdsdmip7scajb14yz2-onejdk21-21.0.9.0.101+148` |

This is the reported-scale monolith (6,876 shards) the protocol was written for.

## Toolchain locations on this host

The reproducible way is `nix-shell` from the repo root — [`shell.nix`](../shell.nix)
pins the Rust 1.97.1 toolchain (rustc/cargo/**clippy**/**rustfmt**/rust-analyzer),
`jdk21`, and `coursier`, and bootstraps scip-java 0.12.3 on first entry. Both
`cargo fmt` and `cargo clippy -D warnings` run there (clippy is clean on the
harness crates). If you must pin by hand instead:

```sh
export PATH="/nix/store/cavxgwfb7l7akyvvqvnl39d6nw0wckgh-cargo-1.97.1/bin:$PATH"
export JAVA_HOME="/nix/store/v7ngs49whjqv3jhdsdmip7scajb14yz2-onejdk21-21.0.9.0.101+148"
export PATH="$JAVA_HOME/bin:$PATH"
```

### scip-java (for auto-indexing / fresh-build parity)

Not a binary on `PATH`, but coursier and the 0.12.3 artifacts are cached. A
standalone launcher was bootstrapped to:

```
/home/sfwork/.local/jabar-bench/bin/scip-java   # scip-java 0.12.3, verified `--version`
```

Reproduce with:

```sh
export PATH="/nix/store/sd4ia6jxwjsc2x0wr0qspgvld49ydplq-coursier-2.1.24/bin:$PATH"
cs bootstrap com.sourcegraph:scip-java_2.13:0.12.3 \
  -M com.sourcegraph.scip_java.ScipJava \
  -o ~/.local/jabar-bench/bin/scip-java --standalone
```

Pass its path to jabar as `index.scipJava` (see the auto-indexing workload).

## What the harness adds

### Server-side phase instrumentation (`crates/jabar-server/src/bench.rs`)

Off unless `JABAR_BENCH_LOG` names a file. When set, each instrumented boundary
appends one JSON line to that file. The field set is a fixed, numeric struct
(`Fields`) — counts, bytes, durations, a `&'static str` outcome — so a bench log
**cannot** contain a source path or symbol string, satisfying the protocol's
privacy rule by construction. When off, each call is one `OnceLock` read.

Events wired so far, mapped to the protocol's boundary table:

| Protocol event | Emitted as | Where |
| --- | --- | --- |
| `initialize.total` | `initialize.total` | `run_server`, request-received → response-sent |
| `cache.key` | `cache.key` | `discover_index` |
| `cache.read` | `cache.read` (with `cache_hit`) | `discover_index` |
| `shards.scan` + `shards.decode` | `shards.decode` (miss path, combined) | `discover_index` |
| `reconcile.scan` | `reconcile.scan` | refresh worker |
| `reload.build` | `reload.build` | refresh worker |
| `reload.swap` | `reload.swap` | `on_refresh_result` |
| `explicit_load.build` | `explicit_load.build` | explicit-load generation worker |
| `index.reclaim` | `index.reclaim` | index reclamation worker |
| `cache.queue` | `cache.queue` | cache job queued → started/superseded |
| `cache.write` | `cache.write` | `write_cache_async` worker |
| `watcher.start` | `watcher.start` | `start_watching` |
| `event_loop.heartbeat` | `event_loop.heartbeat` (`lateness_ns`) | main loop, 20ms, **armed only under bench** |
| (auto-index build) | `index.build` | `build_index` |

Gaps to close before counted runs:
- `shards.scan` and `shards.decode` are **combined** on the discovery path
  (they are one call, `load_validated_generation`). Split them if the protocol
  needs the walk isolated from decode; the refresh path already separates them.
- `startup.total` (process-entry → first correct query) is measured **client
  side** by `jabar-bench` (`spawn_to_first_symbol`), not as a server event.
- References/occurrence counts are recorded on decode/reload events via the new
  `SymbolIndex::reference_count()`; wire them into `initialize.total` too if you
  want them on the handshake event.

### The benchmark client (`tools/bench-client`, binary `jabar-bench`)

Speaks LSP over stdio to a spawned `jabar`, drives one workload, prints exactly
one JSON object, and **exits non-zero when the run fails validation** (index not
loaded, or a sentinel query below its floor) — a slow-but-wrong run cannot read
as a pass. It sets `JABAR_BENCH_RUN`/`JABAR_BENCH_LOG` on the child so the
client JSON and the server phase log share a `run_id`.

Workloads:
- `startup` — spawn→initialize→initialized→status polling→readiness→first
  `workspace/symbol`, with `--ready-timeout-secs` (default 180), `index_loaded`,
  and `returned >= --expect-min` validation. Reports initialize, readiness,
  and first correct query separately. Phase-2 startup.
- `probe` — after startup, open-loop `jabar/status` @50ms and `workspace/symbol`
  (+ optional `textDocument/definition`) @1s for `--duration-secs`, reporting
  nearest-rank p50/p95/max per method. Phase-3 responsiveness. (Single in-flight
  request — a synchronous approximation; the server heartbeat is the
  authoritative pause metric.)

### The fixture generator (`tools/gen-shard`, binary `jabar-gen-shard`)

Writes one synthetic `*.scip` with N class definitions, so the harness can be
validated with no Bazel/JDK/scip-java. Used by the smoke test below.

## Build and self-test

```sh
cd /opt/workspace/jabar
cargo build --release -p jabar-server -p bench-client -p gen-shard

# Harness self-test (fixture scale, ~instant): proves transport, validation,
# exit codes, and that the bench log is written.
JABAR=target/release/jabar; GEN=target/release/jabar-gen-shard; BENCH=target/release/jabar-bench
WS=$(mktemp -d); $GEN "$WS/.jabar/index" Widget Gadget Sprocket
$BENCH startup --jabar "$JABAR" --root "$WS" --query Widget --expect-min 1 \
  --bench-log "$WS/bench.jsonl" --run-id selftest   # -> {"ok":true,...}, exit 0
$BENCH startup --jabar "$JABAR" --root "$WS" --query Widget --expect-min 999   # -> ok:false, exit 1
cat "$WS/bench.jsonl"; rm -rf "$WS"
```

The protocol's completion checklist requires "fixture runs prove the harness
catches wrong counts and corrupt caches" — the `--expect-min 999` case is the
wrong-count proof. Add a corrupt-cache case (truncate `.jabar/index/cache/CURRENT`)
before signing that box off.

## Phase plan against the Monolith

Run `jabar` with `index.auto=false` for cache/reload runs so a Bazel child is
never mistaken for startup work; measure auto-indexing separately.

### Phase 2 — startup workloads (shards already on disk)

Discovery reads **all** `bazel-bin` shards regardless of `index.targets`, so the
first run is the full ~5.4 GiB decode the protocol calls a cache-miss.

- **Cache miss, warm storage** — ensure no `.jabar/index/cache`; run `startup`.
  Records `shards.decode`, then the async `cache.write` publishes a generation.
  ≥10 samples, report p50/range/raw (expensive; expect minutes).
- **Cache hit, warm storage** — repeat `startup`; now `cache.read` hits and
  `initialize.total` should collapse. ≥30 samples (≥100 if within 10% of a gate).
- **Cache hit, cold storage** — **deferred on this shared host**: needs a reboot
  or a page-cache drop we must not do here. Record as deferred with the reason.
- **Invalid cache** — corrupt a copied generation; confirm fallback to shards.
- **Configuration miss** — change `index.targets`/`outputBase`; confirm the key
  invalidates and it falls back correctly.

Example (cache hit, one sample, common sentinel):

```sh
target/release/jabar-bench startup \
  --jabar target/release/jabar \
  --root /path/to/monolith \
  --init-json '{"index":{"auto":false}}' \
  --query Service --expect-min 1 \
  --bench-log /tmp/jabar-bench/monolith-hit.jsonl --run-id monolith-hit-001
```

Wrap in a loop for N samples; keep the process alive through reconciliation,
cache publication, and 30s quiescence per the protocol before recording peak RSS
(via `/usr/bin/time -v` around the client, plus 100ms `/proc/<pid>` sampling).

### Phase 3 — refresh & responsiveness

Drive the 12 refresh and lifecycle scenarios by mutating shards under
`bazel-bin` or switching HEAD while a `probe` run is in flight, e.g.:

```sh
target/release/jabar-bench probe \
  --jabar target/release/jabar --root /path/to/monolith \
  --init-json '{"index":{"auto":false}}' \
  --query Service --duration-secs 90 \
  --bench-log /tmp/jabar-bench/monolith-scn2.jsonl --run-id monolith-scn2-001 &
# ... then touch one shard / incremental aspect build / branch switch ...
```

The refresh state machine already distinguishes the scenarios
(`RefreshIntent::{Idle,Retry,AwaitingBuild}`, watcher-error→unverified,
branch-switch→AwaitingBuild). Correlate the client's latency series with the
server's `reconcile.scan`/`reload.build`/`reload.swap`/`event_loop.heartbeat`
events by `run_id`. Gate: per-method p95 <250ms, heartbeat lateness p95 <100ms,
none >250ms late, no timeouts.

### Auto-indexing & fresh-build parity (scip-java)

To measure the aspect build and produce a from-scratch parity baseline:

```sh
target/release/jabar-bench startup \
  --jabar target/release/jabar --root /path/to/monolith \
  --init-json '{"index":{"auto":true,"targets":["//<scoped>/..."],
                "scipJava":"/home/sfwork/.local/jabar-bench/bin/scip-java"}}' \
  --query Service --expect-min 1 --bench-log /tmp/jabar-bench/monolith-autoindex.jsonl \
  --run-id monolith-autoindex-001
```

This emits `index.build`. Scope `targets` — `//...` on the Monolith includes targets
broken at HEAD, credentialed, or missing toolchains. Compare counts
(shards/definitions/references/occurrences) against a cache-hit process for the
Phase-4 parity check using `SymbolIndex`'s count methods.

### Phase 4 — correctness parity, Phase 5 — publish

Per the protocol: equal shard/definition/reference/occurrence counts between a
cache-hit and a fresh shard-loaded process; a fixed query corpus compared
normalized in-private; artifacts under `benchmarks/monolith/<date>-<rev>/`
(`environment.json`, `runs.jsonl`, `summary.md`, charts), each gate marked
pass/fail with evidence.

## Open items before "measurement complete"

- [ ] Split `shards.scan` from `shards.decode` on the discovery path (currently combined).
- [ ] Add a corrupt-cache fixture assertion to the self-test.
- [ ] Decide cold-storage handling (deferred here) and record it.
- [ ] `probe` is single-in-flight; add true concurrent open-loop sending + bounded
      retained payloads if a scenario needs overlapping requests.
- [x] Run `cargo fmt` + `cargo clippy -D warnings` — now available via `shell.nix`; clippy is clean on the harness crates.
- [ ] Pre-register host GiB caps / effective cgroup limit before the first counted run.

### Follow-ups from the first counted run

The first counted run against the Monolith ([results](monolith-measurement-results.md))
surfaced these, in rough priority order:

- [x] **Async index build** — cache reads, cold shard decode, and optional
      automatic indexing now run after `initialize` on one startup worker;
      `jabar/status` reports readiness and startup outcome.
- [ ] **Faster warm start** — cache-hit is still ~57 s, deserialize-bound on the
      10 GiB snapshot; consider an mmap/zero-copy format or lazy section loading.
- [ ] **`workspace/symbol` perf** — p95 ~290 ms over 3.1M defs, above the 250 ms
      gate; add a persisted name index (prefix/trigram/fst) instead of scanning.
- [ ] **Peak memory** — ~67 GiB peak vs ~17 GiB resident; stream/drop raw shards
      during decode and avoid large transient buffers on the cache-load path.
- [ ] **`startup` cache publication** — add `--hold-secs`/quiesce so the workload
      waits for `cache.write` to publish `CURRENT` before shutdown, making
      cache-miss → publish → cache-hit a single workload.
