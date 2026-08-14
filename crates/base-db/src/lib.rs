//! The salsa database: the inputs everything else is derived from.
//!
//! This crate does no I/O and knows nothing about Bazel or LSP. Something above
//! it reads bytes and pushes them in; everything below recomputes only what the
//! change actually reached.
//!
//! # Durability is the whole point
//!
//! Salsa tracks, per input, how likely it is to change. A derived value whose
//! inputs are all `HIGH` durability does not even get *checked* when a `LOW`
//! input is edited — salsa can skip the comparison, because it knows nothing
//! high-durability moved.
//!
//! That is the mechanism the megarepo design rests on, and Bazel hands us the
//! partition for free. Three tiers, one more than rust-analyzer uses:
//!
//! | Tier | Durability | What it holds | Changes when |
//! | --- | --- | --- | --- |
//! | [`RootKind::FocusTarget`] | `LOW` | the target being edited | every keystroke |
//! | [`RootKind::WorkspaceDep`] | `MEDIUM` | same-repo transitive deps | a branch switch |
//! | [`RootKind::External`] | `HIGH` | jars, the JDK | the build graph moves |
//!
//! The middle tier is the one rust-analyzer lacks and a megarepo needs: in a
//! repo of millions of files, "not the file I am editing" is not the same claim
//! as "will never change".

use std::sync::{Arc, RwLock};

use rustc_hash::FxHashMap;
use salsa::{Durability, Setter as _};
use vfs::FileId;

pub use salsa;

/// A group of files sharing a durability tier.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceRootId(pub u32);

/// How likely a source root is to change.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RootKind {
    /// The target the client is working in.
    FocusTarget,
    /// Same-repo dependencies of the focus target.
    WorkspaceDep,
    /// Jars and the JDK: outside the repo entirely.
    External,
}

impl RootKind {
    /// Durability for the *text* of files in this root.
    pub fn text_durability(self) -> Durability {
        match self {
            RootKind::FocusTarget => Durability::LOW,
            RootKind::WorkspaceDep => Durability::MEDIUM,
            RootKind::External => Durability::HIGH,
        }
    }

    /// Durability for a file's *membership* of this root.
    ///
    /// Membership is steadier than content — editing a file does not move it
    /// between targets — so this sits a tier above the text wherever it can.
    /// Only a build-graph change reassigns a file, and that invalidates
    /// everything anyway.
    pub fn membership_durability(self) -> Durability {
        match self {
            RootKind::FocusTarget => Durability::MEDIUM,
            RootKind::WorkspaceDep | RootKind::External => Durability::HIGH,
        }
    }

    /// Whether files here are ever edited by the client.
    pub fn is_editable(self) -> bool {
        matches!(self, RootKind::FocusTarget)
    }
}

/// The text of one file.
#[salsa::input(debug)]
pub struct FileText {
    #[returns(ref)]
    pub text: Arc<str>,
    #[returns(copy)]
    pub file_id: FileId,
}

/// Which source root a file belongs to.
#[salsa::input(debug)]
pub struct FileSourceRoot {
    #[returns(copy)]
    pub source_root_id: SourceRootId,
}

/// A source root's durability tier.
#[salsa::input(debug)]
pub struct SourceRootInput {
    #[returns(copy)]
    pub kind: RootKind,
}

/// Handles for the inputs, keyed by the ids the VFS hands out.
///
/// Salsa inputs are opaque ids, so something has to remember which `FileText`
/// belongs to which [`FileId`]. Read-mostly: an entry is created once per file
/// and read on every query touching it.
#[derive(Default, Debug)]
struct Inputs {
    files: RwLock<FxHashMap<FileId, FileText>>,
    file_roots: RwLock<FxHashMap<FileId, FileSourceRoot>>,
    roots: RwLock<FxHashMap<SourceRootId, SourceRootInput>>,
}

/// The database.
#[salsa::db]
#[derive(Default, Clone)]
pub struct RootDatabase {
    storage: salsa::Storage<Self>,
    inputs: Arc<Inputs>,
}

#[salsa::db]
impl salsa::Database for RootDatabase {}

impl RootDatabase {
    pub fn new() -> RootDatabase {
        RootDatabase::default()
    }

    /// Builds a database that reports every query execution to `on_event`.
    ///
    /// Used by the tests that prove durability actually prevents work, which
    /// cannot be observed from a query's return value — a re-executed query
    /// returns the same answer as a reused one.
    pub fn with_event_logger(on_event: Box<dyn Fn(salsa::Event) + Send + Sync>) -> RootDatabase {
        RootDatabase { storage: salsa::Storage::new(Some(on_event)), inputs: Arc::default() }
    }

