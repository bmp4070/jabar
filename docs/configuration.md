# Configuration

Everything arrives as LSP `initializationOptions`. Every setting has a default
that works, so a client that sends nothing still gets a working server.

```jsonc
{
  // Where bazel keeps its state. Omit to share the workspace default.
  "outputBase": "/tmp/jabar-base",

  // The bazel executable. `bazelisk` also works.
  "bazel": "bazel",

  "index": {
    // Run the aspect at startup when no index is found. Off by default.
    "auto": true,
    // What to index. Scope it -- see below.
    "targets": ["//java/..."],
    // Path to scip-java. Omit to look it up on PATH.
    "scipJava": "/usr/local/bin/scip-java"
  }
}
```

## `outputBase` — share or separate

**Omit it** and jabar uses the same output base as your own `bazel` commands:
same server, same action cache, no duplicate analysis. Right when nothing else
is building.

**Set it** and jabar gets its own. Bazel takes an exclusive lock per output
base, so sharing means jabar's builds queue behind yours *and block them*. A
separate base removes that.

The cost is real: a second analysis universe and a second set of outputs.
Gerrit's is several GB, and the first build against it is a full cold analysis.

For a configured base, Jabar asks that exact Bazel invocation for its
`bazel-bin` directory and pins the physical path. It does not follow the
workspace `bazel-bin` symlink, because another Bazel invocation may have
repointed that symlink at a different output base. Discovery, cache validation,
and watching all use the pinned directory.

## `index.auto` — off by default

Indexing runs a Bazel build. That can take minutes, and nobody asked for it by
opening an editor. With `auto` off, jabar looks for an existing index and serves
nothing if there is none. It advertises its implemented operations, but their
queries fail with `IndexNotReady` until an index is available. Startup discovery
and an enabled automatic build run after the `initialize` response; check
`jabar/status` for `indexLoading`, `startupState`, and `startupError`.

Turn it on when you would rather wait once than build by hand.

## `index.targets` — scope large workspaces

The default is `//...` so a small repository works without configuration.
Large workspaces should set an explicit scope: the full pattern can include
targets broken at HEAD, targets needing credentials, and targets whose
toolchains are not installed locally. For example:

```json
{ "index": { "targets": ["//java/...", "//lib/..."] } }
```

Measured on Gerrit: `//java/...` is 97 targets, 2m04s, 50,646 definitions.

## Where the aspect goes

jabar writes it to `<workspace>/.jabar/aspects/` on demand and rewrites it
whenever it differs, so upgrading jabar upgrades the aspect. Add `.jabar/` to
`.gitignore`.

## Checking what took effect

`jabar/status` reports the resolved configuration alongside index state — the
output base in use, the target patterns, whether an index is loaded. In VS Code
that is **jabar: Show server status**.
