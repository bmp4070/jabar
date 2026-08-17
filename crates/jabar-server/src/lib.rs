//! The language server: protocol shell, state, and request dispatch.
//!
//! Not yet implemented beyond the entry point. This crate will own the
//! `GlobalState`/snapshot split and the event loop, following rust-analyzer's
//! structure rather than an async runtime — salsa cancellation wants one
//! synchronous writer and many snapshot readers.

pub mod capabilities;
pub mod config;
pub mod documents;
pub mod handlers;
pub mod line_index;
pub mod server;
pub mod uri;

pub use crate::server::{Server, run_server};

use std::io::IsTerminal as _;

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

    // Colour only when a human is watching a terminal. An editor collects
    // stderr into a panel that renders none of it, so the escapes arrive as
    // literal `ESC[2m` noise around every field -- which is most of the line.
    let ansi = std::io::stderr().is_terminal();

    // `try_init` rather than `init`: tests and embedders may have set a
    // subscriber already, and failing to log is not worth aborting over.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .with_target(true)
        // Seconds are enough to correlate with an editor action; nanoseconds
        // just made the line longer.
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new("%H:%M:%S%.3f".to_owned()))
        .try_init();
}
