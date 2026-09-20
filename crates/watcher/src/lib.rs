//! Noticing when the world changed underneath the index.
//!
//! # Why this watches almost nothing
//!
//! The obvious design watches the source tree. On a megarepo that is the
//! expensive, fragile thing: Linux needs one inotify watch per directory and
//! `max_user_watches` runs out, which is why Watchman exists.
//!
//! jabar can mostly avoid it, because three different things change and only
//! one of them needs the filesystem at all:
//!
//! | What changed | How jabar hears about it | Cost |
//! | --- | --- | --- |
//! | The agent edited a file | `didOpen` / `didChange` | free, already handled |
//! | The index was rebuilt | the `.scip` shards themselves | ~one file per target |
//! | The workspace moved | `.git/HEAD` and `.git/index` | two files |
//!
//! The middle row is the important inversion. A SCIP index is a *build output*,
//! so the question is not "did a source file change" but "did the aspect
//! re-run" — and the shards state that directly. Ray's entire Java tree is
//! seven shards. Watching a million source files to infer something seven files
//! already say is the wrong end of the problem.
//!
//! The last row is deliberately coarse. A branch switch or a checkout writes
//! both `.git/HEAD` and `.git/index`, so those two files catch it without
//! walking anything. It reports *that* a lot changed, never *what* — which is
//! the right granularity for "the index is stale, re-run the aspect".
//!
//! What this misses: a source file changed by something that is neither the
//! client nor git — a code generator run outside Bazel, a `sed` across the
//! tree. Those surface at the next build or the next explicit refresh. Watching
//! the whole tree to catch them would cost more than they are worth, and an
//! explicit refresh request from the client is a better answer, since the
//! client is the thing running the builds.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use paths::AbsPath;

/// How long to wait for a burst to settle.
///
/// A build writes many shards in quick succession and a checkout rewrites much
/// of the tree; reloading on the first event would mean reloading against a
/// half-written state. Long enough to coalesce a build, short enough that a
/// human does not notice.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Something happened that the index may need to react to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// SCIP shards were written. The index should be reloaded.
    Index,
    /// The workspace moved — a branch switch, a checkout, a rebase.
    ///
    /// Coarse by design: it means everything may be stale, not that any
    /// particular file changed.
    Workspace,
    /// The watcher may have dropped events, so existing state must be checked.
    /// This is uncertainty, not evidence that the workspace actually moved.
    WatcherError,
}

/// Watches the narrow set of paths that matter, and reports coalesced changes.
///
/// The debouncer is owned here; dropping the [`FileWatcher`] stops watching.
pub struct FileWatcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    receiver: Receiver<Change>,
}

impl FileWatcher {
    /// Starts watching.
    ///
    /// `index_dir` holds the `.scip` shards; `workspace_root` is the repo, whose
    /// `.git` directory is watched for branch movement. Either may be absent —
    /// a workspace with no git directory is perfectly normal — and a path that
    /// cannot be watched is logged and skipped rather than failing the server.
    pub fn spawn(
        index_dir: Option<&AbsPath>,
        workspace_root: Option<&AbsPath>,
    ) -> notify::Result<FileWatcher> {
        let (tx, receiver) = unbounded();
        let sink = tx.clone();
        let git_dir = workspace_root.and_then(resolve_git_dir);
        let dispatch_git_dir = git_dir.clone();

        let mut debouncer =
            new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| match result {
                Ok(events) => {
                    let paths = events.iter().flat_map(|event| event.paths.iter());
                    dispatch(paths, &sink, dispatch_git_dir.as_deref());
                }
                Err(errors) => {
                    // A dropped event means we may miss a change. Worth saying
                    // loudly, because the symptom is a silently stale index.
                    for error in errors {
                        tracing::warn!(%error, "file watch error; the index may go stale");
                    }
                    // The backend cannot tell us which event was lost. Ask the
                    // server to validate provenance and shards while retaining
                    // its last known-good index.
                    let _ = sink.send(Change::WatcherError);
                }
            })?;

        // Shards live one per target under bazel-bin, so this is recursive but
        // over a small tree of build outputs, not over the source.
        if let Some(dir) = index_dir {
            watch(&mut debouncer, dir.as_str(), RecursiveMode::Recursive, "index shards");
        }

