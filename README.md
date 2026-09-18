# jabar

A salsa-backed Java language server for Bazel megarepos, built for AI coding
agents first and editors second.

## Status

**All nine client operations work end to end** — `workspace/symbol`,
`textDocument/definition`, `textDocument/references`, `textDocument/hover`,
`textDocument/implementation`, `textDocument/documentSymbol`, and the call
hierarchy trio. Given SCIP shards produced by the
aspect in `crates/build-model/aspects/`, `jabar` answers symbol searches across
a whole Bazel repo with real ranges, converted into the client's negotiated
position encoding. Before an index is loaded it returns an LSP *error* rather
than an empty list, because a client cannot tell those apart.

No capability is advertised that cannot be served.

| Crate | State |
| --- | --- |
| `paths` | Absolute UTF-8 paths. Done, 7 tests. |
| `vfs` | File ids, path interning, change batching, revisions. Done, 27 tests. No loader yet. |
| `telemetry` | Misbehaviour detection. Done, 21 tests. |
| `watcher` | Reloads the index when shards or git state change. Done, 10 tests. |
| `overlay` | tree-sitter declarations for files the index cannot see yet. Done, 11 tests. |
| `base-db` | Salsa inputs, three durability tiers, VFS bridge. Done, 14 tests. |
| `symbol-index` | Reads SCIP shards: search, cursor resolution, definitions, references, implementors, per-file listing. Done, 27 tests. |
| `build-model` | Bazel labels, aquery parsing, CLI queries. Done, 34 tests (8 hit real bazel). |
| `jabar-server` | LSP shell plus the three query handlers. Done, 61 tests. |

## Why this exists rather than using an existing Java LSP

The usual Java language servers walk the filesystem to discover sources and
index the whole workspace eagerly. Neither survives a repo with millions of
files. jabar takes its file list from Bazel via BSP, and computes only what a
query actually demands.

The second departure is the consumer. The primary clients are Claude Code and
Copilot, which issue nine operations — `workspaceSymbol`, `goToDefinition`,
`findReferences`, `hover`, `documentSymbol`, `goToImplementation`, and the call
hierarchy trio. Six of those are repo-global. Completion, signature help, inlay
hints, semantic tokens and formatting are never requested, and are out of scope.
That deletes the hardest latency constraint a language server normally carries,
and moves the pressure onto cross-target queries instead.

## Design notes

`docs/phase-1.md` is the working plan: scope, milestones, exit gate, and the
findings behind each decision.

Structure follows rust-analyzer: a synchronous event loop with one writer and
many snapshot readers, not an async runtime. Salsa cancellation works by taking
`&mut db`, which unwinds in-flight readers; that model wants a single writer.

Two departures from rust-analyzer, both forced by Java:

- **Dependencies arrive as binaries.** Most of a target's classpath is jars, and
  the JDK's own types live in `lib/modules`, a jimage archive. rust-analyzer has
  no analogue — every dependency it sees is source.
- **A shallow global index sits alongside the deep per-target slice.** Six of the
  nine client operations are repo-wide, so a purely lazy slice has nothing to
  answer them with.

Debugging is DAP, a separate protocol the agent clients do not speak. Breakpoint
and frame mapping lands in Phase 2 off the item tree; expression evaluation waits
for Phase 3. The adapter itself will be `java-debug` driven as a subprocess
rather than a JDI client written here.

## Test fixture

`fixtures/megarepo/` is a 12-target Bazel Java workspace built to make every
query jabar must answer have exactly one checkable right answer — including a
binary-only jar dependency, a generated source, a file owned by no target, and a
file whose offsets differ across UTF-8, UTF-16 and codepoints.

```
cd fixtures/megarepo
bazel build //...
bazel run //java/com/acme/app:app
```

See `fixtures/megarepo/EXPECTATIONS.md` for the golden answers.

## Development

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Toolchain is pinned in `rust-toolchain.toml`.

## Configuration

See `docs/configuration.md`. Everything is optional; the defaults work.

## Editors

`editors/claude-code/` registers jabar as Claude Code's Java language server —
a local plugin marketplace, since that is how Claude Code discovers servers.
`editors/vscode/` holds a VS Code extension. Any LSP client works — the server
advertises its capabilities statically once it finds an index, so nothing is
client-specific — but VS Code needs an extension to spawn a custom binary at all.

## Next steps

### Index discovery should not walk all of `bazel-bin`