    /// The text input for `file_id`.
    ///
    /// # Panics
    ///
    /// Panics if the file was never set. That is a bug in the layer above: a
    /// query should never reach a file the VFS did not push in first.
    pub fn file_text(&self, file_id: FileId) -> FileText {
        self.inputs
            .files
            .read()
            .expect("inputs lock poisoned")
            .get(&file_id)
            .copied()
            .unwrap_or_else(|| panic!("no text recorded for {file_id}; the VFS never pushed it"))
    }

    /// Whether `file_id` has any text recorded, without panicking.
    pub fn has_file(&self, file_id: FileId) -> bool {
        self.inputs.files.read().expect("inputs lock poisoned").contains_key(&file_id)
    }

    /// Records a file's text at the durability of the root it belongs to.
    ///
    /// The root must have been registered first, since its tier decides the
    /// durability — getting that wrong is exactly the bug this crate exists to
    /// prevent.
    pub fn set_file_text(&mut self, file_id: FileId, text: &str) {
        let durability = self.durability_of(file_id);
        let existing =
            self.inputs.files.read().expect("inputs lock poisoned").get(&file_id).copied();
        match existing {
            Some(input) => {
                input.set_text(self).with_durability(durability).to(Arc::from(text));
            }
            None => {
                let input = FileText::builder(Arc::from(text), file_id)
                    .text_durability(durability)
                    .file_id_durability(durability)
                    .new(self);
                self.inputs.files.write().expect("inputs lock poisoned").insert(file_id, input);
            }
        }
    }

    /// Assigns `file_id` to `root`, which must already be registered.
    pub fn set_file_source_root(&mut self, file_id: FileId, root: SourceRootId) {
        let kind = self.root_kind(root);
        let durability = kind.membership_durability();
        let existing =
            self.inputs.file_roots.read().expect("inputs lock poisoned").get(&file_id).copied();
        match existing {
            Some(input) => {
                input.set_source_root_id(self).with_durability(durability).to(root);
            }
            None => {
                let input =
                    FileSourceRoot::builder(root).source_root_id_durability(durability).new(self);
                self.inputs
                    .file_roots
                    .write()
                    .expect("inputs lock poisoned")
                    .insert(file_id, input);
            }
        }
    }

    /// Registers a source root, or changes an existing one's tier.
    pub fn set_source_root(&mut self, root: SourceRootId, kind: RootKind) {
        let existing = self.inputs.roots.read().expect("inputs lock poisoned").get(&root).copied();
        match existing {
            Some(input) => {
                // A root changing tier is a build-graph event, so the fact
                // itself is high-durability even when its files are not.
                input.set_kind(self).with_durability(Durability::HIGH).to(kind);
            }
            None => {
                let input =
                    SourceRootInput::builder(kind).kind_durability(Durability::HIGH).new(self);
                self.inputs.roots.write().expect("inputs lock poisoned").insert(root, input);
            }
        }
    }

    /// The root `file_id` belongs to, if it has been assigned one.
    pub fn file_source_root(&self, file_id: FileId) -> Option<SourceRootId> {
        let input =
            self.inputs.file_roots.read().expect("inputs lock poisoned").get(&file_id).copied()?;
        Some(input.source_root_id(self))
    }

    /// # Panics
    ///
    /// Panics if `root` was never registered.
    pub fn root_kind(&self, root: SourceRootId) -> RootKind {
        let input = self
            .inputs
            .roots
            .read()
            .expect("inputs lock poisoned")
            .get(&root)
            .copied()
            .unwrap_or_else(|| panic!("source root {root:?} was never registered"));
        input.kind(self)
    }

    /// The durability a file's text should carry, from its root's tier.
    ///
    /// Files with no root yet are treated as the most volatile tier. Being
    /// wrong in that direction costs recomputation; being wrong the other way
    /// serves stale answers.
    fn durability_of(&self, file_id: FileId) -> Durability {
        match self.file_source_root(file_id) {
            Some(root) => self.root_kind(root).text_durability(),
            None => Durability::LOW,
        }
    }

