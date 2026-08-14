//! The language server: protocol shell, state, and request dispatch.
//!
//! Not yet implemented beyond the binary entry point. This crate will own the
//! `GlobalState`/snapshot split and the event loop, following rust-analyzer's
//! structure rather than an async runtime — salsa cancellation wants one
//! synchronous writer and many snapshot readers.
