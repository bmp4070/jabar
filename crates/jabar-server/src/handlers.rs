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
    workspace_symbol_with(index, &[], query, workspace_root, client_encoding, read_file)
}

/// The same, plus definitions parsed from files the index cannot see yet.
///
/// `live` comes from the overlay. Its entries win over indexed ones for the same
/// file, because a file the client is editing is described better by what the
/// client holds than by what was built — including symbols that exist only in
/// the buffer, which is the case the overlay exists for.
pub fn workspace_symbol_with(
    index: &SymbolIndex,
    live: &[Definition],
    query: &str,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: impl Fn(&str) -> Option<String>,
) -> SearchResults {
    let needle = query.to_lowercase();
    let live_matches: Vec<&Definition> = live
        .iter()
        .filter(|def| !needle.is_empty() && def.name.to_lowercase().contains(&needle))
        .collect();

    // Files the overlay covers are represented entirely by it, so an indexed
    // symbol for such a file is a stale duplicate rather than an extra result.
    let shadowed: Vec<&str> = live.iter().map(|d| d.path.as_str()).collect();
    let indexed =
        index.search(query).into_iter().filter(|def| !shadowed.contains(&def.path.as_str()));

    // Live first: a symbol the client just wrote is what it is most likely
    // asking about.
    let all: Vec<&Definition> = live_matches.into_iter().chain(indexed).collect();
    let total = all.len();

    let symbols = all
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
    convert_span(&def.path, def.range, def.encoding, client_encoding, read_file)
}