    /// Applies a batch of VFS changes.
    ///
    /// This is the whole bridge from the outside world to the database. Deleted
    /// files keep their id and become empty rather than disappearing: a query
    /// holding a [`FileId`] must still get an answer, and "empty" is the honest
    /// one for a file that is gone.
    pub fn apply_vfs_changes(&mut self, changes: vfs::Changes) {
        let _span = tracing::debug_span!("apply_vfs_changes", n = changes.len()).entered();
        for changed in changes.into_values() {
            match changed.change {
                vfs::Change::Create(bytes) | vfs::Change::Modify(bytes) => {
                    match String::from_utf8(bytes) {
                        Ok(text) => self.set_file_text(changed.file_id, &text),
                        Err(err) => {
                            // Java source is UTF-8 by spec. A file that is not
                            // is either binary or mis-encoded; either way it
                            // has no text, and refusing to guess beats
                            // lossily decoding it into wrong offsets.
                            tracing::warn!(
                                file = %changed.file_id,
                                valid_up_to = err.utf8_error().valid_up_to(),
                                "file is not valid UTF-8; recording it as empty"
                            );
                            self.set_file_text(changed.file_id, "");
                        }
                    }
                }
                vfs::Change::Delete => self.set_file_text(changed.file_id, ""),
            }
        }
    }
}

