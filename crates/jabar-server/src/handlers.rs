//! Query handlers.
//!
//! Two rules hold here, and both come from `docs/phase-1.md`:
//!
//! **An empty answer must be honest.** A client cannot distinguish "no such
//! symbol" from "the index is not loaded", and an agent acts on the first
//! reading — deleting code it believes is unreferenced. So a handler that
//! cannot answer returns an error, never an empty success, and records *why*
//! through [`telemetry`].
//!
//! **A truncated answer must say so.** `findReferences` on a common symbol
//! returns tens of thousands of locations; a silently clipped list makes an
//! agent believe it has seen every call site.

use lsp_types::{Location, Position, Range, SymbolInformation, SymbolKind as LspKind, Url};
use symbol_index::{Definition, PositionEncoding, SymbolIndex, SymbolKind};
use telemetry::{EmptyReason, Failure, Outcome};

use crate::line_index::{LineIndex, LinePosition, PositionEncoding as ClientEncoding};

/// How many results a search returns before truncating.
///
/// Results are billed in context tokens, not screen space. A few dozen ranked
/// hits is what an agent can act on; a thousand is a blown context window.
pub const SEARCH_LIMIT: usize = 50;

/// A search result, with the total before truncation.
pub struct SearchResults {
    pub symbols: Vec<SymbolInformation>,
    /// How many matched, which may exceed `symbols.len()`.
    pub total: usize,
}

impl SearchResults {
    pub fn outcome(&self) -> Outcome {
        if self.total == 0 {
            Outcome::Empty { reason: EmptyReason::NoMatch }
        } else {
            Outcome::Answered { returned: self.symbols.len(), total: self.total }
        }
    }
}

/// Answers `workspace/symbol` from the global index.
///
/// `workspace_root` is needed to turn the index's workspace-relative paths back
/// into `file://` URIs. `read_file` supplies file text for column conversion;
/// it returns `None` for a file that cannot be read, in which case the raw
/// index columns are passed through — see [`convert_range`].
pub fn workspace_symbol(
    index: &SymbolIndex,
    query: &str,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: impl Fn(&str) -> Option<String>,
) -> SearchResults {
    let matches = index.search(query);
    let total = matches.len();

    let symbols = matches
        .into_iter()
        .take(SEARCH_LIMIT)
        .filter_map(|def| to_symbol_information(def, workspace_root, client_encoding, &read_file))
        .collect();

    SearchResults { symbols, total }
}