        // Non-recursive: `.git` holds thousands of loose objects that change on
        // every fetch and tell us nothing. Only HEAD and index matter, and both
        // sit at the top level.
        if let Some(git) = &git_dir {
            watch(
                &mut debouncer,
                git.to_string_lossy().as_ref(),
                RecursiveMode::NonRecursive,
                "git state",
            );
        }

        Ok(FileWatcher { _debouncer: debouncer, receiver })
    }

    /// Changes, coalesced. Receiving blocks until one arrives.
    pub fn receiver(&self) -> &Receiver<Change> {
        &self.receiver
    }
}

/// Resolves both an ordinary `.git` directory and a linked-worktree `.git`
/// file containing `gitdir: <path>`.
fn resolve_git_dir(root: &AbsPath) -> Option<PathBuf> {
    let dot_git = PathBuf::from(root.as_str()).join(".git");
    if dot_git.is_dir() {
        return Some(std::fs::canonicalize(&dot_git).unwrap_or(dot_git));
    }
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let raw = contents.trim().strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(raw);
    let resolved = if path.is_absolute() { path } else { PathBuf::from(root.as_str()).join(path) };
    Some(std::fs::canonicalize(&resolved).unwrap_or(resolved))
}

fn watch(
    debouncer: &mut Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    path: &str,
    mode: RecursiveMode,
    what: &str,
) {
    match debouncer.watch(Path::new(path), mode) {
        Ok(()) => tracing::debug!(path, what, "watching"),
        // A missing directory is the normal case before the first build, or in
        // a workspace that is not a git checkout. Neither is fatal.
        Err(error) => tracing::info!(path, what, %error, "not watching"),
    }
}

/// Classifies changed paths and sends at most one message of each kind.
///
/// Deduplicating here rather than at the receiver keeps a thousand-file
/// checkout from becoming a thousand reload requests.
fn dispatch<'a>(
    paths: impl Iterator<Item = &'a std::path::PathBuf>,
    sink: &Sender<Change>,
    git_dir: Option<&Path>,
) {
    let mut index = false;
    let mut workspace = false;

    for path in paths {
        match classify(path, git_dir) {
            Some(Change::Index) => index = true,
            Some(Change::Workspace) => workspace = true,
            Some(Change::WatcherError) => unreachable!("paths never represent watcher errors"),
            None => {}
        }
    }

    // Workspace first: it is the broader signal, so a receiver that acts on it
    // has already invalidated whatever the index message would have.
    if workspace {
        let _ = sink.send(Change::Workspace);
    }
    if index {
        let _ = sink.send(Change::Index);
    }
}

