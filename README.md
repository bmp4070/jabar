# jabar

A focused Java navigation language server for Bazel monorepos, built for AI
coding agents first and editors second. Jabar serves compiler-produced SCIP
indexes locally, reports index readiness and reference truncation through custom
methods, and handles edited files conservatively.

## Status

**All nine client operations work end to end** — `workspace/symbol`,
`textDocument/definition`, `textDocument/references`, `textDocument/hover`,
`textDocument/implementation`, `textDocument/documentSymbol`, and the call
hierarchy trio. Given SCIP shards produced by the aspect in
`crates/build-model/aspects/`, `jabar` answers symbol searches across all loaded
shards with real ranges, converted into the client's negotiated position
encoding. Before an index is loaded it returns an LSP *error* rather than an
empty list, because a client cannot tell those apart.

No capability is advertised that cannot be served.

| Crate | State |
| --- | --- |
| `paths` | Absolute UTF-8 paths with explicit real and virtual forms. |
| `vfs` | File ids, path interning, change batching, and revisions. |
| `telemetry` | Misbehaviour and lifecycle telemetry. |
| `watcher` | Refresh scheduling when shards or git state change. |
| `overlay` | Tree-sitter declarations for open files newer than the index. |
| `base-db` | Salsa inputs, durability tiers, and the VFS bridge. |
| `symbol-index` | SCIP ingestion, search, cursor resolution, definitions, references, implementors, and per-file symbols. |
| `build-model` | Bazel labels, aquery parsing, aspect execution, and CLI queries. |
| `jabar-server` | LSP transport, the nine operations, index lifecycle, cache, and status extensions. |

## Why this exists

Jabar turns Java SCIP shards produced through Bazel's action graph into a local,
editor-neutral LSP service. The bundled scip-java aspect performs compiler-based
symbol resolution and captures generated sources in the configured build. With
the default output base, Jabar canonicalizes the workspace `bazel-bin` path.
With an explicit output base, it asks that configured Bazel invocation for its
output path. It then loads the discovered shards into one navigation index. It
does not currently use BSP.

The primary clients are coding agents that use nine navigation operations:
`workspaceSymbol`, `goToDefinition`, `findReferences`, `hover`,
`documentSymbol`, `goToImplementation`, and the call hierarchy trio. Completion,
diagnostics, rename, signature help, inlay hints, semantic tokens, code actions,
and formatting are outside the current scope. Mature Java IDE servers provide
those editing features; Jabar concentrates on cross-target repository
investigation and local deployment. `jabar/status` makes readiness and
verification visible, while `jabar/references` reports the loaded-index total
when its 5,000-result response is truncated. Workspace-symbol and call-hierarchy
responses have smaller caps and do not currently report totals to clients.

The global index covers the SCIP shards that are present and successfully
loaded. Jabar does not yet prove that every requested Bazel target produced a
shard, so an empty result is not a guarantee that no unindexed target contains a
match. See [Bazel ecosystem positioning](docs/bazel-lsp-positioning.md) for a
comparison with JDT LS, IntelliJ/BSP, Sourcegraph, Kythe, rust-analyzer, clangd,
and Starlark language servers.

## Design notes

`docs/phase-1.md` records the original design investigation and milestone plan.
The current large-workspace work is tracked in `docs/monolith-roadmap.md`.

The server uses a synchronous protocol event loop. Startup discovery, cache or
shard loading, optional automatic indexing, watcher setup, refresh scans,
replacement index construction, cache writes, and retired-index destruction run
on bounded workers. Completed generations return to the event loop for
publication. Query handlers run synchronously against the published index. The
`base-db` crate contains Salsa infrastructure, but the running server does not
currently depend on it. Its query path uses the VFS, overlay, and eagerly loaded
global SCIP index directly.

Two Java-specific design constraints shape the index:

- **Dependencies often arrive as binaries.** Most of a target's classpath is
  jars, and the JDK's own types live in `lib/modules`, a jimage archive.
- **A shallow global index serves repository-wide operations.** Six of the nine
  client operations are repo-wide. A deeper per-target semantic slice remains
  planned rather than part of the running server.

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

The SCIP golden tests in `crates/symbol-index/tests/fixture.rs` run only when
`JABAR_SCIP_DIR` points to shards generated from the fixture. Ordinary CI builds
the Bazel fixture and tests the workspace model, but does not install
scip-java, regenerate those shards, or exercise the golden SCIP assertions.

## Install

Server archives are published for x86-64 and ARM64 Linux and macOS. They are
built on Ubuntu 24.04 and macOS 15; compatibility with older glibc or macOS
versions has not been established. Choose an explicit version so installation
does not silently select a newer release:

```sh
./scripts/install.sh 0.1.0
export PATH="$HOME/.local/bin:$PATH"
jabar --version
```

