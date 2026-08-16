# jabar for Claude Code

Claude Code discovers LSP servers through plugins, so this is a small local
marketplace containing one.

## Setup

jabar must be findable. Either put it on PATH:

```sh
cargo build --release
ln -sf "$PWD/target/release/jabar" /usr/local/bin/jabar   # or anywhere on PATH
```

…or edit `command` in `.claude-plugin/marketplace.json` to the absolute path.

Then, in Claude Code:

```
/plugin marketplace add /Users/medha/github/jabar/editors/claude-code
/plugin install jabar-lsp@jabar-local
```

Restart Claude Code. Verify with `/plugin` — `jabar-lsp` should be listed and
enabled.

**Disable `jdtls-lsp` if it is on.** The official catalog ships it, it also
claims `.java`, and two servers for one extension is not a defined situation.

## Using it

Open a Bazel Java repo with a SCIP index present — jabar looks for `bazel-bin`
and `.jabar/index`, and advertises nothing without one. Then ask Claude to use
the LSP tool, for example:

- "find the definition of ProjectCache"
- "who calls getAllProjects"
- "what does ProjectCacheImpl.getAllProjects call"
- "list the symbols in ProjectCache.java"

All nine operations the tool exposes are served: `workspaceSymbol`,
`goToDefinition`, `findReferences`, `hover`, `documentSymbol`,
`goToImplementation`, `prepareCallHierarchy`, `incomingCalls`, `outgoingCalls`.

## If nothing works

The most likely cause is no index, which is indistinguishable from a broken
server unless you look. jabar logs to stderr at startup:

```
found an index at startup dir=… shards=97 definitions=50646
```

or

```
no index found; run the SCIP aspect, then reopen or call `jabar/loadIndex`
```

The second means the aspect has not run for that workspace. Build one per
`crates/build-model/aspects/README.md`.
