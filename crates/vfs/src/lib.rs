//! Virtual file system: the single source of truth for file contents.
//!
//! Not yet implemented. This crate will own:
//!
//! * `FileId` allocation and path interning, so the rest of the server passes
//!   around a `u32` rather than a string.
//! * A `VfsPath` that covers both real filesystem paths and jar-internal
//!   entries, since most of a Java classpath arrives as classfiles inside an
//!   archive rather than as files on disk.
//! * Change batching with content hashing, so a write that does not alter the
//!   bytes never reaches salsa and never invalidates anything.
//!
//! See `fixtures/megarepo/EXPECTATIONS.md` for the cases this has to handle.
