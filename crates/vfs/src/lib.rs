//! Virtual file system: the single source of truth for file contents.
//!
//! The VFS does no I/O of its own. Something above it — a file watcher, a BSP
//! query, an editor notification — reads bytes and pushes them in via
//! [`Vfs::set_file_contents`]. The VFS records only what *changed*, and hands
//! that batch to the database on demand via [`Vfs::take_changes`].
//!
//! Two properties earn their keep:
//!
//! **Writes that change nothing are dropped.** Contents are hashed on the way
//! in, and a write whose hash matches the current state returns `false` and
//! records nothing. Agents rewrite files constantly, often with identical
//! bytes; without this, every such write would invalidate the world.
//!
//! **Changes within a batch are merged.** A file created and then deleted
//! before the next [`Vfs::take_changes`] produces no change at all, rather than
//! a delete for something the database never saw.
//!
//! Contents are owned `Vec<u8>`, not memory maps. See `docs/phase-1.md` (F6)
//! for why: open buffers never come from disk, a mapped region raises `SIGBUS`
//! when the file is truncated underneath it, and the bytes have to be validated
//! and normalized into an `Arc<str>` before they can become a salsa input
//! anyway.

mod path_interner;
mod vfs_path;

use std::fmt;
use std::hash::Hasher;
use std::mem;

use indexmap::IndexMap;
use rustc_hash::{FxBuildHasher, FxHasher};

use crate::path_interner::PathInterner;

pub use crate::vfs_path::{JarPath, JrtPath, PathParseError, VfsPath};
pub use paths::{AbsPath, AbsPathBuf};

/// A handle to a path known to the [`Vfs`].
///
/// Ids are dense and stable for the life of the server, which is what lets the
/// rest of the system pass a `u32` around instead of a string.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(u32);

impl FileId {
    /// Top bit left clear so the id has a spare niche.
    pub const MAX: u32 = 0x7fff_ffff;

    #[inline]
    pub const fn from_raw(raw: u32) -> FileId {
        assert!(raw <= FileId::MAX);
        FileId(raw)
    }

    #[inline]
    pub const fn index(self) -> u32 {
        self.0
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileId({})", self.0)
    }
}

/// What the VFS currently believes about a path.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum FileState {
    /// Present, with the given content hash.
    Exists(u64),
    /// Absent — either never seen, or deleted.
    Deleted,
}

/// A change to one file, relative to the last [`Vfs::take_changes`].
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    /// Was absent, is now present.
    Create(Vec<u8>),
    /// Was present, content differs.
    Modify(Vec<u8>),
    /// Was present, is now absent.
    Delete,
}

impl Change {
    pub fn contents(&self) -> Option<&[u8]> {
        match self {
            Change::Create(v) | Change::Modify(v) => Some(v),
            Change::Delete => None,
        }
    }