`discover_index` (`crates/jabar-server/src/server.rs`) reads shards by handing
`bazel-bin` to `SymbolIndex::from_dir` (`crates/symbol-index/src/lib.rs`), which
recurses the whole tree and stats every entry, pruning only `*.scip-targetroot`
and `*.semanticdb`. On a small repo that is ~350ms; on a megarepo it is not. On
Salesforce core, `bazel-bin` holds **2.68M files** and the startup walk takes
**~46s** — paid whether one shard is found or none, because "are there shards?"
currently means "look everywhere." It runs before the server answers
`initialize`, so the editor blocks on it.

Locating shards and keeping them fresh are separate problems, and the walk
conflates them. The index is a build output: it is only ever as fresh as the last
aspect run, so the walk is *not* what makes results current — re-running the
aspect is. That splits the work cleanly:

- **Load fast.** Write the produced index to `.jabar/index/` as one concatenated
  `index.scip` (the aspect README already suggests
  `find bazel-bin -name '*.scip' … | xargs cat`) or a shard-path manifest, and
  check `.jabar/index` *before* `bazel-bin` in `discover_index`, short-circuiting
  on a hit. Startup becomes one read instead of 2.68M stats.
- **Trust it cheaply.** Stamp the written index with what it was built from (git
  revision / content hash / newest shard mtime). On startup, compare the stamp;
  a match takes the fast path, a miss falls back to a rescan or rebuild. This
  avoids both the walk *and* an unconditional Bazel call in the common case.
- **Refresh in the background, never on the hot path.** A truly-fresh index means
  an incremental aspect build (`bazel build //... --output_groups=scip`), which
  also reports where the outputs are — discovery and refresh in one. But on core
  even a no-op incremental build pays minutes of Bazel analysis, so it must not
  run synchronously at startup. Load the cached index instantly, serve queries,
  and swap a fresh one in when the background build lands. jabar already has the
  machinery: `Server::adopt_index` swaps an index in, and the `watcher` crate
  exists to reload when shards or git state change.

For the primary consumer — an AI agent navigating code — a minutes-old index is
almost always acceptable and far better than a 46s stall or an honest "no index"
error, so *instant but slightly stale, refreshing in the background* is the right
default. Scoping the walk to `index.targets` helps a scoped config but not a
`//...` one; a parallel walker (`ignore`/`jwalk`) softens the cold path but still
costs O(files). The fast path plus stamp is the change that actually removes the
startup tax.

## Indexing a repo that compiles with ECJ

scip-java indexes by re-running the target's compilation with stock `javac` plus a
`semanticdb` plugin. Repos that compile with the Eclipse compiler (ECJ /
`JdtJavaBuilder`) instead of `javac` — Salesforce core is one, via a mixed
ECJ/javac Bazel toolchain — carry **ECJ-only options in each target's
`javac_options`** (`-preserveAllLocals`, `-Xemacs`, `-Xecj_use_direct_deps_only`,
`-Xecj_problem_severity_preferences=…`, `-warn:none`). Stock `javac` rejects these
outright (`error: invalid flag: -preserveAllLocals`, exit 2), and the
`ScipJavaIndex` action fails for every ECJ-built target. ECJ cannot load the
`semanticdb` javac plugin, so pointing scip-java at ECJ is not an option — the fix
is to **strip the ECJ-only options before scip-java runs `javac`**, exactly as such
repos already do on their own javac toolchain when a target opts out of ECJ.

The strip: when assembling `javac_options`, drop the exact flags
`-preserveAllLocals`, `-Xemacs`, `-warn:none` and the prefixes `-Xecj_`, `-warn:`,
`-err:`, `-Xep:`, keeping javac-valid options (`-nowarn`, `-parameters`, `-g`,
`-encoding`, `-source`/`-target`). Point the index build at the same JDK the repo
compiles with (for core: `onejdk21`, `-source/-target 17`) via
`--define=java_home=…`. Run with `--keep_going`: the strip clears the invalid-flag
failures, but any target whose sources ECJ accepts and stricter stock `javac`
rejects still fails *that* target — those degrade to gaps in the index rather than
aborting the run.

The bundled aspect (`crates/build-model/aspects/scip_java.bzl`) is kept as the
unmodified upstream snapshot (see License), so this strip currently lives as a
patch on the aspect copy installed into the target repo's `.jabar/aspects/`. Making
it default would mean either upstreaming an ECJ-option filter or teaching jabar to
inject one into the installed copy — worth doing before ECJ repos are a supported
target rather than a hand-patched one.

## License

MIT or Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.

`crates/build-model/aspects/scip_java.bzl` is an unmodified snapshot of the
[upstream scip-java aspect](https://github.com/scip-code/scip-java) (Apache-2.0,
© 2022 Sourcegraph, Inc.). Upstream now supports Bazel 9 and bzlmod, so Jabar
does not need a separate scip-java fork. See `NOTICE` and the
[aspect instructions](crates/build-model/aspects/README.md).