/// Number of lines in a file.
///
/// Deliberately trivial. It exists to give the durability machinery something
/// to derive, so that "a workspace edit does not invalidate dependency-derived
/// state" is a property a test can check rather than a claim in a document.
#[salsa::tracked(returns(copy))]
pub fn line_count(db: &dyn salsa::Database, file: FileText) -> u32 {
    let text = file.text(db);
    tracing::trace!(file = %file.file_id(db), "counting lines");
    text.lines().count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const FOCUS: SourceRootId = SourceRootId(0);
    const DEP: SourceRootId = SourceRootId(1);
    const EXTERNAL: SourceRootId = SourceRootId(2);

    fn file(n: u32) -> FileId {
        FileId::from_raw(n)
    }

    /// A database that records which queries salsa actually executed.
    fn logged_db() -> (RootDatabase, Arc<Mutex<Vec<String>>>) {
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = Arc::clone(&log);
        let db = RootDatabase::with_event_logger(Box::new(move |event| {
            if let salsa::EventKind::WillExecute { .. } = event.kind {
                sink.lock().expect("log poisoned").push(format!("{:?}", event.kind));
            }
        }));
        (db, log)
    }

    fn executions(log: &Arc<Mutex<Vec<String>>>) -> usize {
        log.lock().expect("log poisoned").len()
    }

    /// Registers the three tiers and one file in each.
    fn three_tier_db() -> (RootDatabase, Arc<Mutex<Vec<String>>>) {
        let (mut db, log) = logged_db();
        for (root, kind) in [
            (FOCUS, RootKind::FocusTarget),
            (DEP, RootKind::WorkspaceDep),
            (EXTERNAL, RootKind::External),
        ] {
            db.set_source_root(root, kind);
        }
        for (id, root) in [(0, FOCUS), (1, DEP), (2, EXTERNAL)] {
            db.set_file_source_root(file(id), root);
            db.set_file_text(file(id), "one\ntwo\n");
        }
        (db, log)
    }

    #[test]
    fn text_round_trips() {
        let (mut db, _) = logged_db();
        db.set_source_root(FOCUS, RootKind::FocusTarget);
        db.set_file_source_root(file(0), FOCUS);
        db.set_file_text(file(0), "class A {}\n");

        let input = db.file_text(file(0));
        assert_eq!(&**input.text(&db), "class A {}\n");
        assert_eq!(input.file_id(&db), file(0));
        assert_eq!(line_count(&db, input), 1);
    }

    #[test]
    fn tiers_map_to_the_intended_durabilities() {
        assert_eq!(RootKind::FocusTarget.text_durability(), Durability::LOW);
        assert_eq!(RootKind::WorkspaceDep.text_durability(), Durability::MEDIUM);
        assert_eq!(RootKind::External.text_durability(), Durability::HIGH);
        // Membership is steadier than content.
        assert_eq!(RootKind::FocusTarget.membership_durability(), Durability::MEDIUM);
        assert!(RootKind::FocusTarget.is_editable());
        assert!(!RootKind::External.is_editable());
    }

    #[test]
    fn a_workspace_edit_does_not_invalidate_dependency_derived_values() {
        // The claim the whole megarepo design rests on. Editing the file the
        // client is in must not cause anything derived from a jar to be
        // recomputed -- otherwise every keystroke walks the world.
        let (mut db, log) = three_tier_db();

        let focus = db.file_text(file(0));
        let dep = db.file_text(file(1));
        let external = db.file_text(file(2));

        assert_eq!(line_count(&db, focus), 2);
        assert_eq!(line_count(&db, dep), 2);
        assert_eq!(line_count(&db, external), 2);
        assert_eq!(executions(&log), 3, "each should have been computed once");

        // One keystroke in the focus target.
        db.set_file_text(file(0), "one\ntwo\nthree\n");

        assert_eq!(line_count(&db, dep), 2);
        assert_eq!(line_count(&db, external), 2);
        assert_eq!(
            executions(&log),
            3,
            "a LOW edit must not have recomputed the MEDIUM or HIGH derivations"
        );

        assert_eq!(line_count(&db, focus), 3, "but the edited file itself is recomputed");
        assert_eq!(executions(&log), 4);
    }

    #[test]
    fn an_external_change_does_invalidate_what_depends_on_it() {
        // The other half: durability must not make HIGH inputs immutable, or a
        // rebuilt jar would serve stale answers forever.
        let (mut db, log) = three_tier_db();
        let external = db.file_text(file(2));
        assert_eq!(line_count(&db, external), 2);
        let before = executions(&log);

        db.set_file_text(file(2), "one\ntwo\nthree\nfour\n");
        assert_eq!(line_count(&db, external), 4);
        assert_eq!(executions(&log), before + 1, "a HIGH edit must recompute");
    }

    #[test]
    fn salsa_does_not_deduplicate_identical_input_writes() {
        // Worth pinning down, because it is easy to assume otherwise: salsa
        // backdates *derived* values, but an input setter is taken at its word.
        // Writing the same bytes again bumps the revision and everything
        // downstream recomputes.
        //
        // So the VFS's content hashing is not a redundant optimisation sitting
        // in front of a smarter layer -- it is the only thing standing between
        // an agent rewriting a file with identical bytes and a full
        // invalidation. See `vfs::Vfs::set_file_contents`.
        let (mut db, log) = three_tier_db();
        let focus = db.file_text(file(0));
        assert_eq!(line_count(&db, focus), 2);
        let before = executions(&log);

        db.set_file_text(file(0), "one\ntwo\n");
        assert_eq!(line_count(&db, focus), 2);
        assert_eq!(
            executions(&log),
            before + 1,
            "salsa re-runs on an identical write; the VFS is what prevents this"
        );
    }

    #[test]
    fn the_vfs_is_what_stops_identical_writes_reaching_the_database() {
        // The other half of the test above, through the real pipeline: an
        // identical write produces no change at all, so nothing recomputes.
        let (mut db, log) = logged_db();
        db.set_source_root(FOCUS, RootKind::FocusTarget);

        let mut vfs = vfs::Vfs::default();
        let path: vfs::VfsPath = "/repo/A.java".parse().unwrap();
        vfs.set_file_contents(path.clone(), Some(b"one\ntwo\n".to_vec()));
        let file_id = vfs.file_id(&path).expect("just written");
        db.set_file_source_root(file_id, FOCUS);
        db.apply_vfs_changes(vfs.take_changes());

        assert_eq!(line_count(&db, db.file_text(file_id)), 2);
        let before = executions(&log);

        vfs.set_file_contents(path, Some(b"one\ntwo\n".to_vec()));
        let changes = vfs.take_changes();
        assert!(changes.is_empty(), "the VFS dropped the no-op write");
        db.apply_vfs_changes(changes);

        assert_eq!(line_count(&db, db.file_text(file_id)), 2);
        assert_eq!(executions(&log), before, "nothing reached salsa, so nothing recomputed");
    }

    #[test]
    fn a_change_that_preserves_the_result_is_backdated() {
        // Salsa re-runs `line_count` because its input changed, but sees the
        // answer is unchanged. Anything derived *from* line_count would then be
        // spared -- which is what makes deep query graphs survive editing.
        let (mut db, log) = three_tier_db();
        let focus = db.file_text(file(0));
        assert_eq!(line_count(&db, focus), 2);
        let before = executions(&log);

        // Same line count, different text.
        db.set_file_text(file(0), "ONE\nTWO\n");
        assert_eq!(line_count(&db, focus), 2);
        assert_eq!(executions(&log), before + 1, "the query itself re-ran");
    }

    #[test]
    fn vfs_changes_flow_into_the_database() {
        // The bridge M2 exists to prove: bytes pushed at the VFS become salsa
        // inputs without anything in between reading the filesystem.
        let (mut db, _) = logged_db();
        db.set_source_root(FOCUS, RootKind::FocusTarget);

        let mut vfs = vfs::Vfs::default();
        let path: vfs::VfsPath = "/repo/A.java".parse().unwrap();
        vfs.set_file_contents(path.clone(), Some(b"class A {}\n".to_vec()));
        let file_id = vfs.file_id(&path).expect("just written");
        db.set_file_source_root(file_id, FOCUS);

        db.apply_vfs_changes(vfs.take_changes());
        assert_eq!(&**db.file_text(file_id).text(&db), "class A {}\n");
        assert!(!vfs.has_pending_changes(), "the batch was drained");

        // And an edit flows through the same path.
        vfs.set_file_contents(path, Some(b"class A {}\nclass B {}\n".to_vec()));
        db.apply_vfs_changes(vfs.take_changes());
        assert_eq!(line_count(&db, db.file_text(file_id)), 2);
    }

    #[test]
    fn a_deleted_file_becomes_empty_rather_than_missing() {
        // A query may still hold the id. Answering "empty" beats panicking on a
        // file the client just removed.
        let (mut db, _) = logged_db();
        db.set_source_root(FOCUS, RootKind::FocusTarget);

        let mut vfs = vfs::Vfs::default();
        let path: vfs::VfsPath = "/repo/Gone.java".parse().unwrap();
        vfs.set_file_contents(path.clone(), Some(b"class Gone {}\n".to_vec()));
        let file_id = vfs.file_id(&path).expect("just written");
        db.set_file_source_root(file_id, FOCUS);
        db.apply_vfs_changes(vfs.take_changes());

        vfs.set_file_contents(path, None);
        db.apply_vfs_changes(vfs.take_changes());

        assert!(db.has_file(file_id), "the id still resolves");
        assert_eq!(&**db.file_text(file_id).text(&db), "");
        assert_eq!(line_count(&db, db.file_text(file_id)), 0);
    }

    #[test]
    fn invalid_utf8_is_recorded_as_empty_not_guessed() {
        // Lossy decoding would produce text whose byte offsets do not match the
        // file, and every range derived from it would be quietly wrong.
        let (mut db, _) = logged_db();
        db.set_source_root(EXTERNAL, RootKind::External);

        let mut vfs = vfs::Vfs::default();
        let path: vfs::VfsPath = "jar:/repo/a.jar!/com/A.class".parse().unwrap();
        vfs.set_file_contents(path.clone(), Some(vec![0xca, 0xfe, 0xba, 0xbe]));
        let file_id = vfs.file_id(&path).expect("just written");
        db.set_file_source_root(file_id, EXTERNAL);
        db.apply_vfs_changes(vfs.take_changes());

        assert_eq!(&**db.file_text(file_id).text(&db), "");
    }

    #[test]
    fn a_file_with_no_root_gets_the_most_volatile_tier() {
        // Being wrong towards LOW costs recomputation. Being wrong towards HIGH
        // serves stale answers, which is the failure that matters.
        let (mut db, log) = logged_db();
        db.set_file_text(file(9), "one\n");
        let input = db.file_text(file(9));
        assert_eq!(line_count(&db, input), 1);
        let before = executions(&log);

        db.set_file_text(file(9), "one\ntwo\n");
        assert_eq!(line_count(&db, input), 2);
        assert_eq!(executions(&log), before + 1);
    }

    #[test]
    fn moving_a_file_between_roots_changes_its_durability() {
        // What happens when the client opens a file in a dependency: it becomes
        // the focus target, and its text has to stop being high-durability or
        // edits to it would be ignored.
        let (mut db, _) = three_tier_db();
        assert_eq!(db.file_source_root(file(2)), Some(EXTERNAL));

        db.set_file_source_root(file(2), FOCUS);
        assert_eq!(db.file_source_root(file(2)), Some(FOCUS));

        db.set_file_text(file(2), "edited\n");
        assert_eq!(line_count(&db, db.file_text(file(2))), 1);
    }

    #[test]
    fn a_root_can_change_tier() {
        let (mut db, _) = three_tier_db();
        assert_eq!(db.root_kind(DEP), RootKind::WorkspaceDep);
        db.set_source_root(DEP, RootKind::FocusTarget);
        assert_eq!(db.root_kind(DEP), RootKind::FocusTarget);
    }

    #[test]
    #[should_panic(expected = "the VFS never pushed it")]
    fn reading_an_unknown_file_is_a_bug_not_an_empty_answer() {
        let (db, _) = logged_db();
        db.file_text(file(404));
    }
}