    pub fn kind(&self) -> ChangeKind {
        match self {
            Change::Create(_) => ChangeKind::Create,
            Change::Modify(_) => ChangeKind::Modify,
            Change::Delete => ChangeKind::Delete,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Create,
    Modify,
    Delete,
}

/// One entry in a batch of changes.
#[derive(Debug)]
pub struct ChangedFile {
    pub file_id: FileId,
    pub change: Change,
}

impl ChangedFile {
    /// True unless the file was deleted.
    pub fn exists(&self) -> bool {
        !matches!(self.change, Change::Delete)
    }

    /// True when the file appeared or disappeared, which is what makes the
    /// *set* of files change rather than just their contents.
    pub fn is_structural(&self) -> bool {
        matches!(self.change, Change::Create(_) | Change::Delete)
    }
}

/// A batch of changes, in the order the files were first touched.
pub type Changes = IndexMap<FileId, ChangedFile, FxBuildHasher>;

/// Path interning plus a log of pending changes.
#[derive(Default)]
pub struct Vfs {
    interner: PathInterner,
    /// Indexed by [`FileId::index`]; parallel to the interner.
    state: Vec<FileState>,
    changes: Changes,
}

impl Vfs {
    /// The id for `path` if it is currently present. Deleted and never-seen
    /// paths both give `None`.
    pub fn file_id(&self, path: &VfsPath) -> Option<FileId> {
        let file_id = self.interner.get(path)?;
        match self.state_of(file_id) {
            FileState::Exists(_) => Some(file_id),
            FileState::Deleted => None,
        }
    }

    /// # Panics
    ///
    /// Panics if `file_id` did not come from this VFS.
    pub fn file_path(&self, file_id: FileId) -> &VfsPath {
        self.interner.lookup(file_id)
    }

    /// Whether `file_id` currently refers to a present file.
    pub fn exists(&self, file_id: FileId) -> bool {
        matches!(self.state_of(file_id), FileState::Exists(_))
    }

    /// Every present file, in id order.
    pub fn iter(&self) -> impl Iterator<Item = (FileId, &VfsPath)> + '_ {
        (0..self.state.len() as u32)
            .map(FileId::from_raw)
            .filter(move |&id| self.exists(id))
            .map(move |id| (id, self.interner.lookup(id)))
    }

    /// Number of paths ever interned, present or not.
    pub fn len(&self) -> usize {
        self.interner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Records the contents of `path`; `None` means the file is gone.
    ///
    /// Returns whether anything actually changed. A write whose bytes hash to
    /// the current state is dropped, as is a delete of something already
    /// absent.
    pub fn set_file_contents(&mut self, path: VfsPath, contents: Option<Vec<u8>>) -> bool {
        // Hashed once, up front, and carried through: a cold load hashes every
        // file in the repo, so doing it twice per write is not free.
        let hashed = contents.map(|bytes| {
            let hash = hash_contents(&bytes);
            (bytes, hash)
        });
        let file_id = self.alloc_file_id(path);

        let (change, new_state) = match (self.state_of(file_id), hashed) {
            (FileState::Deleted, None) => return false,
            (FileState::Deleted, Some((bytes, hash))) => {
                (Change::Create(bytes), FileState::Exists(hash))
            }
            (FileState::Exists(_), None) => (Change::Delete, FileState::Deleted),
            (FileState::Exists(old_hash), Some((bytes, hash))) => {
                if hash == old_hash {
                    return false;
                }
                (Change::Modify(bytes), FileState::Exists(hash))
            }
        };

        self.state[file_id.0 as usize] = new_state;
        self.record(file_id, change);
        true
    }

    /// Whether [`Vfs::take_changes`] would return anything.
    ///
    /// The read-after-write barrier uses this: a query that must observe a
    /// just-written file has to drain the VFS into the database first.
    pub fn has_pending_changes(&self) -> bool {
        !self.changes.is_empty()
    }

    /// Drains the pending batch.
    pub fn take_changes(&mut self) -> Changes {
        mem::take(&mut self.changes)
    }

    /// Folds a new change into whatever is already pending for this file.
    ///
    /// Both the pending change and the new one are expressed relative to
    /// different baselines — the pending one relative to the last drain, the
    /// new one relative to current state — so they cannot simply be appended.
    fn record(&mut self, file_id: FileId, change: Change) {
        use Change::*;

        let Some(pending) = self.changes.get_mut(&file_id) else {
            self.changes.insert(file_id, ChangedFile { file_id, change });
            return;
        };

        match (&mut pending.change, change) {
            // Created and then deleted before the database ever saw it. Emitting
            // a Delete here would name a file the consumer never learned about.
            //
            // `shift_remove` rather than `swap_remove` to keep the batch in
            // first-touch order. It is O(n) in the batch, but a file that is
            // created and deleted between two drains is rare.
            (Create(_), Delete) => {
                self.changes.shift_remove(&file_id);
            }
            // Still a creation, just with later content.
            (Create(prev), Modify(new) | Create(new)) => *prev = new,
            (Modify(prev), Modify(new)) => *prev = new,
            (change @ Modify(_), Delete) => *change = Delete,
            // Deleted and then recreated: from the last drain's point of view
            // the file was there before and is there now, so it is a Modify.
            (change @ Delete, Create(new) | Modify(new)) => *change = Modify(new),
            // `set_file_contents` computes the new change against current state,
            // which already reflects the pending one, so a second Delete or a
            // Create-over-existing cannot arise. Assert in tests, but degrade
            // rather than panic in a server: losing the session over a
            // bookkeeping slip is worse than a redundant delete.
            (change @ Delete, Delete) => {
                debug_assert!(false, "double delete for {file_id}");
                *change = Delete;
            }
            (change @ Modify(_), Create(new)) => {
                debug_assert!(false, "create over pending modify for {file_id}");
                *change = Modify(new);
            }
        }
    }

    /// Interns `path`, starting it out as absent.
    fn alloc_file_id(&mut self, path: VfsPath) -> FileId {
        let file_id = self.interner.intern(path);
        let needed = file_id.0 as usize + 1;
        if self.state.len() < needed {
            self.state.resize(needed, FileState::Deleted);
        }
        file_id
    }

    fn state_of(&self, file_id: FileId) -> FileState {
        self.state.get(file_id.0 as usize).copied().unwrap_or(FileState::Deleted)
    }
}

impl fmt::Debug for Vfs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vfs")
            .field("paths", &self.interner.len())
            .field("pending_changes", &self.changes.len())
            .finish()
    }
}

