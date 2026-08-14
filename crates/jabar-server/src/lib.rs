//! The language server: protocol shell, state, and request dispatch.
//!
//! Not yet implemented beyond the entry point. This crate will own the
//! `GlobalState`/snapshot split and the event loop, following rust-analyzer's
//! structure rather than an async runtime — salsa cancellation wants one
//! synchronous writer and many snapshot readers.

pub mod capabilities;
pub mod documents;
pub mod line_index;
pub mod server;
pub mod uri;

pub use crate::server::{Server, run_server};

use tracing_subscriber::EnvFilter;

/// Sends diagnostics to stderr, since stdout carries the LSP wire protocol and
/// a stray log line there corrupts the session.
///
/// Verbosity comes from `JABAR_LOG` (falling back to `RUST_LOG`), e.g.
/// `JABAR_LOG=jabar_server=debug,telemetry=debug`. The `telemetry` target
/// carries the per-query stream; see that crate's docs on what it may contain.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_env("JABAR_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // `try_init` rather than `init`: tests and embedders may have set a
    // subscriber already, and failing to log is not worth aborting over.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .try_init();
}
