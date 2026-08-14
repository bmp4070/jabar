//! Documents the client is holding open, and applying its edits to them.
//!
//! An open document's authoritative text lives in the client, not on disk. The
//! two disagree from the first keystroke until a save, so anything answering a
//! query about an open file has to read the client's version.
//!
//! That matters more here than in a server built for humans. An agent writes
//! files with its own tools and queries microseconds later, so the window in
//! which disk and memory disagree is the window it works in — see the
//! read-after-write finding in `docs/phase-1.md` (F3).

use lsp_types::TextDocumentContentChangeEvent;
use rustc_hash::FxHashMap;
use vfs::VfsPath;

use crate::line_index::{LineIndex, LinePosition, PositionEncoding};

/// One open document.
#[derive(Clone, Debug)]
pub struct Document {
    /// The client's version counter. Monotonic per document.
    pub version: i32,
    pub text: String,
}

/// The set of documents the client has open.
#[derive(Default, Debug)]
pub struct Documents {
    open: FxHashMap<VfsPath, Document>,
}

impl Documents {
    pub fn open(&mut self, path: VfsPath, version: i32, text: String) -> Option<Document> {
        self.open.insert(path, Document { version, text })
    }

    pub fn close(&mut self, path: &VfsPath) -> Option<Document> {
        self.open.remove(path)
    }

    pub fn get(&self, path: &VfsPath) -> Option<&Document> {
        self.open.get(path)
    }

    pub fn is_open(&self, path: &VfsPath) -> bool {
        self.open.contains_key(path)
    }

    pub fn len(&self) -> usize {
        self.open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&VfsPath, &Document)> {
        self.open.iter()
    }

    /// Applies a batch of edits, returning the new text.
    ///
    /// Returns `None` if the document is not open — which is a client protocol
    /// error, not something to paper over by creating one.
    pub fn apply_changes(
        &mut self,
        path: &VfsPath,
        version: i32,
        changes: &[TextDocumentContentChangeEvent],
        encoding: PositionEncoding,
    ) -> Option<&str> {
        let doc = self.open.get_mut(path)?;
        doc.version = version;
        for change in changes {
            apply_change(&mut doc.text, change, encoding);
        }
        Some(&doc.text)
    }
}

/// Applies one edit in place.
///
/// A change with no range replaces the whole document. Otherwise the range is
/// resolved against the text *as it currently stands*, which is why the index is
/// rebuilt per change rather than once per batch: LSP specifies that each edit
/// applies to the document produced by the previous one.
fn apply_change(
    text: &mut String,
    change: &TextDocumentContentChangeEvent,
    encoding: PositionEncoding,
) {
    let Some(range) = change.range else {
        *text = change.text.clone();
        return;
    };

    let index = LineIndex::new(text);
    let start = index.offset(to_line_position(range.start), encoding);
    let end = index.offset(to_line_position(range.end), encoding);

    let (Some(start), Some(end)) = (start, end) else {
        // A range naming a line the document does not have. Dropping the edit
        // keeps the buffer coherent; applying it at a guessed offset would
        // corrupt it silently, and the client will resync on the next full
        // change if it really has diverged.
        tracing::warn!(?range, "edit range is outside the document; dropping it");
        return;
    };
    // An inverted range is likewise a client bug. Ignoring it beats panicking
    // inside `String::replace_range`.
    if start > end {
        tracing::warn!(?range, "edit range is inverted; dropping it");
        return;
    }

    text.replace_range(start as usize..end as usize, &change.text);
}