The installer downloads the named archive and `SHA256SUMS` from that GitHub
release, verifies the archive before extraction, requires exactly three regular
files, and then installs only `jabar`. Release assets can be replaced by a
repository administrator, so an explicit version and a checksum downloaded from
the same release do not make those assets immutable or independently
authenticated. Set `JABAR_INSTALL_DIR` or pass a second argument to choose
another destination. For a manual install, download the
matching `jabar-v0.1.0-<target>.tar.gz` and `SHA256SUMS` from the same release,
select that artifact's line, and verify it before extraction:

```sh
grep ' jabar-v0.1.0-x86_64-unknown-linux-gnu.tar.gz$' SHA256SUMS | sha256sum -c -
# macOS: replace sha256sum with `shasum -a 256`
```

Jabar uses `scip-java` only when it needs to produce or refresh SCIP shards. It
is a separate runtime dependency and is never downloaded or executed by the
installer. Install it from the
[upstream scip-java project](https://github.com/scip-code/scip-java), verify the
artifact using the upstream release information, and either put it on `PATH` or
set `index.scipJava` to its absolute path. The bundled aspect tracks upstream
commit `0e47f47c4aebf47ce7f739eb51fa50938f3356d5` plus the source-jar fix described
in [`crates/build-model/aspects/README.md`](crates/build-model/aspects/README.md);
the older scip-java 0.12.3 executable is not a compatible tested pairing.

To build from source instead:

```sh
cargo build --release --locked --package jabar-server --bin jabar
mkdir -p "$HOME/.local/bin"
install -m 0755 target/release/jabar "$HOME/.local/bin/jabar"
```

The 0.1.0 GitHub release contains the server binary only. The VS Code shim is
compiled in CI but is not published as a VSIX or to the Marketplace; use it
from `editors/vscode/` as a development extension.

## Configuration

See `docs/configuration.md`. Everything is optional; the defaults work.

## Editors

`editors/claude-code/` registers jabar as Claude Code's Java language server —
a local plugin marketplace, since that is how Claude Code discovers servers.
`editors/vscode/` holds a VS Code extension. The server advertises its
implemented navigation capabilities during initialization. Until the background
startup worker publishes an index, navigation requests return an explicit
`IndexNotReady` error and `jabar/status` reports loading progress. VS Code needs
an extension to spawn a custom binary.

## Next steps

### Large repository startup and indexing

The first counted large-workspace run loaded 6,876 SCIP shards containing 42.6
million occurrences. Cache format v2 reduced the snapshot to 3.74 GiB, warm
load to about 22 seconds, warm peak RSS to 24.6 GiB, and steady RSS to about 7.3
GiB. These are a small number of samples on one host and do not establish a
production SLO or superiority over another language server. See
[`docs/monolith-measurement-results.md`](docs/monolith-measurement-results.md)
for the environment, limitations, and unmeasured workloads.

The cache is a performance aid, not proof that an index matches current sources.
Jabar persists the built index after loading shards from `bazel-bin`, validates
cache provenance, reconciles shard metadata, and performs refresh work off the
LSP loop. Cached sessions avoid a recursive `bazel-bin` watcher. Verification
that the cache matches current shards does not prove that those shards match
current sources. Complete target coverage remains to be implemented. Startup
loading now runs on a bounded worker after `initialize`; the remaining latency
is time to index readiness and the first correct query. See
[`docs/index-cache.md`](docs/index-cache.md) for the cache design and
[`docs/monolith-roadmap.md`](docs/monolith-roadmap.md) for implementation stages,
measurements, ECJ coverage, and query-scale work.

## Indexing a repo that compiles with ECJ

scip-java re-runs compilation with `javac` and a SCIP compiler plugin. ECJ
targets can supply options that `javac` rejects, such as `-preserveAllLocals`
and `-Xecj_use_direct_deps_only`. The bundled aspect also selects only actions
whose mnemonic is `Javac`; an ECJ target with a different action mnemonic may
be skipped before its options are examined. Both behaviors need verification
against representative targets.

The ECJ option policy belongs in a maintained aspect or upstream change, with
tests for the actual toolchain and flags. Hand-editing `.jabar/aspects/` is not a
supported workaround: Jabar overwrites that copy on the next index build.
`--keep_going` permits partial output, so the build must also report failed and
skipped targets rather than silently presenting their symbols as absent. The
work and acceptance criteria are in [`docs/monolith-roadmap.md`](docs/monolith-roadmap.md).

## License

Apache-2.0. See `LICENSE` and `NOTICE`.

`crates/build-model/aspects/scip_java.bzl` is derived from the
[upstream scip-java aspect](https://github.com/scip-code/scip-java) (Apache-2.0,
© 2022 Sourcegraph, Inc.) and carries one documented source-jar fix. Upstream
supports Bazel 9 and bzlmod. See `NOTICE` and the
[aspect instructions](crates/build-model/aspects/README.md).
