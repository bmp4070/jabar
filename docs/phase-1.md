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

Ordered by how expensive the mistake is to undo, not by sequence. F1–F3 follow
from the consumer profile, F8–F9 from adding debugging, F10–F12 from measuring
Bazel and tree-sitter rather than assuming, and F13–F18 from an external review
of this document and the decisions that followed it; the rest hold regardless of who calls the server.

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

### F10 — Bazel already builds the item tree (blocking)

Measured against `fixtures/megarepo`, not assumed. Every entry on a target's
javac classpath is a *header jar*: `libpolicy-hjar.jar`, never `libpolicy.jar`.
Confirmed with `javap` — class names, supertypes, interfaces and full method
signatures, with the `Code` attribute stripped:

```
FULL jar                          HEADER jar
public boolean shouldRetry(...)   public boolean shouldRetry(...)
  Code:                           public int maxAttempts();
     0: iload_1  …                  ← no body at all
```

That is an item tree, in the sense F2 and Phase 2 both mean. Bazel builds one
per target as part of every compile, caches it remotely, and invalidates it
correctly. It is the artifact M4 was going to write an aspect to produce.

The limit is as sharp as the opportunity. Header jars carry **no `SourceFile`
and no `LineNumberTable`** — verified. They answer "does this symbol exist and
what shape is it" and never "where is it". Positions need a second, lazy step
against the source file `aquery` already names.

*Phase 1 action:* build the shallow tier by reading header jars rather than by
authoring a Bazel aspect. Resolve positions lazily, per result. See the M3 gate
below for the measurement that decides whether an aspect becomes necessary
after all.

### F11 — Skyframe is not externally reachable, and aspects are the only alternative to query (gap)

The PDF proposes querying "Bazel's Skyframe" directly. There is no such door:
Skyframe is internal to the Bazel server's JVM, and an outside process reaches
it only through `query`, `cquery`, `aquery`, or an aspect. Those *are* the
Skyframe API. Nothing lower is available to a Rust process, so the real choice
is aspects versus the query family.

| | Aspects | Query family |
| --- | --- | --- |
| Runs as | build actions | loading / analysis phase |
| Remote-cacheable | **yes** | no |
| Needs a repo change | yes, a `.bzl` file | no |
| Push invalidation | via the build | no, polling only |
| Measured on the fixture | not authored | 67–170ms per query |
| Version coupling | rules_java internals | JavaBuilder flag names |

Both are fragile, differently. The aspect flavour is already visible: the
`JavaInfo` provider key is `@@rules_java+//java/private:java_info.bzl%JavaInfo`
under bzlmod, not `"JavaInfo"` — it moved into Starlark and the key is
version-qualified.

The aspect's real advantage is the remote cache, which is the only mechanism
that answers F5. But given F10, the shallow tier needs no aspect: the artifact
already exists and is already cached. An aspect earns its place only when
something is needed that the build does not already produce — which, concretely,
means positions.

*Recommendation:* query family now, aspect only if the M3 gate says so.

### F12 — A parser is needed later and smaller than the plan assumes, and should be bought (rework)

Header jars answer the global tier without parsing anything, so the parser
arrives at M4 rather than M2 — and its first job is narrow: **positions**. Every
LSP response is a `Location { uri, range }`, and a header jar cannot say where in
the file a declaration sits. Bodies, references and call hierarchy are Phase 2
and want a full tree; position resolution wants only "find this declaration and
give me its range".

rust-analyzer wrote its own parser because Rust had no error-tolerant one and it
needed rowan's specific properties. That reasoning does not transfer. Java has
`tree-sitter-java`, and it does the two hard things off the shelf. Measured
against `fixtures/megarepo`:

| | |
| --- | --- |
| Full parse throughput | 19.3 MB/s |
| Incremental reparse, one keystroke in a 750KB file | 501µs (77× faster) |
| `Tree: Send + Sync` | yes — survives the salsa boundary |

Correctness on the cases the fixture exists to be nasty about: `record` syntax
parsed clean, the non-ASCII `grüße` identifier extracted correctly, and a file
with `int x = ;` plus an unclosed brace reported 3 errors while still recovering
all 3 declarations. That last property decides whether the server is useful
*during* editing or only after it, which for an agent client is most of the time.

