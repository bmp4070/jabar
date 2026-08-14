//! Build Server Protocol client.
//!
//! Not yet implemented. BSP is JSON-RPC 2.0 over the same `Content-Length`
//! framing as LSP, so the transport is `lsp-server` rather than a second
//! JSON-RPC stack.
//!
//! The methods that matter for jabar:
//!
//! * `build/initialize` — handshake with bazel-bsp.
//! * `buildTarget/inverseSources` — file to owning target. The cheap query.
//! * `buildTarget/sources` — a target's sources.
//! * `buildTarget/javacOptions` — the classpath, and thus the jars.
//! * `buildTarget/didChange` — invalidation when the graph moves.