fn to_symbol_information(
    def: &Definition,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Option<SymbolInformation> {
    let abs = workspace_root.join(&def.path);
    let uri = Url::from_file_path(abs.as_str()).ok()?;
    let range = convert_range(def, client_encoding, read_file);

    #[allow(deprecated)] // `deprecated` is a required field of the struct
    Some(SymbolInformation {
        name: def.name.clone(),
        kind: to_lsp_kind(def.kind),
        tags: None,
        deprecated: None,
        location: Location { uri, range },
        // The owning file, which is what a client shows beside the name. The
        // Bazel target would be more useful and is not in the index; see F15.
        container_name: def.path.rsplit('/').next().map(str::to_owned),
    })
}

/// Converts an index range into the client's negotiated encoding.
///
/// SCIP columns are UTF-16 code units (scip-java leaves the field unspecified;
/// see `symbol-index`). A UTF-8 client needs byte columns, which requires the
/// file's text — the two disagree on any line containing a non-ASCII character,
/// and the fixture's `Messages.java` exists to prove it.
///
/// When the text is unavailable the columns pass through unconverted. That is
/// wrong on non-ASCII lines, but it is wrong by a few columns in a file the
/// server cannot read, which beats dropping the result entirely.
fn convert_range(
    def: &Definition,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Range {
    let raw = def.range;
    let passthrough = Range {
        start: Position::new(raw.start_line, raw.start_col),
        end: Position::new(raw.end_line, raw.end_col),
    };

    // Identical encodings need no text and no work.
    let index_encoding = match def.encoding {
        PositionEncoding::Utf8 => ClientEncoding::Utf8,
        // UTF-32 is not something a JVM indexer emits; treating it as UTF-16
        // is wrong only for astral characters, and passthrough would be worse.
        PositionEncoding::Utf16 | PositionEncoding::Utf32 => ClientEncoding::Utf16,
    };
    if index_encoding == client_encoding {
        return passthrough;
    }

    let Some(text) = read_file(&def.path) else {
        tracing::debug!(path = %def.path, "no text for column conversion; passing columns through");
        return passthrough;
    };
    let line_index = LineIndex::new(&text);

    let convert = |line: u32, col: u32| -> Position {
        let offset = line_index.offset(LinePosition::new(line, col), index_encoding);
        match offset {
            Some(offset) => {
                let converted = line_index.position(offset, client_encoding);
                Position::new(converted.line, converted.character)
            }
            // A position the file does not have. The index is stale relative to
            // the text; the caller sees a slightly wrong column rather than
            // nothing.
            None => Position::new(line, col),
        }
    };

    Range { start: convert(raw.start_line, raw.start_col), end: convert(raw.end_line, raw.end_col) }
}

fn to_lsp_kind(kind: SymbolKind) -> LspKind {
    match kind {
        SymbolKind::Class => LspKind::CLASS,
        SymbolKind::Interface => LspKind::INTERFACE,
        SymbolKind::Enum => LspKind::ENUM,
        SymbolKind::Method => LspKind::METHOD,
        SymbolKind::Constructor => LspKind::CONSTRUCTOR,
        SymbolKind::Field => LspKind::FIELD,
        SymbolKind::Other => LspKind::OBJECT,
    }
}

/// The outcome for a query refused because no index is loaded.
///
/// A *failure*, not an empty result, because that is what the client is sent.
/// Recording it as an empty would inflate the misleading-empty count with cases
/// where the server behaved correctly, and that number has to stay trustworthy
/// — it is the one that says the server is lying.
pub fn index_unavailable_outcome() -> Outcome {
    Outcome::Failed { failure: Failure::IndexUnavailable }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> paths::AbsPathBuf {
        paths::AbsPathBuf::try_from("/repo").expect("absolute")
    }

    fn empty_index() -> SymbolIndex {
        SymbolIndex::default()
    }

    #[test]
    fn no_match_is_reported_as_a_truthful_empty() {
        let results =
            workspace_symbol(&empty_index(), "Nothing", &root(), ClientEncoding::Utf8, |_| None);
        assert_eq!(results.total, 0);
        assert!(results.symbols.is_empty());
        // NoMatch is the one healthy empty. An unloaded index must not use it.
        assert_eq!(results.outcome(), Outcome::Empty { reason: EmptyReason::NoMatch });
        assert!(!results.outcome().is_misleading());
    }

    #[test]
    fn refusing_is_recorded_as_a_failure_not_a_misleading_empty() {
        // The server returns an LSP error when it has no index, so nothing
        // misleading reaches the client and the misleading-empty count -- the
        // number that says the server is lying -- must stay clean.
        let outcome = index_unavailable_outcome();
        assert!(!outcome.is_misleading(), "a refusal is honest");
        assert!(matches!(outcome, Outcome::Failed { .. }));
        // Whereas answering empty without knowing is the damaging case.
        assert!(Outcome::Empty { reason: EmptyReason::IndexNotReady }.is_misleading());
        assert!(!Outcome::Empty { reason: EmptyReason::NoMatch }.is_misleading());
    }

    #[test]
    fn symbol_kinds_map_to_lsp() {
        assert_eq!(to_lsp_kind(SymbolKind::Interface), LspKind::INTERFACE);
        assert_eq!(to_lsp_kind(SymbolKind::Method), LspKind::METHOD);
        assert_eq!(to_lsp_kind(SymbolKind::Other), LspKind::OBJECT);
    }

    fn definition(path: &str, name: &str, line: u32, start: u32, end: u32) -> Definition {
        Definition {
            symbol: format!("semanticdb maven . . com/acme/{name}#"),
            name: name.to_owned(),
            kind: SymbolKind::Class,
            path: path.to_owned(),
            range: symbol_index::Range {
                start_line: line,
                start_col: start,
                end_line: line,
                end_col: end,
            },
            encoding: PositionEncoding::Utf16,
            implements: Vec::new(),
            documentation: Vec::new(),
        }
    }

    /// Builds an index holding one definition at a known range.
    fn index_with(path: &str, name: &str, line: u32, start: u32, end: u32) -> SymbolIndex {
        let mut index = SymbolIndex::default();
        index.insert(definition(path, name, line, start, end));
        index
    }

    #[test]
    fn utf16_columns_become_byte_columns_for_a_utf8_client() {
        // The conversion the fixture's Messages.java exists to force. On
        // `public static String grüße(String locale) {`, the identifier
        // `String` after `grüße` sits at UTF-16 columns 29..35 and UTF-8 bytes
        // 31..37, because ü and ß cost an extra byte each.
        // Verbatim from fixtures/megarepo, indentation included -- the columns
        // below come from a real SCIP shard for this line.
        let text = "  public static String grüße(String locale) {\n";
        let index = index_with("A.java", "String", 0, 29, 35);

        let results = workspace_symbol(&index, "String", &root(), ClientEncoding::Utf8, |_| {
            Some(text.to_owned())
        });
        let range = results.symbols[0].location.range;
        assert_eq!((range.start.character, range.end.character), (31, 37));

        // And the bytes at that range really are the identifier.
        assert_eq!(&text[31..37], "String");
    }

    #[test]
    fn a_utf16_client_gets_the_columns_unchanged() {
        let index = index_with("A.java", "String", 0, 29, 35);
        let results = workspace_symbol(&index, "String", &root(), ClientEncoding::Utf16, |_| {
            panic!("no file read should be needed when the encodings agree")
        });
        let range = results.symbols[0].location.range;
        assert_eq!((range.start.character, range.end.character), (29, 35));
    }

    #[test]
    fn an_unreadable_file_passes_columns_through_rather_than_dropping_the_hit() {
        let index = index_with("Gone.java", "Symbol", 3, 4, 10);
        let results = workspace_symbol(&index, "Symbol", &root(), ClientEncoding::Utf8, |_| None);
        assert_eq!(results.symbols.len(), 1, "the hit survives");
        assert_eq!(results.symbols[0].location.range.start.character, 4);
    }

    #[test]
    fn results_are_truncated_with_the_true_total_reported() {
        let mut index = SymbolIndex::default();
        for i in 0..(SEARCH_LIMIT + 25) {
            index.insert(definition(&format!("F{i}.java"), &format!("Widget{i}"), 0, 0, 1));
        }
        let results = workspace_symbol(&index, "Widget", &root(), ClientEncoding::Utf16, |_| None);

        assert_eq!(results.symbols.len(), SEARCH_LIMIT, "the response is capped");
        assert_eq!(results.total, SEARCH_LIMIT + 25, "but the true total is reported");
        assert!(results.outcome().is_truncated());
    }
}