The numbers also confirm the two-tier split rather than undermining it. A typical
8KB file is ~0.4ms, so a 500-file focus slice is ~250ms single-threaded and
trivially parallel — but a 10GB megarepo would be ~500 seconds, so parsing
everything is off the table and the header-jar tier has to carry the global
queries.

Limits worth stating: it produces a CST, not a typed AST, and carries no name
resolution or types. Neither matters much, because lowering to our own item tree
and HIR happens regardless and name resolution is Phase 2/3 work under any
parser. New Java syntax needs a grammar update, which is someone else's
maintenance — the point of buying.

*Staging:* nothing now; `tree-sitter-java` at M4 for position resolution only
(roughly a day's work); the same trees feed the item tree and body lowering in
Phase 2, so nothing is thrown away.

### F13 — jabar and the agent share one Bazel server, and Bazel serializes (blocking)

Raised by an external review of this plan; nothing in F1–F12 covers it.

The primary client *runs Bazel itself*. An agent's loop is edit →
`bazel build`/`test` → query the server. Two properties of Bazel then dominate
everything the earlier measurements captured:

- **The workspace lock is exclusive.** One command per output base. Every
  `same_pkg_direct_rdeps` and every `aquery` queues behind the agent's own
  multi-minute builds — and, worse, jabar's queries block the agent's builds. A
  language server that intermittently stalls the client's build tool is a server
  that gets switched off.
- **The analysis cache is discarded when flags change.** The warm 67–170ms
  figures assume the resident server's analysis cache is warm *for jabar's
  flags*. Agents invoke Bazel with their own `--config`, and each flip pays full
  re-analysis. This was demonstrated accidentally while verifying the review:
  changing one flag printed `Build option --min_param_file_size has changed,
  discarding analysis cache (this can be expensive)`.

The 12-target fixture cannot surface either, because nothing else is using its
Bazel server.

This also exposed a contradiction in §7 that survived an earlier rewrite: one row
decided "the CLI, not BSP" while another still said "a long-lived bazel-bsp
subprocess", justified by JVM warmup that does not exist — the Bazel client is a
thin binary talking to a resident server, which is precisely why warm queries are
fast. The second row is now corrected.

*Phase 1 action:* measure contention at the M3 gate. The likely mitigation is a
dedicated `--output_base` for jabar, which decouples both the lock and the
analysis cache at the cost of a second analysis universe. Also consider pinning
jabar's flags to the repo's `.bazelrc` defaults so cache flips are rare, and
`--noblock_for_lock` with retry so a lock wait surfaces as a visible
`Failure::BuildServer` rather than a silent stall.

### F14 — Header jars are build outputs, and this plan treated them as a source of truth (blocking)

F10 stands as far as it goes: the artifact format already exists and no aspect
need author one. But header jars are *outputs*, and three consequences were
missed.

- **Never-built targets.** A developer's `bazel-out` holds outputs for the
  slices they have built — a small fraction of a megarepo. An index promising
  coverage "across every target" would be reading artifacts that mostly do not
  exist locally.
- **Built without the bytes.** With remote execution and
  `--remote_download_toplevel` or `minimal` — standard on repos this size —
  header jars are *intermediate* outputs, exactly what is not downloaded. A fully
  built repo can still have an empty header-jar shelf.
- **Staleness, the sharpest.** A header jar reflects the last build, not current
  source. The agent writes `NewRetryPolicy.java` and immediately asks
  `workspaceSymbol("NewRetryPolicy")`. The index searched and found nothing, so
  it reports `EmptyReason::NoMatch` — the one reason classified as *healthy*. The
  precise failure the telemetry crate exists to catch passes through it
  undetected, because staleness of the index's source material is not in the
  vocabulary. The exit gate's "read-after-write is a test, not a hope" and
  "`workspaceSymbol` served from the on-disk index" are in direct tension for any
  symbol created since the last build.

Not fatal, but it changes M4. The index needs an explicit three-way source model
per target: header jar present, read it; absent, materialize or degrade honestly
as `TargetNotLoaded`, never `NoMatch`; source newer than the jar, overlay from
parsed source. The overlay widens F12 slightly — tree-sitter covers declarations
of dirty files, not only positions — which is cheap since the parser is already
in the tree at M4.

Materializing header jars for never-built targets probably does require a small
aspect after all, one whose only job is to request `JavaInfo.compile_jars` as an
output group so `bazel build --aspects=… --output_groups=…` fetches them. Turbine
header jars depend only on direct deps' header jars, so cache hit rates are
excellent. F11's conclusion narrows rather than reverses: **do not author an
aspect to produce an item tree; you may need one to fetch it.**

Two mechanical consequences: add `EmptyReason::IndexStale`, and track index
generation against `vfs::Revision`, which already exists.

### F15 — `findReferences` and `incomingCalls` have no mechanism (blocking)

Three of the nine client operations need call-site data, and neither tier holds
it. The shallow tier has names, kinds and supertypes; the deep slice has one
target's bodies. §2 of this document claims "reverse-dependency edges +
per-target item trees" — but item trees do not contain call sites, and the
rdeps-scoping trick collapses exactly where it matters. The fixture's own
high-fan-out case makes the point: `Preconditions.checkNotNull`'s reverse
dependency closure in a real repo is approximately the whole repo.

rust-analyzer survives this with a text-search candidate pass over a workspace it
can afford to scan. That is not available here.

So one of two things has to be budgeted, and neither is currently in the plan:

- a persistent identifier-occurrence or trigram index — a code-search component,
  and the largest unbudgeted item in the project; or
- per-target reference tables emitted at build time, which is the aspect again.

By F1's own logic — the tier boundary determines the aspect, and rewriting an
aspect means re-running it over the repo — this must be decided before M4 sets
the index schema. The reference fan-out measurement in the M3 gate exists to size
it.

### F15 resolved — consume SCIP from a scip-java aspect

Decided: **build-time reference tables, produced by an aspect, in SCIP format via
`scip-java`.** Not a bespoke extractor.

The reasoning that settles it: a reference table has to know *which*
`checkNotNull`, which is name resolution, which needs javac's resolved AST.
tree-sitter cannot do it. So the build-time indexer was always going to be a
javac plugin — and that is a thing that already exists, maintained by someone
else against new Java versions.

`scip-java` gained automatic Bazel support in v0.8.24. It is invoked as

```
scip-java index "--bazel-scip-java-binary=$(which scip-java)"
```

and the flag is mandatory because *it runs an aspect that needs the absolute path
to the binary* — i.e. the mechanism this plan chose independently. Indexing
happens inside the Bazel action graph, so it inherits parallel compilation and
the build cache. Java on Bazel is supported; Kotlin on Bazel is not, which is
worth knowing if the repo has any.

The SCIP schema covers more of the nine operations than the header-jar plan did:

| Operation | SCIP mechanism |
| --- | --- |
| `workspaceSymbol` | `Occurrence` with `SymbolRole::Definition` |
| `goToDefinition` | same, plus `Document.relative_path` |
| `findReferences` | non-definition occurrences; `Relationship.is_reference` |
| `goToImplementation` | `Relationship.is_implementation` |
| Positions | `single_line_range` / `multi_line_range`, per occurrence |

Two details that matter to work already done. `Document.position_encoding` is an
explicit field, and JVM indexers emit `UTF16CodeUnitOffsetFromLineStart` — so
`LineIndex` is exactly the bridge needed when a client negotiates UTF-8, and M1's
encoding work is load-bearing rather than incidental. And `SymbolRole` is a
bitset distinguishing `Definition`, `Import`, `ReadAccess`, `WriteAccess`,
`Generated` and `Test`, which is the reference-kind distinction
`EXPECTATIONS.md` asks for — a rename touches all kinds, a call graph wants only
some.

**Open, and the first thing the spike must check:** whether SCIP carries enough
enclosing-scope information to answer call hierarchy. Three of the nine
operations are `prepareCallHierarchy`, `incomingCalls` and `outgoingCalls`, and
SCIP was designed for Sourcegraph's definition/reference/hover surface. If the
containing symbol of an occurrence is not derivable, call hierarchy needs
something else and that changes M4 again.

### F17 — A first build is a prerequisite, and the index should come from CI (rework)

Follows from F14 and closes it. If the index is a build output, then having one
means a build has happened; so require it rather than engineering around its
absence. That collapses M4's three-way source model to two: indexed, or dirty.

Three consequences.

**Scope it, do not build the world.** On a real megarepo `bazel build //...` is
not merely slow — it includes targets that are broken at HEAD, need credentials,
or want toolchains the machine does not have. Nobody runs it. The index scope has
to be configurable (`//myteam/...` and its deps), and designing for that from the
start avoids the assumption that will not survive contact.

**A dedicated `--output_base` becomes required, not a candidate.** F13 was about
queries blocking the agent's builds. Running *builds* in the background holds the
exclusive workspace lock for minutes, directly stalling `bazel test`. The cost of
a separate output base is that jabar's builds miss the agent's local action
cache, so the remote cache has to carry it.

**The index should be produced in CI, not locally.** This is how the comparable
systems work — Kythe at Google, Glean at Meta, SCIP at Sourcegraph: the build
already runs for every commit, the aspect rides along, and the index is uploaded
to a shared store that clients fetch from. That is the PDF's "Remote Cache
Hydration", which this plan filed as an optional Phase 3 optimization. Between
F14 and this, that was wrong: it is what makes the aspect approach viable at
scale, not a later optimization. Local indexing remains the fallback and the
small-repo path. The aspect is the same either way, which is what makes it safe
to build now and decide distribution later.

**What none of this fixes:** the window between an agent writing a file and a
build running. Nothing build-based can close it. That is what the dirty-file
overlay is for, and it gives tree-sitter a clearer job than "positions":

```
committed and built  →  SCIP index          complete, resolved, remotely cached
dirty or new files   →  tree-sitter overlay immediate, shallow, local
```

An unindexed symbol must report `IndexStale`, never `NoMatch`.

### F18 — File watching is missing from the plan entirely (gap)

Raised by the same review. F3's read-after-write barrier covers writes arriving
through `didOpen`/`didChange` or a re-stat. It does not cover branch switches,
codegen, or another tool mutating files — and under F17 those are exactly what
must trigger a background reindex.

Recursive watching of a megarepo is its own hard problem; `notify` does not scale
to it, which is why watchman and fsmonitor exist. This needs a decision, not a
default: client `didChangeWatchedFiles` only, a watchman integration, or an
explicit `jabar/refresh` the agent calls after its own builds. The last is worth
serious consideration precisely because the client here *is* the thing running
the builds.

### F16 — Agents have no stable focus target (rework)

`RootKind::FocusTarget` and `is_editable` in `base-db` assume a human who keeps
one module warm for an hour. An agent's session hops: global search, edit in
target A, definition into B, edit B, edit C. Every one of those writes lands on
files currently classified `MEDIUM`.

Correctness survives — a `MEDIUM` write invalidates properly — but if `MEDIUM`
writes are as frequent as `LOW` ones, the middle tier degenerates into two tiers
plus bookkeeping, and the bookkeeping is not free: re-rooting churns membership
inputs whose durability is deliberately higher.

This is the same error §1 accuses the original plan of: designing for a user who
does not show up.

*Phase 1 action:* write the policy down and test it. **The focus is a set, not a
target, and membership is promote-on-write** — the first client write to a file
in a `MEDIUM` root promotes that root to `LOW` for the session, and demotion
happens only on a graph refresh.

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
- Build-graph access: targets, sources, classpath and the JVM run environment
  (the last for DAP in Phase 2), on-disk graph cache, and an invalidation story.
  Via the bazel CLI rather than BSP; see F11 and the M3 gate.
- Shallow global index: names, kinds, owning target, ranges, supertype edges —
  built from a Bazel aspect and persisted.
- Focus slice plumbing: `inverseSources` → transitive slice → durability-tagged
  salsa inputs.
- Telemetry that detects wrong answers, not just slow ones (see below).

**Pulled forward.** From Phase 2, a skeletal salsa database — `FileText`,
`SourceRoot`, `FileSourceRoot` inputs and one trivial derived query, no parser —
because without it the VFS-to-salsa pipeline is untestable. From Phase 3, index
persistence, for the reasons in F5.

**Not in this phase.** No parser beyond the narrow position-resolution use in
M4 (F12). No item tree, no type inference, no remote HIR cache.

**Cut from the roadmap entirely.** Completion, signature help, inlay hints,
semantic tokens, code lens, folding ranges, document highlight, formatting, code
actions. None are reachable from the client surface. If any appear in a later
phase document, that document has drifted.

**Rename is the one cut now restored, to Phase 3.** The original justification —
not reachable from the client surface — mistook today's schema for the client's
needs. Agents rename constantly; they simply do it badly, by string-replacing
across files. A correct cross-repo rename is plausibly the highest-value single
operation available to hand an agent, it shares most of its machinery with
`findReferences`, and if the client surface grows one operation a
workspace-edit-shaped one is the likely candidate.

### Telemetry: detecting misbehaviour

Latency instrumentation assumes a human who notices a slow response and re-runs
it. An agent re-runs nothing — it takes the first answer as ground truth, so the
failure that costs most is the fast, confident, wrong one.

The sharpest case is the empty result. `workspaceSymbol` returning nothing
because the index has not finished building is, on the wire, identical to
returning nothing because the symbol does not exist. The client cannot tell
those apart, and one of them makes it delete code that is actually referenced.
So an empty outcome must carry *why* it was empty, and exactly one reason —
"searched, found nothing" — is healthy. Everything else is the server saying
"none" when it means "I do not know".

The `telemetry` crate encodes that in its types, so every call site is asked the
question rather than being allowed to return a bare empty list. Alongside it:

- **Truncation** is recorded as returned-versus-total, so withheld results are
  counted rather than silently dropped (F4).
- **Stale reads** are flagged when an answer is produced while the VFS still
  holds undrained writes — the F3 race, made observable.
- **Runtime invariant checks** validate that returned ranges sit on character
  boundaries and inside the file, which is the bug class the fixture's
  `Messages.java` exists to provoke. They record and carry on rather than
  panicking.
- **Concerns** turn the counters into a verdict, ranked so a wrong answer sorts
  above an outright failure — a failure is at least visible to the client.

One boundary to hold: the `Health` summary contains counts, durations and
operation kinds only — no paths, symbol names or file contents — so it is safe
to share. The per-query `tracing` stream is the opposite, and is what makes it
useful for debugging and unsafe to ship anywhere. Nothing leaves the process on
its own.

**Auditing is not mitigation.** The telemetry makes a misleading empty visible to
whoever reads the health summary, but the agent on the wire still receives a bare
`[]` and still deletes the code. So the policy, not just the counter: a query on a
*standard* LSP method that would be `IndexNotReady`, `IndexStale` or
`TargetNotLoaded` must **block until it can answer, or return an LSP error** —
never an empty success. The reasons ride the custom methods, where a client that
understands them can act on them.

Two gaps in the crate as built, both to close before the first handler uses it:

- `InFlight`'s `Drop` hardcodes `stale: false` and there is no `mark_stale()`, so
  the guard-based flow — which every handler will use — can never emit the
  stale-read signal the crate advertises.
- `EmptyReason` has no variant for an index whose *source material* is stale,
  which is how F14's failure escapes classification as unhealthy.

### A known break waiting at scale

`aquery` reports a Javac action's `arguments` as the literal command line, and
`build-model` scans it for `--sources`, `--classpath` and `--output`. On the
fixture that argv is inline. On a real repo it may not be: once the command line
crosses Bazel's params-file threshold — a thousand-jar classpath does so easily —
or Javac runs as a persistent worker, `arguments` collapses to roughly
`["…JavaBuilder", "@bazel-out/…/libfoo.jar-2.params"]` and every flag moves into
the file.

The scan would then return empty vectors and produce a `CompileInfo` with no
sources and no classpath: not an error, a **silently wrong answer** — the exact
failure class the telemetry section condemns. `ParseError::NoJavacAction` does not
catch it, because the Javac action is present and matches.

Attempting to reproduce this on the fixture with `--min_param_file_size=1` left
the argv inline, so it is unconfirmed here; `--include_param_files` exists on
`aquery` precisely because params files are a case it has to handle. The fix is
cheap either way and should not wait for confirmation: read the params file when
an argument begins with `@`, and in the meantime treat "no flags found but an
`@file` argument present" as a hard error so the break is loud.

## 5. Crate layout

Mirrors rust-analyzer's separation, minus what this consumer does not need, plus
the index tier it does. The rule that matters: no crate below `jabar-server`
performs I/O.

```
jabar/
├── crates/
│   ├── paths          absolute, UTF-8 paths — thin wrapper over camino
│   ├── vfs            FileId interner, change log, VfsPath (incl. jar:// virtual)
│   ├── build-model    bazel labels, aquery parsing, CLI queries, durability
│   ├── symbol-index   shallow global tier: names, supertypes, persistence
│   ├── telemetry      outcome/invariant recording, health summary
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
| M1 | LSP shell | Claude Code completes a session over stdio: lifecycle, encoding negotiation, incremental text sync into the VFS, and a `jabar/status` request. No capability is advertised that is not served. **Done.** | 1–2 |
| M2 | VFS and salsa inputs | A test writes a file out of band, immediately queries, and sees the new content. A second test proves a `LOW`-durability edit does not invalidate `HIGH`-durability derived values. | 2–3 |
| M3 | Build graph | Targets, sources, classpath and the JVM run environment resolved and cached to disk; **the graph-access gate below is measured and recorded**; the jar-versus-srcjar decision written down with a working spike behind it. | 3–5 |
| M4 | Global index | `workspaceSymbol` and `findReferences` answer across the whole fixture repo from a cold process in under a second, served from an on-disk SCIP index produced by the `scip-java` aspect (F15), with a `tree-sitter-java` overlay for dirty files (F17). Results match `EXPECTATIONS.md`. Truncation returns a stated total, not a silently clipped list. | 5–7 |
| M5 | Focus slice | A local query loads exactly its target's slice and nothing more; a `BUILD` edit re-slices and invalidates the affected shard, debounced. | 7–8 |
| M6 | Instrumentation | Cold p50 for `workspaceSymbol` and `goToDefinition` measured on a real target, not just the fixture. Reference fan-out distribution recorded to size the truncation budget. | 8 |

### The M3 gate: how the build graph is reached

Everything above is calibrated on twelve targets. One measurement decides whether
the query family carries M4 or an aspect has to be written, and it must be taken
on the real repo before M4 starts:

> **Time `bazel cquery //... --output=starlark` over the whole repo, cold and
> warm.** That is the analysis phase across the full graph — the cost an aspect
> amortizes through the remote cache and the query family pays on every server
> start.

Read it as:

- **Seconds.** Skip the aspect. Enumerate targets with `cquery`, read header
  jars, resolve positions lazily. No repo change, nothing to get signed off.
- **Minutes.** The aspect is buying the remote cache, and F5 makes that
  decisive. Write it — and since you are paying for an aspect anyway, have it
  emit positions too, which is the one thing header jars cannot give you.

Four more measurements belong on the same trip, each settling a finding that
cannot be settled from a 12-target fixture. Record every number in this document
when taken.

| # | Measure | Settles |
| --- | --- | --- |
| 1 | `bazel cquery //... --output=starlark`, cold and warm | aspect vs query family |
| 2 | Query latency **while a build holds the lock**, and `aquery` latency immediately after a differently-flagged build | F13 — whether jabar needs its own `--output_base` |
| 3 | Fraction of targets with a readable header jar locally, with and without `--remote_download_toplevel` | F14 — whether an aspect is needed to *fetch* header jars |
| 4 | Reference fan-out of one `checkNotNull`-class symbol, and the rdeps-closure size of its owning target | F15 — whether Phase 2 needs a code-search index |
| 5 | Size of `aquery deps(//deep:target) --output=jsonproto` for one deep target | per-target vs bulk slice loading |

Measurement 4 is the one to take first if time is short: it sizes the largest
unbudgeted item in the project.

A sixth is a five-minute check rather than a measurement: run
`bazel aquery 'mnemonic("Javac", //some/big:target)' --include_param_files` on a
large target and confirm whether the argv is a params-file reference. See the
note under §5.

**The scip-java spike, which now gates M4's design.** Run `scip-java index` over
`fixtures/megarepo` and check, in this order:

1. Does it run against Bazel 9.2.0 at all? Its Bazel support may target an older
   Bazel, and that is a hard blocker rather than a detail.
2. Does SCIP carry enough enclosing-scope information to answer call hierarchy?
   Three of nine operations depend on it (F15).
3. Do the emitted definitions and references match `EXPECTATIONS.md`? The fixture
   already states the golden answers — 30 `checkNotNull` call sites across 15
   files in 8 targets, three `RetryPolicy` implementations in two targets — so
   this is a conformance check rather than an inspection.
4. What does the index weigh per target? That sizes the distribution problem in
   F17.

Half a day, and it either de-risks the largest item in the project or says to
write our own extractor.

**Schedule risk.** M3 and M4 are where this slips, and they share a root cause:
nothing here has been measured against a repo of your size. If M3 threatens M4,
stub the workspace model from a checked-in JSON dump and build M4 against it —
the index design is worth proving even on fake graph data, and M4 is the
milestone this consumer actually needs.

## 7. Decisions Phase 1 must close

Each gets baked into a type signature early. Deferring means rewriting Phase 2.

| Question | Recommendation |
| --- | --- |
| How do external dependencies enter the database — jars, srcjars, or both? | Classfile reader as the primary path, srcjars opportunistically when BSP offers them. Bytecode already carries the names, signatures and supertypes the shallow tier needs. |
| Write a Java parser, or use an existing one? | `tree-sitter-java`. Error-tolerant and incremental off the shelf — 19.3 MB/s, 501µs to reparse a keystroke, `Send + Sync` — and it recovers declarations from half-written files, which is the state an agent's file is usually in. rust-analyzer's reasons for writing its own do not transfer. Arrives at M4 for positions only. |
| Aspects or the query family for reaching the build graph? | Query family now. Skyframe is not externally reachable, so those are the only two doors, and F10 means the shallow tier needs no aspect. Revisit only if the M3 gate shows `cquery //...` costing minutes, or once positions must be precomputed. |
| BSP, or the bazel CLI? | The CLI. bazel-bsp is a JVM tool needing coursier and version-matching, and `query`/`aquery` answer the same questions of the same graph in 67–170ms. Keep aquery parsing separate from the CLI runner so BSP can be added later for `buildTarget/didChange` push invalidation. |
| Build the debug adapter, or drive an existing one? | Drive `java-debug` as a subprocess. Supply the launch config and jar-to-source mapping — the two things it cannot derive under Bazel — and let it own JDI, stepping and thread control. |
| Does the shallow index live in salsa, or beside it? | Beside it, as a plain persisted structure keyed by target. It changes only when the build graph changes, so putting it under salsa buys invalidation you do not need and costs a serialization story you do. |
| Does `VfsPath` carry jar-internal entries, or do jars get extracted to a cache directory? | Virtual paths. Extraction adds an I/O and invalidation problem the design is explicitly trying to avoid. |
| What is the unit of durability? | Three tiers. Focus target `LOW`, same-repo transitive deps `MEDIUM`, external jars `HIGH`. The middle tier is the one rust-analyzer lacks and a megarepo needs. |
| How does the client learn that a result was truncated? | A custom method returning an explicit total and a ranking rationale, with standard `textDocument/references` kept as a conformant fallback for Copilot. Silent truncation makes an agent confidently wrong. |
| Own a Bazel daemon connection, or shell out per query? | Shell out to the CLI, which talks to the resident Bazel server — there is no per-query JVM warmup to avoid. The real cost is contention with the agent's own builds (F13), answered by a dedicated `--output_base` rather than by a different protocol. |
| How does the index behave when a target's header jar is missing or stale? | Three-way source model per target: present → read it; absent → `TargetNotLoaded`, never `NoMatch`; source newer than jar → overlay from parsed source (F14). |
| Where does call-site data for `findReferences` come from? | **Decided: build-time reference tables from an aspect, in SCIP format via `scip-java`.** A reference table needs name resolution, so the extractor was always a javac plugin; that already exists. Spike it against the fixture before committing (F15). |
| Is a first build a prerequisite? | **Yes, scoped — not `//...`.** An index is a build output, so require the build rather than engineer around its absence. Produced in CI and fetched where possible; locally as a fallback (F17). |
| How does jabar learn a file changed outside the client? | **Undecided.** `didChangeWatchedFiles`, watchman, or an explicit `jabar/refresh` the agent calls after its own builds. The last is attractive because the client is the thing running the builds (F18). |
| What is the unit of focus? | A set, not a target, with promote-on-write membership (F16). |
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
- [ ] Every client-facing handler reports an outcome, and a health summary over
      a fixture session shows zero misleading empties.
- [ ] The classpath decision is written down with the spike that justifies it
      committed.
- [ ] All five M3 gate measurements are taken on the real repo and their numbers
      recorded in this document.
- [ ] The `scip-java` spike has run against the fixture, and call hierarchy is
      either answerable from SCIP or has a written alternative (F15).
- [ ] The index scope is configurable, and `//...` is nowhere assumed (F17).
- [ ] jabar builds in its own `--output_base`, verified not to block a concurrent
      `bazel test` (F13, F17).
- [ ] The file-change trigger is decided and implemented (F18).
- [ ] An `@params-file` javac action is either handled or refused loudly — never
      parsed into an empty `CompileInfo`.
- [ ] No standard-method query returns an empty success when the true answer is
      "not indexed yet".
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
