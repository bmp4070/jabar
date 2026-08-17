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

One wrinkle, handled but worth knowing: switching output base leaves
`bazel-bin` pointing into the previous one until a command rewrites it, so the
first aspect run after a switch fails and is retried automatically. You will see
`the first run repointed bazel-bin…` in the log once.

## `index.auto` — off by default

Indexing runs a Bazel build. That can take minutes, and nobody asked for it by
opening an editor. With `auto` off, jabar looks for an existing index and serves
nothing if there is none — which is honest, since it advertises no capability it
cannot serve.

Turn it on when you would rather wait once than build by hand.

## `index.targets` — not `//...`

Defaults to `//...` because that is the only sensible default for a small repo,
and it is the wrong value for a large one. A real megarepo's `//...` includes
targets broken at HEAD, targets needing credentials, and targets whose
toolchains are not installed. Scope it to what you work in:

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
