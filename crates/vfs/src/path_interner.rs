//! Maps [`VfsPath`]s to compact integer ids.
//!
//! Ids are never reused and paths are never removed. A megarepo session touches
//! a large set of paths but not an unbounded one, and stable ids mean a
//! [`FileId`] handed out early stays meaningful after the build graph moves
//! underneath it.

use indexmap::IndexSet;
use rustc_hash::FxBuildHasher;

use crate::{FileId, VfsPath};

#[derive(Default)]
pub(crate) struct PathInterner {
    map: IndexSet<VfsPath, FxBuildHasher>,
}

impl PathInterner {
    /// The id for `path`, or `None` if it was never interned.
    pub(crate) fn get(&self, path: &VfsPath) -> Option<FileId> {
        self.map.get_index_of(path).map(|i| FileId::from_raw(i as u32))
    }

    /// The id for `path`, allocating one if this is the first time it is seen.
    pub(crate) fn intern(&mut self, path: VfsPath) -> FileId {
        let (id, _inserted) = self.map.insert_full(path);
        assert!(id < FileId::MAX as usize, "ran out of file ids");
        FileId::from_raw(id as u32)
    }

    /// # Panics
    ///
    /// Panics if `id` was not produced by this interner.
    pub(crate) fn lookup(&self, id: FileId) -> &VfsPath {
        self.map.get_index(id.index() as usize).expect("file id is not from this interner")
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}
