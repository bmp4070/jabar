# jabar for VS Code

A thin shim. VS Code cannot talk to an arbitrary LSP binary on its own, so this
spawns `jabar` and points it at Java files. Everything a user sees comes from
the server; logic added here is logic no other editor gets.

## Running it

```sh
cargo build --release                 # from the repo root
cd editors/vscode && npm install && npm run compile
code --extensionDevelopmentPath="$PWD"   # opens a window with the extension loaded
```

VS Code runs `out/extension.js`, not the TypeScript. **Any change to `src/`
needs `npm run compile` before it takes effect**, and a reload of the extension
host (`Developer: Reload Window`) after that. Symptom of forgetting: behaviour,
including error messages, that matches an older version of the source. `npm run
watch` recompiles on save and avoids the whole problem.

Then open a Bazel Java workspace.

The binary is found automatically: `target/release/jabar`, then `target/debug`,
looked for beside the extension and under the open workspace, falling back to
`jabar` on PATH. Set `jabar.server.path` only to override that.

If it cannot start, the error names what it tried — `spawn jabar ENOENT` on its
own means the search found nothing and PATH had no `jabar`, which almost always
means the release build has not run.

## The index has to exist first

jabar reads SCIP shards; it does not yet produce them. On startup it looks for
`bazel-bin` and `.jabar/index` under the workspace root, and advertises its
query capabilities only if it finds an index — so with no shards, VS Code shows
no jabar features rather than showing broken ones.

Produce them with the aspect in `crates/build-model/aspects/`:

```sh
export JAVA_HOME=$(/usr/libexec/java_home)
bazel build //...                     # must precede the aspect; see that README
bazel build //java/... \
  --aspects //jabar_aspects:scip_java.bzl%scip_java_aspect \
  --output_groups=scip --keep_going \
  --define=sourceroot=$PWD --define=java_home=$JAVA_HOME \
  "--define=scip_java_binary=$(which scip-java)"
```

The server watches for shards changing and reloads on its own. Reopen the
window after the first build, since capabilities are decided at `initialize`.

## Commands

- **jabar: Show server status** — whether an index is loaded, how many
  definitions, whether the watcher is running, and any health concerns. Worth
  running first when something looks wrong: a server with no index behaves very
  differently from a broken one.
- **jabar: Reload the symbol index** — re-reads shards from a directory. Rarely
  needed, since the server watches for rebuilds; it is for an index produced
  somewhere else, or when the watcher could not start.

## If you also have Red Hat's Java extension

Both provide definitions and hovers, and VS Code merges results, so you will see
duplicates. Disable one for a clean comparison.