/// What a changed path means, if anything.
fn classify(path: &Path, git_dir: Option<&Path>) -> Option<Change> {
    if path.extension().is_some_and(|ext| ext == "scip") {
        // Per-source shards inside a targetroot are intermediate outputs. The
        // sibling target shard triggers the reload after aggregation finishes.
        let intermediate = path.ancestors().skip(1).any(|ancestor| {
            ancestor.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
                name.ends_with(".scip-targetroot") || name.ends_with(".semanticdb")
            })
        });
        return (!intermediate).then_some(Change::Index);
    }
    // `HEAD` moves on checkout and branch switch; `index` moves on any staging
    // operation, which is the cheapest proxy for "the tree was rewritten".
    // Ignore `HEAD.lock` and friends, which appear mid-operation.
    let name = path.file_name()?.to_str()?;
    let directly_in_git_dir = git_dir.is_some_and(|dir| path.parent() == Some(dir));
    let directly_in_dot_git = path.parent()?.file_name().is_some_and(|name| name == ".git");
    if matches!(name, "HEAD" | "index") && (directly_in_git_dir || directly_in_dot_git) {
        return Some(Change::Workspace);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn scip_shards_mean_the_index_changed() {
        assert_eq!(
            classify(Path::new("/repo/bazel-bin/java/com/acme/core/core.scip"), None),
            Some(Change::Index)
        );
    }

    #[test]
    fn intermediate_scip_shards_do_not_trigger_reload() {
        assert!(
            changes_from(&[
                "/repo/bazel-bin/a/a.scip-targetroot/A.java.scip",
                "/repo/bazel-bin/b/b.semanticdb/B.java.scip",
            ])
            .is_empty()
        );
    }

    #[test]
    fn git_head_and_index_mean_the_workspace_moved() {
        assert_eq!(classify(Path::new("/repo/.git/HEAD"), None), Some(Change::Workspace));
        assert_eq!(classify(Path::new("/repo/.git/index"), None), Some(Change::Workspace));
    }

    #[test]
    fn transient_git_files_are_ignored() {
        // These appear and vanish during any git operation. Treating them as
        // changes would fire a reload several times per checkout.
        assert_eq!(classify(Path::new("/repo/.git/HEAD.lock"), None), None);
        assert_eq!(classify(Path::new("/repo/.git/index.lock"), None), None);
        assert_eq!(classify(Path::new("/repo/.git/objects/ab/cdef"), None), None);
        assert_eq!(classify(Path::new("/repo/.git/refs/heads/main"), None), None);
    }

    #[test]
    fn a_file_named_index_outside_git_is_not_a_workspace_change() {
        // `src/index` or `docs/HEAD` are ordinary files.
        assert_eq!(classify(Path::new("/repo/src/index"), None), None);
        assert_eq!(classify(Path::new("/repo/docs/HEAD"), None), None);
    }

    #[test]
    fn ordinary_source_files_are_ignored() {
        // The whole point: editing source does not go through the watcher. The
        // client already told us, and the index only moves when a build runs.
        assert_eq!(classify(Path::new("/repo/java/com/acme/A.java"), None), None);
        assert_eq!(classify(Path::new("/repo/BUILD.bazel"), None), None);
    }

    fn changes_from(paths: &[&str]) -> Vec<Change> {
        let (tx, rx) = unbounded();
        let owned: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        dispatch(owned.iter(), &tx, None);
        drop(tx);
        rx.into_iter().collect()
    }

    #[test]
    fn a_linked_worktree_uses_and_classifies_its_real_git_directory() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
        let git_dir = temp.path().join("git/worktrees/checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(checkout.join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
        let root = paths::Utf8Path::from_path(&checkout)
            .and_then(paths::AbsPath::try_new)
            .expect("absolute UTF-8 temp path");
        let canonical_git_dir = std::fs::canonicalize(&git_dir).unwrap();

        assert_eq!(resolve_git_dir(root).as_deref(), Some(canonical_git_dir.as_path()));
        assert_eq!(
            classify(&canonical_git_dir.join("HEAD"), Some(&canonical_git_dir)),
            Some(Change::Workspace)
        );
    }

    #[test]
    fn a_burst_of_shards_collapses_to_one_message() {
        // A build writes one shard per target. Ray's Java tree is seven; a real
        // repo is thousands. One reload, not thousands.
        let paths: Vec<String> =
            (0..500).map(|i| format!("/repo/bazel-bin/t{i}/t{i}.scip")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        assert_eq!(changes_from(&refs), [Change::Index]);
    }

    #[test]
    fn a_checkout_reports_the_workspace_before_the_index() {
        // Both fire during a branch switch that also swaps build outputs. The
        // broader signal comes first so a receiver acting on it has already
        // covered the narrower one.
        let changes = changes_from(&[
            "/repo/bazel-bin/a/a.scip",
            "/repo/.git/HEAD",
            "/repo/.git/index",
            "/repo/bazel-bin/b/b.scip",
        ]);
        assert_eq!(changes, [Change::Workspace, Change::Index]);
    }

    #[test]
    fn irrelevant_paths_produce_no_message() {
        assert!(changes_from(&["/repo/java/A.java", "/repo/.git/objects/aa/bb"]).is_empty());
    }

    #[test]
    fn watching_a_missing_directory_is_not_fatal() {
        // The normal state before the first build: no bazel-bin yet.
        let watcher = FileWatcher::spawn(
            Some(AbsPath::new_unchecked(paths::Utf8Path::new("/nonexistent/bazel-bin"))),
            Some(AbsPath::new_unchecked(paths::Utf8Path::new("/nonexistent/repo"))),
        );
        assert!(watcher.is_ok(), "a missing path should be skipped, not fail the server");
    }

    #[test]
    fn a_written_shard_is_reported() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = paths::Utf8Path::from_path(dir.path()).expect("utf-8 temp path");
        let watcher = FileWatcher::spawn(Some(AbsPath::new_unchecked(root)), None)
            .expect("watcher should start");

        std::fs::write(dir.path().join("core.scip"), b"not really protobuf").expect("write");

        let change = watcher
            .receiver()
            .recv_timeout(Duration::from_secs(10))
            .expect("the write should be reported");
        assert_eq!(change, Change::Index);
    }
}
