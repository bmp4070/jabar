# Phase 1: Shell and Build Graph

A review of the proposed three-phase plan against how rust-analyzer actually
works, recalibrated for a server whose primary clients are Claude Code and
Copilot rather than a human in an editor.

Line references point at a rust-analyzer checkout and will drift. The operation
surface was verified against Claude Code's LSP tool; the classpath and
source-mapping claims were verified by attaching `jdb` to `fixtures/megarepo`.

## 1. Verdict

The phase ordering is sound and the instinct to let Bazel define the workspace is
right. But the original plan is designed around a human who opens a file, types,
and waits impatiently. That reader never shows up. The actual client issues nine
operations, six of them repo-global, from a cold process, and pays for every
result in context tokens.

Two consequences dominate. **Focus Target is the wrong entry point** for a client
whose first move is almost always `workspaceSymbol` across the repo. And **an
ephemeral client cannot amortize a warm cache**, which promotes the persistence
work the original plan filed under "Phase 3, optional."

**Net change to Phase 1.** Add a shallow global symbol and type-hierarchy index
alongside the deep focus slice. Promote on-disk persistence into the phase. Add
response shaping — ranking and truncation — as a first-class layer. Delete
completion, signature help, inlay hints, semantic tokens, code lens and
formatting from the roadmap entirely.

## 2. The consumer

Claude Code's LSP tool exposes exactly nine operations. That list is a scope
document, and what is absent from it decides more than what is present.

| Operation | Scope | Index tier it needs | Rank |
| --- | --- | --- | --- |
| `workspaceSymbol` | Global | Shallow name index across every target | 1 |
| `goToDefinition` | Global | Shallow index, then deep slice for the owning target | 2 |
| `findReferences` | Global | Reverse-dependency edges + per-target item trees | 3 |
| `hover` | Local | Deep slice for the current target only | 4 |
| `documentSymbol` | Local | Parse of one file. No graph needed. | 5 |
| `goToImplementation` | Global | Repo-wide supertype edges | 6 |
| `prepareCallHierarchy` | Local | Deep slice, current position | 7 |
| `incomingCalls` | Global | Reverse-dependency edges + body-level HIR | 8 |
| `outgoingCalls` | Local | Deep slice, one method body | 9 |

### What this deletes

No completion. No signature help, inlay hints, semantic tokens, code lens,
folding ranges, document highlight, formatting, or color provider. Copilot does
not route inline completions through your server either — it generates them
itself.

Completion is normally the largest and most latency-critical subsystem in a
language server; rust-analyzer devotes an entire crate to it. Cutting it removes
the hardest latency constraint in the project and a substantial share of the
eventual type-inference surface. Take the cut explicitly, because it also removes
the main argument for sub-100ms responses.

### What this promotes

- **Call hierarchy is three of nine operations.** It is a first-class feature
  here, not the afterthought it is in most servers, and `incomingCalls` needs
  exactly the repo-wide reverse-reference data that Focus Target defers.
- **`goToImplementation` needs a global type hierarchy.** In Java — interfaces
  everywhere, DI frameworks, generated stubs — this is heavily used and cannot be
  answered from one target's slice.
- **Diagnostics arrive through a different door.** Both agents use errors to
  self-correct after an edit. Support the LSP 3.17 pull model
  (`textDocument/diagnostic`); a push stream has no useful subscriber here.

### The second consumer: the debugger

The IDE matters mainly for debugging, and debugging is DAP — a separate protocol,
a separate process, and not something the agent clients speak at all. It still
belongs in this plan, because the one thing a Java debug adapter cannot work out
for itself under Bazel is exactly what jabar knows.

Attaching `jdb` to the fixture makes the gap concrete. The debuggee's entire
classpath is jars, one per Bazel target:

```
app.runfiles/_main/java/com/acme/policy/libpolicy.jar
app.runfiles/_main/java/com/acme/core/libcore.jar
app.runfiles/_main/third_party/tinyjson/tinyjson.jar
  … 12 jars, zero source paths
```

And the classfile inside carries only `Compiled from "DefaultRetryPolicy.java"` —
a bare filename with no directory. So a breakpoint on line 42 of a source file
has nothing connecting it to a loaded class, and a stack frame has nothing
connecting it back to a file the editor can open. Adapters built for Maven and
Gradle paper over this by assuming a conventional source layout. Bazel has none.