fn convert_span(
    path: &str,
    raw: symbol_index::Range,
    index_encoding: symbol_index::PositionEncoding,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Range {
    let passthrough = Range {
        start: Position::new(raw.start_line, raw.start_col),
        end: Position::new(raw.end_line, raw.end_col),
    };

    // Identical encodings need no text and no work.
    let index_encoding = match index_encoding {
        PositionEncoding::Utf8 => ClientEncoding::Utf8,
        // UTF-32 is not something a JVM indexer emits; treating it as UTF-16
        // is wrong only for astral characters, and passthrough would be worse.
        PositionEncoding::Utf16 | PositionEncoding::Utf32 => ClientEncoding::Utf16,
    };
    if index_encoding == client_encoding {
        return passthrough;
    }

    let Some(text) = read_file(path) else {
        tracing::debug!(path, "no text for column conversion; passing columns through");
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

/// How many references a single response carries.
///
/// Raised from 200 after watching an agent hit the cap on Gerrit's
/// `ProjectCache` and fall back to `grep` to get "the complete picture". The
/// fallback was worse in both directions: it cost a shell command and 647 lines
/// of output instead of the 450 locations jabar already had, and it was wrong —
/// grep counted 50 `ProjectCacheImpl` matches, a different symbol, plus
/// comments and javadoc, and reported 522 as the true total.
///
/// So the cap was not saving tokens, it was spending more of them on a less
/// accurate answer. It exists now only to bound the pathological case: on
/// Gerrit, four symbols out of 203 probed exceed 1,000, the worst being
/// `Account` at 2,372.
pub const REFERENCE_LIMIT: usize = 1000;

/// A resolved location, plus what the index knew about it.
pub struct Located {
    pub location: Location,
    /// The SCIP symbol, so a caller can chain another query without re-resolving.
    pub symbol: String,
}

/// Resolves the symbol under a cursor to its definition.
///
/// `position` arrives in the client's encoding and is converted to the index's
/// UTF-16 columns before lookup — the two disagree on any line with a non-ASCII
/// character, which is most lines in a real internationalised codebase.
pub fn goto_definition(
    index: &SymbolIndex,
    relative_path: &str,
    position: LinePosition,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Option<Located> {
    let symbol = symbol_under_cursor(index, relative_path, position, client_encoding, read_file)?;
    let def = index.definition(&symbol)?;
    let location = to_location(
        def.path.as_str(),
        def.range,
        def.encoding,
        workspace_root,
        client_encoding,
        read_file,
    )?;
    Some(Located { location, symbol })
}

/// References to the symbol under a cursor, ranked and capped.
///
/// The definition is included when `include_declaration` is set, which is what
/// the LSP request's own parameter asks for.
pub fn find_references(
    index: &SymbolIndex,
    relative_path: &str,
    position: LinePosition,
    include_declaration: bool,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Option<ReferenceResults> {
    let symbol = symbol_under_cursor(index, relative_path, position, client_encoding, read_file)?;

    let mut hits: Vec<(&str, symbol_index::Range, symbol_index::PositionEncoding)> = Vec::new();
    if include_declaration && let Some(def) = index.definition(&symbol) {
        hits.push((def.path.as_str(), def.range, def.encoding));
    }
    for reference in index.references(&symbol) {
        hits.push((reference.path.as_str(), reference.range, reference.encoding));
    }

    // Same file first, then same directory, then everything else. A caller
    // reading a truncated list gets the references nearest what it was looking
    // at, which is the ordering an agent can act on without re-querying.
    let here_dir = parent_dir(relative_path);
    hits.sort_by_key(|(path, range, _)| {
        let proximity = if *path == relative_path {
            0
        } else if parent_dir(path) == here_dir {
            1
        } else {
            2
        };
        (proximity, path.to_owned(), range.start_line, range.start_col)
    });

    let total = hits.len();
    let locations = hits
        .into_iter()
        .take(REFERENCE_LIMIT)
        .filter_map(|(path, range, encoding)| {
            to_location(path, range, encoding, workspace_root, client_encoding, read_file)
        })
        .collect();

    Some(ReferenceResults { symbol, locations, total })
}

pub struct ReferenceResults {
    pub symbol: String,
    pub locations: Vec<Location>,
    /// How many exist, which may exceed `locations.len()`.
    pub total: usize,
}

impl ReferenceResults {
    pub fn outcome(&self) -> Outcome {
        if self.total == 0 {
            Outcome::Empty { reason: EmptyReason::NoMatch }
        } else {
            Outcome::Answered { returned: self.locations.len(), total: self.total }
        }
    }
}

/// The SCIP symbol under a client-supplied cursor position.
fn symbol_under_cursor(
    index: &SymbolIndex,
    relative_path: &str,
    position: LinePosition,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    // The index stores UTF-16 columns. A UTF-8 client's column is a byte
    // offset, and converting needs the file's text.
    let col = if client_encoding == ClientEncoding::Utf16 {
        position.character
    } else {
        let text = read_file(relative_path)?;
        let line_index = LineIndex::new(&text);
        let offset = line_index.offset(position, client_encoding)?;
        line_index.position(offset, ClientEncoding::Utf16).character
    };
    index.symbol_at(relative_path, position.line, col).map(str::to_owned)
}

fn to_location(
    relative_path: &str,
    range: symbol_index::Range,
    index_encoding: symbol_index::PositionEncoding,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Option<Location> {
    let abs = workspace_root.join(relative_path);
    let uri = Url::from_file_path(abs.as_str()).ok()?;
    Some(Location {
        uri,
        range: convert_span(relative_path, range, index_encoding, client_encoding, read_file),
    })
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

/// Documentation and signature for the symbol under a cursor.
///
/// Markdown, because every client renders it and the alternative is plain text
/// with the signature indistinguishable from the prose around it.
pub fn hover(
    index: &SymbolIndex,
    relative_path: &str,
    position: LinePosition,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Option<lsp_types::Hover> {
    let symbol = symbol_under_cursor(index, relative_path, position, client_encoding, read_file)?;
    let def = index.definition(&symbol)?;

    let mut markdown = String::new();
    if !def.signature.is_empty() {
        // A fenced block so the client syntax-highlights it rather than
        // reflowing it as prose.
        markdown.push_str("```java\n");
        markdown.push_str(&def.signature);
        markdown.push_str("\n```");
    }
    for paragraph in &def.documentation {
        if !markdown.is_empty() {
            markdown.push_str("\n\n");
        }
        markdown.push_str(paragraph.trim());
    }
    // What a symbol implements is often the most useful thing about it and is
    // nowhere else in the response.
    if !def.implements.is_empty() {
        if !markdown.is_empty() {
            markdown.push_str("\n\n");
        }
        markdown.push_str("Implements: ");
        let names: Vec<String> =
            def.implements.iter().map(|s| symbol_index::short_name_of(s)).collect();
        markdown.push_str(&names.join(", "));
    }

    // Nothing to say is not the same as no symbol here. Returning an empty
    // hover would render as a blank tooltip; `None` lets the client fall back.
    if markdown.is_empty() {
        return None;
    }

    Some(lsp_types::Hover {
        contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: markdown,
        }),
        range: None,
    })
}

/// Implementations of the interface or method under a cursor.
pub fn goto_implementation(
    index: &SymbolIndex,
    relative_path: &str,
    position: LinePosition,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Option<Vec<Location>> {
    let symbol = symbol_under_cursor(index, relative_path, position, client_encoding, read_file)?;
    let locations = index
        .implementors(&symbol)
        .into_iter()
        .filter_map(|def| {
            to_location(
                &def.path,
                def.range,
                def.encoding,
                workspace_root,
                client_encoding,
                read_file,
            )
        })
        .collect();
    Some(locations)
}

/// Every symbol declared in one file, nested by enclosing scope.
pub fn document_symbols(
    index: &SymbolIndex,
    relative_path: &str,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Vec<lsp_types::DocumentSymbol> {
    document_symbols_from(
        &index.definitions_in(relative_path),
        relative_path,
        client_encoding,
        read_file,
    )
}

/// The same, from definitions the caller already has — the overlay's, for a file
/// the index has not seen.
pub fn document_symbols_from(
    defs: &[&Definition],
    relative_path: &str,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Vec<lsp_types::DocumentSymbol> {
    let converted: Vec<(usize, lsp_types::DocumentSymbol)> = defs
        .iter()
        .enumerate()
        .map(|(i, def)| (i, to_document_symbol(def, relative_path, client_encoding, read_file)))
        .collect();

    // Nest by enclosing span: a symbol whose selection range falls inside an
    // earlier symbol's declaration is a member of it. Walking a stack works
    // because `definitions_in` returns source order, so a parent always
    // precedes its members.
    let mut roots: Vec<lsp_types::DocumentSymbol> = Vec::new();
    let mut stack: Vec<(symbol_index::Range, lsp_types::DocumentSymbol)> = Vec::new();

    for (i, symbol) in converted {
        let def = defs[i];
        while let Some((span, _)) = stack.last() {
            if contains(span, &def.range) {
                break;
            }
            let (_, finished) = stack.pop().expect("just checked");
            attach(&mut stack, &mut roots, finished);
        }
        match def.enclosing {
            Some(span) => stack.push((span, symbol)),
            // No declaration span, so nothing can nest inside it.
            None => attach(&mut stack, &mut roots, symbol),
        }
    }
    while let Some((_, finished)) = stack.pop() {
        attach(&mut stack, &mut roots, finished);
    }
    roots
}

fn attach(
    stack: &mut [(symbol_index::Range, lsp_types::DocumentSymbol)],
    roots: &mut Vec<lsp_types::DocumentSymbol>,
    symbol: lsp_types::DocumentSymbol,
) {
    match stack.last_mut() {
        Some((_, parent)) => parent.children.get_or_insert_with(Vec::new).push(symbol),
        None => roots.push(symbol),
    }
}

/// Whether `outer` wholly contains `inner`.
fn contains(outer: &symbol_index::Range, inner: &symbol_index::Range) -> bool {
    let starts_after = (inner.start_line, inner.start_col) >= (outer.start_line, outer.start_col);
    let ends_before = (inner.end_line, inner.end_col) <= (outer.end_line, outer.end_col);
    starts_after && ends_before
}

fn to_document_symbol(
    def: &Definition,
    relative_path: &str,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> lsp_types::DocumentSymbol {
    let selection =
        convert_span(relative_path, def.range, def.encoding, client_encoding, read_file);
    // The full declaration where the indexer gave one, otherwise the name --
    // LSP requires `range` to contain `selection_range`, so it can never be
    // narrower.
    let range = match def.enclosing {
        Some(span) => convert_span(relative_path, span, def.encoding, client_encoding, read_file),
        None => selection,
    };
    #[allow(deprecated)] // `deprecated` is a required field of the struct
    lsp_types::DocumentSymbol {
        name: def.name.clone(),
        detail: (!def.signature.is_empty()).then(|| def.signature.clone()),
        kind: to_lsp_kind(def.kind),
        tags: None,
        deprecated: None,
        range,
        selection_range: selection,
        children: None,
    }
}

/// The callable under a cursor, as the item a call hierarchy starts from.
///
/// The SCIP symbol rides in `data`, which LSP preserves across the prepare →
/// calls round trip. Without it the follow-up requests would have to re-resolve
/// from a position, and the client is entitled to have moved the cursor by then.
pub fn prepare_call_hierarchy(
    index: &SymbolIndex,
    relative_path: &str,
    position: LinePosition,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Option<lsp_types::CallHierarchyItem> {
    let symbol = symbol_under_cursor(index, relative_path, position, client_encoding, read_file)?;
    let def = index.definition(&symbol)?;
    // Only a callable can have callers. Starting a hierarchy from a class would
    // give a tree of things that are not calls.
    if !matches!(def.kind, SymbolKind::Method | SymbolKind::Constructor) {
        return None;
    }
    to_call_item(def, workspace_root, client_encoding, read_file)
}

/// Methods that call the given one.
///
/// Every reference to the symbol sits inside some callable; that callable is a
/// caller. References outside any callable — a field initialiser, an annotation
/// — have no caller to name and are dropped rather than attributed to the
/// enclosing class.
pub fn incoming_calls(
    index: &SymbolIndex,
    symbol: &str,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Vec<lsp_types::CallHierarchyIncomingCall> {
    // One entry per caller, carrying every site it calls from -- which is what
    // `from_ranges` is for, and why a caller appearing twice is one result.
    let mut callers: Vec<(&Definition, Vec<Range>)> = Vec::new();

    for reference in index.references(symbol).iter().take(CALL_HIERARCHY_LIMIT) {
        let Some(caller) = index.enclosing_callable(&reference.path, reference.range) else {
            continue;
        };
        let range = convert_span(
            &reference.path,
            reference.range,
            reference.encoding,
            client_encoding,
            read_file,
        );
        match callers.iter_mut().find(|(def, _)| def.symbol == caller.symbol) {
            Some((_, ranges)) => ranges.push(range),
            None => callers.push((caller, vec![range])),
        }
    }

    callers
        .into_iter()
        .filter_map(|(def, from_ranges)| {
            Some(lsp_types::CallHierarchyIncomingCall {
                from: to_call_item(def, workspace_root, client_encoding, read_file)?,
                from_ranges,
            })
        })
        .collect()
}

/// Methods the given one calls.
pub fn outgoing_calls(
    index: &SymbolIndex,
    symbol: &str,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Vec<lsp_types::CallHierarchyOutgoingCall> {
    let Some(def) = index.definition(symbol) else { return Vec::new() };
    // Without a declaration span there is no body to look inside.
    let Some(span) = def.enclosing else { return Vec::new() };

    index
        .references_within(&def.path, span)
        .into_iter()
        .filter_map(|(referenced, at)| {
            let callee = index.definition(referenced)?;
            // A body mentions types and fields as well as calls. Only callables
            // belong in a call hierarchy.
            if !matches!(callee.kind, SymbolKind::Method | SymbolKind::Constructor) {
                return None;
            }
            Some(lsp_types::CallHierarchyOutgoingCall {
                to: to_call_item(callee, workspace_root, client_encoding, read_file)?,
                // Where in *this* method the call appears.
                from_ranges: vec![convert_span(
                    &def.path,
                    at,
                    def.encoding,
                    client_encoding,
                    read_file,
                )],
            })
        })
        .take(CALL_HIERARCHY_LIMIT)
        .collect()
}

/// How many callers or callees one response carries.
///
/// Lower than the reference cap: a call hierarchy is read as a tree and expanded
/// a level at a time, so breadth past this is noise rather than context.
pub const CALL_HIERARCHY_LIMIT: usize = 100;

/// Recovers the SCIP symbol a `CallHierarchyItem` was built from.
pub fn call_item_symbol(item: &lsp_types::CallHierarchyItem) -> Option<String> {
    item.data.as_ref()?.get("symbol")?.as_str().map(str::to_owned)
}

fn to_call_item(
    def: &Definition,
    workspace_root: &paths::AbsPath,
    client_encoding: ClientEncoding,
    read_file: &impl Fn(&str) -> Option<String>,
) -> Option<lsp_types::CallHierarchyItem> {
    let abs = workspace_root.join(&def.path);
    let uri = lsp_types::Url::from_file_path(abs.as_str()).ok()?;
    let selection_range =
        convert_span(&def.path, def.range, def.encoding, client_encoding, read_file);
    // LSP requires `range` to contain `selection_range`, so a definition without
    // a declaration span uses its name for both.
    let range = match def.enclosing {
        Some(span) => convert_span(&def.path, span, def.encoding, client_encoding, read_file),
        None => selection_range,
    };
    Some(lsp_types::CallHierarchyItem {
        name: def.name.clone(),
        kind: to_lsp_kind(def.kind),
        tags: None,
        detail: (!def.signature.is_empty()).then(|| def.signature.clone()),
        uri,
        range,
        selection_range,
        data: Some(serde_json::json!({ "symbol": def.symbol })),
    })
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
            signature: String::new(),
            enclosing: None,
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