/// Content hash used only to answer "did these bytes change?".
///
/// FxHash is weak as hashes go, but the comparison is always old-versus-new for
/// a single file, never across files, so the birthday bound does not apply — it
/// is one 64-bit comparison per write. The length is folded in explicitly
/// because FxHash avalanches poorly on appended zero bytes, and "file grew by
/// some NUL padding" is otherwise exactly the shape it could miss.
fn hash_contents(bytes: &[u8]) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write_usize(bytes.len());
    hasher.write(bytes);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> VfsPath {
        s.parse().unwrap_or_else(|e| panic!("`{s}` should parse: {e}"))
    }

    fn b(s: &str) -> Option<Vec<u8>> {
        Some(s.as_bytes().to_vec())
    }

    /// Drains and returns the single pending change, asserting there is exactly one.
    fn take_one(vfs: &mut Vfs) -> Change {
        let mut changes = vfs.take_changes();
        assert_eq!(changes.len(), 1, "expected exactly one change, got {changes:?}");
        changes.shift_remove_index(0).expect("just checked len").1.change
    }

    #[test]
    fn create_then_read_back() {
        let mut vfs = Vfs::default();
        let p = path("/repo/A.java");
        assert!(vfs.set_file_contents(p.clone(), b("class A {}")));

        let id = vfs.file_id(&p).expect("file should be present");
        assert!(vfs.exists(id));
        assert_eq!(vfs.file_path(id), &p);
        assert_eq!(take_one(&mut vfs), Change::Create(b"class A {}".to_vec()));
    }

    #[test]
    fn absent_paths_have_no_id() {
        let mut vfs = Vfs::default();
        assert_eq!(vfs.file_id(&path("/repo/Nope.java")), None);
        // Deleting something never seen is not a change.
        assert!(!vfs.set_file_contents(path("/repo/Nope.java"), None));
        assert!(!vfs.has_pending_changes());
    }

    #[test]
    fn identical_rewrite_is_dropped() {
        // The case agents hit constantly: rewriting a file with the same bytes.
        let mut vfs = Vfs::default();
        let p = path("/repo/A.java");
        vfs.set_file_contents(p.clone(), b("same"));
        vfs.take_changes();

        assert!(!vfs.set_file_contents(p.clone(), b("same")), "no-op write should be dropped");
        assert!(!vfs.has_pending_changes());

        assert!(vfs.set_file_contents(p, b("different")));
        assert_eq!(take_one(&mut vfs), Change::Modify(b"different".to_vec()));
    }

    #[test]
    fn create_then_delete_in_one_batch_vanishes() {
        // The database never saw this file, so it must not be told to delete it.
        let mut vfs = Vfs::default();
        let p = path("/repo/Scratch.java");
        vfs.set_file_contents(p.clone(), b("temp"));
        vfs.set_file_contents(p.clone(), None);

        assert!(!vfs.has_pending_changes(), "create+delete should cancel out");
        assert_eq!(vfs.file_id(&p), None);
    }

    #[test]
    fn create_then_modify_stays_a_create() {
        let mut vfs = Vfs::default();
        let p = path("/repo/A.java");
        vfs.set_file_contents(p.clone(), b("one"));
        vfs.set_file_contents(p, b("two"));
        assert_eq!(take_one(&mut vfs), Change::Create(b"two".to_vec()));
    }

    #[test]
    fn delete_then_recreate_is_a_modify() {
        // Relative to the last drain the file was there and still is, so the
        // consumer should see a content change, not a delete plus a create.
        let mut vfs = Vfs::default();
        let p = path("/repo/A.java");
        vfs.set_file_contents(p.clone(), b("before"));
        vfs.take_changes();

        vfs.set_file_contents(p.clone(), None);
        vfs.set_file_contents(p, b("after"));
        assert_eq!(take_one(&mut vfs), Change::Modify(b"after".to_vec()));
    }

    #[test]
    fn modify_then_delete_is_a_delete() {
        let mut vfs = Vfs::default();
        let p = path("/repo/A.java");
        vfs.set_file_contents(p.clone(), b("before"));
        vfs.take_changes();

        vfs.set_file_contents(p.clone(), b("after"));
        vfs.set_file_contents(p, None);
        assert_eq!(take_one(&mut vfs), Change::Delete);
    }

    #[test]
    fn ids_survive_deletion() {
        // A FileId handed out earlier must keep meaning the same path even
        // after the file goes away and comes back.
        let mut vfs = Vfs::default();
        let p = path("/repo/A.java");
        vfs.set_file_contents(p.clone(), b("one"));
        let first = vfs.file_id(&p).unwrap();

        vfs.set_file_contents(p.clone(), None);
        assert_eq!(vfs.file_id(&p), None, "deleted files have no id");
        assert!(!vfs.exists(first));
        assert_eq!(vfs.file_path(first), &p, "but the id still names the path");

        vfs.set_file_contents(p.clone(), b("two"));
        assert_eq!(vfs.file_id(&p), Some(first), "and is reused on recreation");
    }

    #[test]
    fn take_changes_resets_the_batch() {
        let mut vfs = Vfs::default();
        vfs.set_file_contents(path("/repo/A.java"), b("a"));
        assert!(vfs.has_pending_changes());
        assert_eq!(vfs.take_changes().len(), 1);
        assert!(!vfs.has_pending_changes());
        assert!(vfs.take_changes().is_empty(), "draining twice yields nothing");
    }

    #[test]
    fn archive_entries_are_ordinary_files_here() {
        // A classfile inside a jar gets an id and a change like anything else;
        // only the reader above the VFS cares that it lives in an archive.
        let mut vfs = Vfs::default();
        let cls = path("jar:/repo/libpolicy.jar!/com/acme/policy/P.class");
        let jrt = path("jrt:/java.base/java/lang/String.class");

        assert!(vfs.set_file_contents(cls.clone(), Some(vec![0xca, 0xfe, 0xba, 0xbe])));
        assert!(vfs.set_file_contents(jrt.clone(), Some(vec![0xca, 0xfe, 0xba, 0xbe])));

        assert!(vfs.file_id(&cls).is_some());
        assert!(vfs.file_id(&jrt).is_some());
        assert_ne!(vfs.file_id(&cls), vfs.file_id(&jrt), "identical bytes, distinct paths");
        assert_eq!(vfs.take_changes().len(), 2);
    }

    #[test]
    fn iter_skips_deleted_and_is_id_ordered() {
        let mut vfs = Vfs::default();
        for name in ["/repo/A.java", "/repo/B.java", "/repo/C.java"] {
            vfs.set_file_contents(path(name), b("x"));
        }
        vfs.set_file_contents(path("/repo/B.java"), None);

        let present: Vec<_> = vfs.iter().map(|(_, p)| p.to_string()).collect();
        assert_eq!(present, ["/repo/A.java", "/repo/C.java"]);
        assert_eq!(vfs.len(), 3, "deleted paths stay interned");
    }

    #[test]
    fn changes_keep_first_touch_order() {
        let mut vfs = Vfs::default();
        vfs.set_file_contents(path("/repo/B.java"), b("b"));
        vfs.set_file_contents(path("/repo/A.java"), b("a"));
        // Touching B again must not move it behind A.
        vfs.set_file_contents(path("/repo/B.java"), b("b2"));

        let changes = vfs.take_changes();
        let order: Vec<_> =
            changes.values().map(|c| vfs.file_path(c.file_id).to_string()).collect();
        assert_eq!(order, ["/repo/B.java", "/repo/A.java"]);
    }

    #[test]
    fn empty_and_large_contents() {
        let mut vfs = Vfs::default();
        let p = path("/repo/Empty.java");
        assert!(vfs.set_file_contents(p.clone(), Some(Vec::new())));
        assert!(vfs.exists(vfs.file_id(&p).unwrap()), "an empty file still exists");
        assert!(!vfs.set_file_contents(p.clone(), Some(Vec::new())));

        // Length is folded into the hash, so appended NULs are never missed.
        assert!(vfs.set_file_contents(p, Some(vec![0u8; 4096])));
    }

    #[test]
    fn structural_changes_are_flagged() {
        let mut vfs = Vfs::default();
        let p = path("/repo/A.java");
        vfs.set_file_contents(p.clone(), b("one"));
        let changes = vfs.take_changes();
        assert!(changes.values().all(|c| c.is_structural() && c.exists()));

        vfs.set_file_contents(p.clone(), b("two"));
        let changes = vfs.take_changes();
        assert!(changes.values().all(|c| !c.is_structural() && c.exists()));

        vfs.set_file_contents(p, None);
        let changes = vfs.take_changes();
        assert!(changes.values().all(|c| c.is_structural() && !c.exists()));
    }
}