Only the build graph closes the loop: `libpolicy.jar` comes from
`//java/com/acme/policy`, whose `srcs` are the real paths. That is a
`buildTarget/sources` query joined to an item tree — both things jabar builds
anyway.

### Latency, recalibrated

An agent tool round trip already costs on the order of a second, so a 2–3s query
is fine; the original "<15s to first definition" target is comfortable. But it
optimizes the wrong metric. What hurts is **cold p50 on the first
`workspaceSymbol` of a session**, because agent sessions are short and may never
reach the warm state the whole design is built around.

## 3. Findings

Ordered by how expensive the mistake is to undo, not by sequence. The first three
follow from the consumer profile and the last two from adding debugging; the rest
hold regardless of who calls the server.

### F1 — Focus Target answers the question the agent asks last (blocking)

The original plan's core move — index only the opened file's target, ignore the
other 99% of the repo — assumes a developer working locally in a module they
already have open. An agent's session starts the other way round: "find the class
that handles retries," then definition, then references, then callers. Six of the
nine operations are repo-global. A lazily-sliced database has nothing to answer
them with.

The resolution is two tiers. A **shallow global index** — fully-qualified name,
kind, owning target, file, range, plus supertype edges — is small, cheap to build
from a Bazel aspect, and trivially serializable. It answers `workspaceSymbol`,
seeds `goToDefinition`, and makes `goToImplementation` possible. The **deep
per-target slice** stays exactly as originally described, demand-loaded when a
query needs bodies or types.

*Phase 1 action:* design both tiers now even though only the shallow one ships in
the phase. The tier boundary determines the Bazel aspect you write, and rewriting
an aspect after the fact means re-running it over the whole repo.

### F2 — Java dependencies are jars, and rust-analyzer has no answer (blocking)

Every rust-analyzer dependency is Rust source; its VFS, source roots, and HIR
pipeline all assume a dependency is a directory of parseable files. In a Bazel
Java repo, the bulk of a target's closure arrives as `.jar` outputs on the javac
classpath.

The choice determines Phase 2's parser story. Either require
`buildTarget/dependencySources` srcjars and parse Java throughout, or write a
classfile reader that lowers constant pool and method descriptors straight into
the item tree.

The classfile reader fits this consumer. A classfile *is* a pre-lowered item
tree, and it carries exactly the shallow-tier data — names, signatures,
supertypes — that the six global operations need. Parsing source you will never
edit, to recover information already encoded in the bytecode, is spent budget.

*Phase 1 action:* decide in M3. Prototype `buildTarget/javacOptions` and enumerate
one jar's entries through a virtual path, the hook for
`jar:///…!/java/lang/String.class`. Note that JDK types are not in a jar at all —
since Java 9 they live in `$JAVA_HOME/lib/modules`, a jimage archive (146MB on
JDK 26). A reader that only opens zip archives resolves nothing from the JDK,
which is most of what a Java file references.

### F3 — The agent edits files on disk, then queries immediately (blocking)

Humans type into an editor buffer, and `didChange` delivers the text before any
query arrives. Agents write files with their own tools and issue the next LSP call
within milliseconds. If your only path from disk to database is a filesystem
watcher, you will serve answers from before the write — a stale-read race that
does not exist for human clients, and one that surfaces as the agent "not seeing"
its own edit.

This makes text synchronization more important than the op list suggests, for a
different reason. If the harness syncs edits through `didOpen`/`didChange`,
ordering is free. Where it does not, you need an explicit barrier: re-stat the
touched paths and drain pending VFS changes before answering, or carry a version
token the client can wait on.

*Phase 1 action:* make read-after-write a tested invariant in M2. The test is:
write a file out of band, immediately issue a query, assert the new content is
reflected.

### F4 — Results are billed in tokens, and nothing in LSP knows that (rework)

`findReferences` on a common symbol in a Java megarepo returns tens of thousands
of `Location` values. For a human that is a scrollable panel; for an agent it is a
blown context window, or a client-side truncation that silently discards the
relevant hit.

A bare `Location` is also close to useless without surrounding text, so every
agent integration re-reads the file to see what is at the range — turning one
query into thirty file reads.

This wants a response-shaping layer with no rust-analyzer analogue: rank results
(same target first, then same package, then reverse-dependency distance),
truncate to a budget while reporting the true total, group by target, and attach
a line of surrounding context per hit. Deliver it through custom methods
alongside the standard ones; rust-analyzer's `lsp/ext.rs` is the pattern.

