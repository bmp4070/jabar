# jabar

A salsa-backed Java language server for Bazel megarepos, built for AI coding
agents first and editors second.

## Status

Foundation only. The fixture is complete and the Rust workspace compiles; no
language server exists yet. `jabar` currently exits non-zero and says so.

| Crate | State |
| --- | --- |
| `paths` | Absolute UTF-8 paths. Done, 7 tests. |
| `vfs` | File ids, path interning, change batching, revisions. Done, 27 tests. No loader yet. |
| `telemetry` | Misbehaviour detection. Done, 15 tests. |
| `bsp` | Not started. Build Server Protocol client. |
| `jabar-server` | Not started beyond a stub binary. |

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
