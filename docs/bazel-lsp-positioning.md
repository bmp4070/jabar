# Jabar in the Bazel language-tooling ecosystem

## Scope

Jabar is a focused Java navigation LSP for Bazel monorepos. It serves
compiler-produced SCIP indexes locally, exposes readiness and reference
truncation to agent clients, and combines a global loaded-shard index with a
declaration overlay for edited files.

This page describes current strengths and limits. It does not claim that Jabar
is faster, smaller, or more accurate than another language server: no controlled
comparative benchmark has established those conclusions.

## Where Jabar is useful

### Bazel-produced Java semantics

Jabar runs or consumes the
[upstream scip-java Bazel aspect](https://github.com/scip-code/scip-java/blob/main/docs/getting-started.md)
and loads its SCIP shards. The indexing action uses compiler information and
includes generated sources visible to the configured build. Bazel can
parallelize those actions and reuse its action cache. Jabar supplies the local
LSP serving layer; the Java semantic extraction belongs to scip-java.

With no explicit `outputBase`, Jabar canonicalizes the workspace `bazel-bin`
path. With an explicit base, it queries that configured Bazel invocation for its
output path. It pins the resulting physical path and does not currently use BSP.

### Navigation across loaded shards

The symbol index aggregates every discovered shard into one repository-wide
view. Navigation does not depend on which projects an editor has imported. This
is useful when an IDE or language-server integration deliberately narrows a
large workspace. For example, rules_rust documents that per-package workspace
splitting can omit callers in dependent packages.

The boundary is the loaded shard set and the metadata each operation supports.
Jabar does not yet report complete target coverage, and failed or skipped Bazel
targets may be absent. Location conversion can also discard unusable entries.
Call hierarchy requires indexed source definitions and enclosing callable
metadata, so it excludes some external or binary-only callees and is an
approximation of Java dispatch. An empty response therefore does not prove that
every possible target or runtime relationship was examined.

### A small local service for agent navigation

Jabar implements nine operations used for repository investigation:

- workspace symbol search;
- definition, references, hover, and implementation;
- document symbols;
- call hierarchy prepare, incoming calls, and outgoing calls.

It runs as a local LSP process and does not require a Sourcegraph deployment or
an IDE platform. Completion, diagnostics, rename, code actions, formatting,
signature help, semantic tokens, and a live Java type engine remain outside the
current scope.

### Explicit incomplete and stale states

`jabar/status` reports whether an index is loaded, its verification state, and
cache/build configuration. `jabar/references` reports both the returned and
loaded-index totals so an agent can detect the 5,000-result cap. The standard
LSP references response remains a bare capped array. `workspace/symbol` caps
responses at 50 and call hierarchy caps them at 100; those standard responses do
not expose a total.

For open files, a tree-sitter overlay contributes declarations newer than the
SCIP index. Queries that require indexed positions reject known-stale documents
instead of silently applying old coordinates to new text. The overlay does not
recreate full Java type analysis, and source freshness for every unopened file
is not yet proven.

### Measured large-workspace feasibility

The first representative run loaded 6,876 shards with 3.13 million definitions,
39.4 million references, and 42.6 million occurrences. Cache format v2 measured
a 3.74 GiB snapshot, about 22 seconds for a warm load, 24.6 GiB warm peak RSS,
about 7.3 GiB steady RSS, and a 152 ms `workspace/symbol` p95 for one query.

These measurements are a feasibility baseline from a limited sample on one
host. Refresh workloads, complete target coverage, full response parity, and a
broad query corpus remain open. See
[monolith measurement results](monolith-measurement-results.md).

## Comparison by tool category

These systems solve overlapping parts of the problem, so comparisons must keep
their scopes separate.

| Tool or category | What it provides | Jabar's relevant distinction | Jabar's limitation |
| --- | --- | --- | --- |
| [Eclipse JDT LS](https://github.com/eclipse-jdtls/eclipse.jdt.ls) | A full Java semantic LSP with completion, diagnostics, refactoring, and documented Maven/Gradle project import. | Jabar provides a direct Bazel aspect and SCIP path without translating the workspace into JDT projects. | JDT LS has substantially deeper live editing support. Bazel import extensions have existed, so Jabar must not claim that JDT LS cannot work with Bazel. |
| [IntelliJ Bazel support](https://www.jetbrains.com/help/idea/bazel.html) and [BSP](https://build-server-protocol.github.io/docs/specification.html) | Rich target import, build, run, test, debug, Starlark, and IDE Java semantics. | Jabar is a standalone, editor-neutral LSP process suited to lightweight agent harnesses. | Jabar does not provide the IDE project model, build UI, debugger, or complete editing experience. BSP is a build protocol, not a Java semantic engine. |
| [Sourcegraph precise navigation](https://sourcegraph.com/docs/code-navigation/precise-code-navigation) with SCIP | Multi-language, offline-produced precise indexes served through a broader code-navigation platform. | Jabar serves Java SCIP locally through LSP and adds session status and edited-file handling without requiring a Sourcegraph service. | Jabar uses the same indexing ecosystem and cannot claim better compiler accuracy or broader cross-repository scale. |
| [Kythe](https://kythe.io/) | Compiler-derived, language-neutral code graphs, including Java and Bazel extraction. | Jabar has a smaller directly usable local-LSP surface. | Compiler-derived graphs and Bazel-scale offline navigation are established ideas, not unique Jabar features. |
| [rust-analyzer with rules_rust](https://bazelbuild.github.io/rules_rust/rust_analyzer.html) | A full Rust semantic server with Bazel-generated project descriptions, toolchain setup, and workspace splitting. | Jabar applies a compiler-index approach to Java and keeps a global view of its loaded shards. | rust-analyzer has a deeper incremental semantic model. It should not be described as Cargo-only or unable to work with Bazel. |
| [clangd indexes](https://clangd.llvm.org/design/indexing) with a Bazel compilation database | A full C/C++ semantic LSP with dynamic, background, static, and remote indexes. | Jabar packages the corresponding Java+Bazel navigation workflow around SCIP. | clangd already demonstrates layered live and offline indexes; those architecture ideas are not unique to Jabar. |
| [VS Code Bazel](https://github.com/bazel-contrib/vscode-bazel) and Starlark language servers such as [starpls](https://github.com/withered-magic/starpls) | BUILD/`.bzl` completion, navigation, formatting, linting, labels, and build/test integration. | Jabar resolves Java symbols and calls across loaded Java shards. | These tools are complementary and should be used for Bazel/Starlark source intelligence. |

The archived `JetBrains/bazel-bsp` repository moved into
[Hirschgarten](https://github.com/JetBrains/hirschgarten). Its archive state is
not evidence that BSP-based Bazel integration was abandoned.

## Claims supported today

- Jabar is a focused Java navigation LSP for Bazel monorepos.
- It serves compiler-produced SCIP indexes locally through standard LSP.
- It aggregates navigation across all loaded shards, independent of editor
  project selection.
- Its custom methods expose readiness, verification, and reference truncation.
- It has been exercised on an index containing tens of millions of occurrences,
  with the limitations recorded in the measurement report.

## Claims that require more evidence

- Complete repository or target coverage.
- All possible callers or references.
- Full live Java semantics for edited files.
- Better correctness, startup time, query latency, or memory use than another
  mature server.
- Production latency or memory objectives based on the current limited sample.