### F5 — Ephemeral sessions never reach the warm state (rework)

The original economics assume a server that starts once and gets warmer all day.
Agent sessions are short and often start a fresh process against a repo state that
has moved. Every session then pays cold cost, and the ~80% HIR cache-hit target is
only reachable if the cache outlives the process.

That reframes "Phase 3, optional" remote hydration as load-bearing. The full
remote HIR story can still wait, but the shallow global index is small, changes
only when the build graph changes, and persisting it is cheap. Persist it in
Phase 1 and the first `workspaceSymbol` of a cold session is a file read rather
than a Bazel query.

### F6 — The mmap VFS is not the lever, and this consumer makes it worse (rework)

rust-analyzer deliberately does not mmap. It stores owned `Vec<u8>` per change
(`vfs/src/lib.rs:150`) and its salsa input is `FileText { text: Arc<str> }`
(`base-db/src/lib.rs:239`). A mapped region raises `SIGBUS` when the file is
truncated underneath it — and an agent rewriting files continuously is a far
heavier source of that than a human hitting save. Text also needs UTF-8 validation
and line-ending normalization before it can become `Arc<str>`
(`global_state.rs:411`), which is a copy regardless of origin, and rowan interns
token text into the green tree at parse time, ending any borrow anyway.

The mechanism that delivers the win is salsa `Durability`. rust-analyzer marks
library text `HIGH` and workspace text `LOW` (`base-db/src/change.rs:97`), so an
edit cannot invalidate anything derived from a dependency. Bazel hands you that
partition for free.

*Phase 1 action:* build on `Arc<str>` inputs plus a content hash to suppress no-op
writes. Three durability tiers off the BSP partition. Drop mmap from the plan.

### F7 — "Instantly retrieve the dependency graph" overstates bazel-bsp (gap)

Query Skyframe, get the graph, no `stat` calls — that describes the steady state,
not the first call. `workspace/buildTargets` in bazel-bsp runs an aspect over the
workspace and can take minutes cold on a large repo. Budget for it, and cache to
disk keyed on the Bazel server's graph version, which F5 already requires.

The cheap query is `buildTarget/inverseSources`, file to owning target. Keep it as
the primitive behind `hover` and the other local operations, just not as the entry
point for the global ones.

*Free win:* BSP is JSON-RPC 2.0 over the same `Content-Length` framing as LSP.
`lsp-server`'s `Message`, `Connection` and `ReqQueue` are protocol-agnostic, so
one crate serves as both LSP server transport and BSP client.

### F8 — DAP is missing from all three phases, and most of it fits in Phase 2 (gap)

Debugging decomposes along the line the phases already use, which is why it slots
in rather than disrupting.

- **Breakpoint and frame mapping needs only the item tree.** Fully-qualified name
  per source file, line ranges per method, joined to the build graph's
  jar-to-target mapping. No type inference at all. That makes it a **Phase 2**
  deliverable — and arguably the cheapest real feature the item tree can support,
  cheaper than `goToDefinition`, which needs cross-target name resolution the item
  tree alone does not give you.
- **Expression evaluation needs inference.** Conditional breakpoints, watch
  expressions, and the debug console all evaluate Java at a program point. Those
  want exactly what **Phase 3** builds, so they wait.

Sequence the Phase 2 half *after* the item tree rather than beside it. As the item
tree's first consumer it is a useful forcing function: if breakpoint mapping is
awkward to express, the item tree is wrong.

*Phase 1 consequence:* the BSP client must fetch the *runtime* environment, not
just the compile classpath — BSP's JVM extension exposes run and test environment
queries alongside `buildTarget/javacOptions`. Small addition, but it has to be
known in M3, and whether your bazel-bsp version implements them is an M3 check
rather than an assumption.

### F9 — Do not write a JDI client in Rust (rework)

The temptation with DAP in scope is to build the adapter in Rust alongside
everything else. That means reimplementing JDI — a large, fiddly binary protocol —
plus stepping, thread control, breakpoint bookkeeping and evaluation. Months of
work whose output is a worse version of something that already exists.

JDI ships with the JDK (`jdk.jdi`, confirmed present on JDK 26), and Microsoft's
`java-debug` already implements DAP over it and is what VS Code's Java debugging
runs on. Run it as a subprocess and feed it the two things it cannot derive under
Bazel: the launch configuration, and the jar-to-source mapping.

