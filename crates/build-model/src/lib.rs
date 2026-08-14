//! The build graph: what jabar knows about targets, sources, and classpaths.
//!
//! Bazel is the only thing that knows which files exist and how they connect,
//! so every question about workspace structure comes through here. See
//! [`aquery`] for why this talks to the `bazel` CLI rather than to a Build
//! Server Protocol server.

pub mod aquery;
mod bazel;
mod label;

pub use crate::aquery::{CompileInfo, ParseError, parse_javac_actions};
pub use crate::bazel::{BazelCli, BazelError};
pub use crate::label::{LabelError, TargetLabel};
