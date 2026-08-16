# jabar-lsp

Registers jabar as the Java language server for Claude Code's LSP tool.

Claude Code's LSP surface is exactly the nine operations jabar implements, which
is not a coincidence — the surface is what the design was built around. See
`docs/phase-1.md` §2.

## Requirements

- `jabar` on PATH, or edit `command` in the marketplace entry to an absolute
  path such as `<repo>/target/release/jabar`.
- A SCIP index in the workspace. jabar looks for `bazel-bin` and `.jabar/index`
  at startup and serves nothing without one. See
  `crates/build-model/aspects/README.md`.

## Conflicts

The official catalog ships `jdtls-lsp`, which also claims `.java`. Enable one.