The cost is a coupling to the custom commands `java-debug` expects from its
language server — a real dependency, but a documented and small one, and far
cheaper than owning a JDI implementation.

### One earlier finding, downgraded

An earlier draft flagged tower-lsp as a rework item on cancellation grounds. With
an agent client that de-prioritizes: agents do not cancel by typing, so the retry
path (`handlers/dispatch.rs:262`) matters much less. The write-side discipline
still holds — an agent writes ten files then queries, so `&mut db` bursts still
cancel in-flight reads, and holding a snapshot across an `.await` still stalls the
writer. Still use `lsp-server` and copy rust-analyzer's `GlobalState`/snapshot
split (`main_loop.rs:74`), but on simplicity grounds rather than as a correctness
blocker.

## 4. Revised scope

**In scope**

- LSP shell on `lsp-server`: lifecycle, capabilities, pull diagnostics, response
  shaping and truncation.
- VFS: path interning, `FileId` allocation, change batching, content hashing,
  virtual paths for jar entries, read-after-write barrier.
- BSP client: transport, the methods that matter (including the JVM run
  environment, for DAP in Phase 2), on-disk graph cache, `buildTarget/didChange`
  invalidation.
- Shallow global index: names, kinds, owning target, ranges, supertype edges —
  built from a Bazel aspect and persisted.
- Focus slice plumbing: `inverseSources` → transitive slice → durability-tagged
  salsa inputs.
- Agent-shaped latency instrumentation.

**Pulled forward.** From Phase 2, a skeletal salsa database — `FileText`,
`SourceRoot`, `FileSourceRoot` inputs and one trivial derived query, no parser —
because without it the VFS-to-salsa pipeline is untestable. From Phase 3, index
persistence, for the reasons in F5.

**Cut from the roadmap entirely.** Completion, signature help, inlay hints,
semantic tokens, code lens, folding ranges, document highlight, formatting,
rename, code actions. None are reachable from the client surface. If any appear in
a later phase document, that document has drifted.

## 5. Crate layout

Mirrors rust-analyzer's separation, minus what this consumer does not need, plus
the index tier it does. The rule that matters: no crate below `jabar-server`
performs I/O.

```
jabar/
├── crates/
│   ├── paths          absolute, UTF-8 paths — thin wrapper over camino
│   ├── vfs            FileId interner, change log, VfsPath (incl. jar:// virtual)
│   ├── bsp            BSP wire types + client, transport = lsp-server
│   ├── build-model    BSP → JavaWorkspace → source roots, classpath, durability
│   ├── symbol-index   shallow global tier: names, supertypes, persistence
│   ├── base-db        salsa inputs only in Phase 1
│   └── jabar-server   GlobalState, main_loop, dispatch, handlers, response shaping
└── xtask/             bench + fixture tooling
```

rust-analyzer's `vfs` is close enough to vendor rather than rewrite — roughly
1,300 lines, and the only Rust assumption in it is a convenience constructor for
`.rs` globs. Its `base-db` `Files` and durability machinery is worth porting
structurally.

## 6. Milestones

Ordered because each unblocks the next. Week estimates assume one or two
engineers and are the softest part of this document.

| ID | Milestone | Exit criterion | Wk |
| --- | --- | --- | --- |
| M0 | Skeleton and harness | Fixture repo builds; `tracing` spans emit; CI green. | 1 |
| M1 | LSP shell | Claude Code connects and gets a well-formed answer to `documentSymbol`. Truncation layer returns a stated total, not a silently clipped list. | 1–2 |
| M2 | VFS and salsa inputs | A test writes a file out of band, immediately queries, and sees the new content. A second test proves a `LOW`-durability edit does not invalidate `HIGH`-durability derived values. | 2–3 |
| M3 | BSP client | Handshake with bazel-bsp; targets, sources, javacOptions and the JVM run environment cached to disk; the jar-versus-srcjar decision written down with a working spike behind it. | 3–5 |
| M4 | Shallow global index | `workspaceSymbol` answers across the whole fixture repo from a cold process in under a second, served from the on-disk index. | 5–7 |
| M5 | Focus slice | A local query loads exactly its target's slice and nothing more; a `BUILD` edit re-slices and invalidates the affected shard, debounced. | 7–8 |
| M6 | Instrumentation | Cold p50 for `workspaceSymbol` and `goToDefinition` measured on a real target, not just the fixture. Reference fan-out distribution recorded to size the truncation budget. | 8 |