fn to_line_position(position: lsp_types::Position) -> LinePosition {
    LinePosition::new(position.line, position.character)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn path(s: &str) -> VfsPath {
        s.parse().expect("test path should parse")
    }

    fn edit(start: (u32, u32), end: (u32, u32), text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position::new(start.0, start.1),
                end: Position::new(end.0, end.1),
            }),
            range_length: None,
            text: text.to_owned(),
        }
    }

    fn full(text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent { range: None, range_length: None, text: text.to_owned() }
    }

    fn docs_with(text: &str) -> (Documents, VfsPath) {
        let mut docs = Documents::default();
        let p = path("/repo/A.java");
        docs.open(p.clone(), 1, text.to_owned());
        (docs, p)
    }

    #[test]
    fn opening_and_closing_tracks_state() {
        let (mut docs, p) = docs_with("class A {}");
        assert!(docs.is_open(&p));
        assert_eq!(docs.get(&p).unwrap().version, 1);
        assert_eq!(docs.len(), 1);

        let closed = docs.close(&p).expect("was open");
        assert_eq!(closed.text, "class A {}");
        assert!(!docs.is_open(&p));
        assert!(docs.is_empty());
    }

    #[test]
    fn a_change_with_no_range_replaces_everything() {
        let (mut docs, p) = docs_with("old");
        let text = docs.apply_changes(&p, 2, &[full("new")], PositionEncoding::Utf8).unwrap();
        assert_eq!(text, "new");
        assert_eq!(docs.get(&p).unwrap().version, 2);
    }

    #[test]
    fn a_ranged_change_edits_in_place() {
        let (mut docs, p) = docs_with("class A {}\n");
        let text = docs
            .apply_changes(&p, 2, &[edit((0, 6), (0, 7), "Bee")], PositionEncoding::Utf8)
            .unwrap();
        assert_eq!(text, "class Bee {}\n");
    }

    #[test]
    fn edits_in_one_batch_apply_in_sequence() {
        // LSP specifies each edit applies to the document the previous one
        // produced. Resolving them all against the original text would put the
        // second edit at the wrong offset.
        let (mut docs, p) = docs_with("abc\n");
        let text = docs
            .apply_changes(
                &p,
                2,
                &[edit((0, 0), (0, 1), "XX"), edit((0, 0), (0, 1), "Y")],
                PositionEncoding::Utf8,
            )
            .unwrap();
        assert_eq!(text, "YXbc\n");
    }

    #[test]
    fn insertion_uses_an_empty_range() {
        let (mut docs, p) = docs_with("ac");
        let text = docs
            .apply_changes(&p, 2, &[edit((0, 1), (0, 1), "b")], PositionEncoding::Utf8)
            .unwrap();
        assert_eq!(text, "abc");
    }

    #[test]
    fn deletion_uses_empty_text() {
        let (mut docs, p) = docs_with("abc");
        let text =
            docs.apply_changes(&p, 2, &[edit((0, 1), (0, 2), "")], PositionEncoding::Utf8).unwrap();
        assert_eq!(text, "ac");
    }

    #[test]
    fn edits_spanning_lines() {
        let (mut docs, p) = docs_with("one\ntwo\nthree\n");
        let text = docs
            .apply_changes(&p, 2, &[edit((0, 1), (2, 2), "X")], PositionEncoding::Utf8)
            .unwrap();
        assert_eq!(text, "oXree\n");
    }

    #[test]
    fn utf16_columns_land_correctly_around_an_emoji() {
        // The whole reason encoding is negotiated. Under UTF-16 the text after
        // `🔁` starts at column 2, not 4; treating the column as bytes would
        // splice the edit into the middle of the character.
        let (mut docs, p) = docs_with("🔁 retrying");
        let text = docs
            .apply_changes(&p, 2, &[edit((0, 3), (0, 11), "done")], PositionEncoding::Utf16)
            .unwrap();
        assert_eq!(text, "🔁 done");
    }

    #[test]
    fn the_same_edit_under_utf8_uses_byte_columns() {
        let (mut docs, p) = docs_with("🔁 retrying");
        let text = docs
            .apply_changes(&p, 2, &[edit((0, 5), (0, 13), "done")], PositionEncoding::Utf8)
            .unwrap();
        assert_eq!(text, "🔁 done");
    }

    #[test]
    fn an_out_of_range_edit_is_dropped_not_applied() {
        // A range naming a line that does not exist means client and server have
        // diverged. Guessing an offset would corrupt the buffer silently.
        let (mut docs, p) = docs_with("abc\n");
        let text = docs
            .apply_changes(&p, 2, &[edit((99, 0), (99, 5), "X")], PositionEncoding::Utf8)
            .unwrap();
        assert_eq!(text, "abc\n", "the document is left intact");
    }

    #[test]
    fn an_inverted_range_is_dropped_not_panicked() {
        let (mut docs, p) = docs_with("abcdef\n");
        let text = docs
            .apply_changes(&p, 2, &[edit((0, 4), (0, 1), "X")], PositionEncoding::Utf8)
            .unwrap();
        assert_eq!(text, "abcdef\n");
    }

    #[test]
    fn editing_an_unopened_document_is_refused() {
        // Creating one here would leave the server holding a document the client
        // does not think it opened, which then never gets closed.
        let mut docs = Documents::default();
        assert!(
            docs.apply_changes(&path("/repo/Ghost.java"), 1, &[full("x")], PositionEncoding::Utf8)
                .is_none()
        );
        assert!(docs.is_empty());
    }

    #[test]
    fn reopening_replaces_the_previous_contents() {
        let (mut docs, p) = docs_with("first");
        let previous = docs.open(p.clone(), 5, "second".to_owned());
        assert_eq!(previous.map(|d| d.text).as_deref(), Some("first"));
        assert_eq!(docs.get(&p).unwrap().text, "second");
        assert_eq!(docs.len(), 1);
    }
}