**Schedule risk.** M3 and M4 are where this slips, and they share a root cause:
nobody has measured bazel-bsp or a custom aspect against a repo of your size. If
M3 threatens M4, stub `JavaWorkspace` from a checked-in JSON dump and build M4
against it — the index design is worth proving even on fake graph data, and M4 is
the milestone this consumer actually needs.

## 7. Decisions Phase 1 must close

Each gets baked into a type signature early. Deferring means rewriting Phase 2.

| Question | Recommendation |
| --- | --- |
| How do external dependencies enter the database — jars, srcjars, or both? | Classfile reader as the primary path, srcjars opportunistically when BSP offers them. Bytecode already carries the names, signatures and supertypes the shallow tier needs. |
| Build the debug adapter, or drive an existing one? | Drive `java-debug` as a subprocess. Supply the launch config and jar-to-source mapping — the two things it cannot derive under Bazel — and let it own JDI, stepping and thread control. |
| Does the shallow index live in salsa, or beside it? | Beside it, as a plain persisted structure keyed by target. It changes only when the build graph changes, so putting it under salsa buys invalidation you do not need and costs a serialization story you do. |
| Does `VfsPath` carry jar-internal entries, or do jars get extracted to a cache directory? | Virtual paths. Extraction adds an I/O and invalidation problem the design is explicitly trying to avoid. |
| What is the unit of durability? | Three tiers. Focus target `LOW`, same-repo transitive deps `MEDIUM`, external jars `HIGH`. The middle tier is the one rust-analyzer lacks and a megarepo needs. |
| How does the client learn that a result was truncated? | A custom method returning an explicit total and a ranking rationale, with standard `textDocument/references` kept as a conformant fallback for Copilot. Silent truncation makes an agent confidently wrong. |
| Own a Bazel daemon connection, or shell out per query? | A long-lived bazel-bsp subprocess. Per-query `bazel` invocations pay JVM and analysis-cache warmup every time — the exact cost this design exists to avoid. |
| What happens when a file belongs to no target, or to several? | Several: pick deterministically and log. None: index standalone with an empty classpath rather than failing. Agents hit generated and scratch files constantly. |

## 8. Exit gate

Phase 1 is done when all of these hold.

- [ ] Claude Code connects and answers `workspaceSymbol` across the whole fixture
      repo from a cold process.
- [ ] The shallow index survives a process restart and is served from disk, not
      rebuilt.
- [ ] A file written out of band is reflected in the very next query —
      read-after-write is a test, not a hope.
- [ ] A high-fan-out reference query returns a ranked, truncated result with the
      true total stated.
- [ ] A test asserts that editing a workspace file leaves dependency-derived salsa
      values intact.
- [ ] A `BUILD` edit re-slices and invalidates the affected shard, debounced,
      without a restart.
- [ ] Cold p50 for the top two operations is recorded for a real target.
- [ ] The classpath decision is written down with the spike that justifies it
      committed.
- [ ] No parser and no completion code exist in the tree. If either crept in,
      scope was not held.

## 9. rust-analyzer files worth reading first

Roughly in the order Phase 1 needs them.

| Path | Why |
| --- | --- |
| `lib/lsp-server/src/` | The whole transport. Small enough to read end to end; doubles as your BSP client. |
| `crates/rust-analyzer/src/main_loop.rs` | `Event` enum, event loop, task handling. The spine of M1. |
| `crates/rust-analyzer/src/global_state.rs` | State/snapshot split and `process_changes` — the VFS-to-salsa bridge. |
| `crates/rust-analyzer/src/lsp/ext.rs` | How to add custom methods without breaking conformance. The template for response shaping. |
| `crates/vfs/src/` | Change batching, path interning, virtual paths. Vendor candidate. |
| `crates/base-db/src/change.rs` | Ninety lines containing the entire durability strategy. |
| `crates/ide-db/src/symbol_index.rs` | The closest existing analogue to the shallow global tier — read before designing M4. |
| `crates/load-cargo/src/lib.rs` | Workspace model to source roots to file sets. Template for `build-model`. |
| `crates/rust-analyzer/src/discover.rs` | Lazy per-file project discovery over a subprocess — closest analogue to the focus-slice flow. |
| `crates/rust-analyzer/src/integrated_benchmarks.rs` | How to measure incrementality honestly. Copy the approach in M6. |

---

Phases 2 and 3 need reworking against §2 — the two-tier index, the completion cut
and the DAP split all reshape them.
