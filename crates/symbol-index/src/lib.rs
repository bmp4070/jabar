//! The global symbol index, read from SCIP shards.
//!
//! One shard per Bazel target, produced by the aspect in
//! `crates/build-model/aspects/`. Shards are independent and compose by
//! construction: a symbol string like
//! `semanticdb maven . . com/acme/core/RetryPolicy#` means the same thing in
//! every shard, so the target that *defines* it and the targets that
//! *reference* it agree without any linking step.
//!
//! This is the shallow global tier. It holds names, kinds, positions and
//! relationships — enough for `workspaceSymbol`, `goToDefinition`,
//! `findReferences` and `goToImplementation` — and no method bodies.
//!
//! # Positions are UTF-16, and SCIP does not say so
//!
//! `Document.position_encoding` exists, but scip-java leaves it
//! `UnspecifiedPositionEncoding` while emitting UTF-16 code-unit columns.
//! Verified against the fixture: an occurrence on the line declaring `grüße`
//! spans columns 29..35, which is `"String"` read as UTF-16 and `"e(Stri"` read
//! as UTF-8. The wrong reading does not fail — it returns plausible garbage,
//! and only on lines containing non-ASCII.
//!
//! So [`PositionEncoding::of`] assumes UTF-16 when the field is unspecified,
//! and a test pins that. If a future scip-java starts populating the field
//! honestly, that test is what will notice.

use std::path::Path;

use protobuf::Message as _;
use rustc_hash::FxHashMap;
use scip::types::{Index, SymbolRole};

/// Where a symbol is, in the index's own coordinates.
///
/// Columns are in whatever [`PositionEncoding`] the containing document used.
/// Converting to a client's negotiated encoding is the server's job, not this
/// crate's — it has the file text and this does not.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Range {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

impl Range {
    /// Reads SCIP's packed range encoding.
    ///
    /// Three elements mean a single-line range and four mean a multi-line one;
    /// anything else is a shard we do not understand.
    fn from_scip(raw: &[i32]) -> Option<Range> {
        match *raw {
            [line, start, end] => Some(Range {
                start_line: line as u32,
                start_col: start as u32,
                end_line: line as u32,
                end_col: end as u32,
            }),
            [start_line, start_col, end_line, end_col] => Some(Range {
                start_line: start_line as u32,
                start_col: start_col as u32,
                end_line: end_line as u32,
                end_col: end_col as u32,
            }),
            _ => None,
        }
    }
}

/// How a document's columns are counted.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    /// The encoding a document's columns are in.
    ///
    /// Defaults to UTF-16 when unspecified, which is what every JVM indexer
    /// emits — see the module docs. Guessing UTF-8 here would skew every column
    /// after the first non-ASCII character on a line.
    fn of(doc: &scip::types::Document) -> PositionEncoding {
        use scip::types::PositionEncoding as P;
        match doc.position_encoding.enum_value_or_default() {
            P::UTF8CodeUnitOffsetFromLineStart => PositionEncoding::Utf8,
            P::UTF32CodeUnitOffsetFromLineStart => PositionEncoding::Utf32,
            P::UTF16CodeUnitOffsetFromLineStart | P::UnspecifiedPositionEncoding => {
                PositionEncoding::Utf16
            }
        }
    }
}

/// What kind of thing a symbol is. A narrowing of SCIP's much longer list to
/// what a Java client can act on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Class,
    Interface,
    Enum,
    Method,
    Constructor,
    Field,
    Other,
}

impl SymbolKind {
    fn from_scip(kind: scip::types::symbol_information::Kind) -> SymbolKind {
        use scip::types::symbol_information::Kind as K;
        match kind {
            K::Class => SymbolKind::Class,
            K::Interface => SymbolKind::Interface,
            K::Enum => SymbolKind::Enum,
            K::Method | K::StaticMethod | K::AbstractMethod => SymbolKind::Method,
            K::Constructor => SymbolKind::Constructor,
            K::Field | K::StaticField | K::EnumMember => SymbolKind::Field,
            _ => SymbolKind::Other,
        }
    }
}

/// A symbol's definition site.
#[derive(Clone, Debug)]
pub struct Definition {
    /// The SCIP symbol string. Globally unique, and the key everything joins on.
    pub symbol: String,
    /// The short name a human searches for: `RetryPolicy`, `checkNotNull`.
    pub name: String,
    pub kind: SymbolKind,
    /// Workspace-relative path of the defining file.
    pub path: String,
    pub range: Range,
    pub encoding: PositionEncoding,
    /// Symbols this one implements or extends.
    pub implements: Vec<String>,
    /// Javadoc, if the indexer captured any.
    pub documentation: Vec<String>,
}

/// One use of a symbol somewhere other than its definition.
#[derive(Clone, Debug)]
pub struct Reference {
    pub symbol: String,
    pub path: String,
    pub range: Range,
    pub encoding: PositionEncoding,
    /// True when the occurrence is an `import`, which a call-graph query wants
    /// to skip and a rename does not.
    pub is_import: bool,
}

/// Symbols and references, keyed for lookup.
#[derive(Default)]
pub struct SymbolIndex {
    definitions: Vec<Definition>,
    /// Lowercased short name to definition indices, for case-insensitive search.
    by_name: FxHashMap<String, Vec<usize>>,
    /// Symbol string to definition index.
    by_symbol: FxHashMap<String, usize>,
    /// Symbol string to every reference to it.
    references: FxHashMap<String, Vec<Reference>>,
    /// Supertype symbol to the symbols implementing it.
    implementors: FxHashMap<String, Vec<usize>>,
    shards: usize,
}

impl SymbolIndex {
    /// Reads every `*.scip` under `dir`, recursively.
    ///
    /// Shards that fail to parse are logged and skipped rather than failing the
    /// load: one corrupt shard should cost its own target's symbols, not the
    /// whole index.
    pub fn from_dir(dir: &Path) -> std::io::Result<SymbolIndex> {
        let mut index = SymbolIndex::default();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            for entry in std::fs::read_dir(&current)?.flatten() {
                let path = entry.path();
                // `symlink_metadata` so a bazel-out symlink loop cannot walk forever.
                let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
                if meta.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "scip") {
                    match std::fs::read(&path) {
                        Ok(bytes) => index.add_shard(&bytes, &path.display().to_string()),
                        Err(err) => tracing::warn!(?path, %err, "unreadable shard"),
                    }
                }
            }
        }
        Ok(index)
    }

    /// Adds one shard's contents. `origin` is used only for diagnostics.
    pub fn add_shard(&mut self, bytes: &[u8], origin: &str) {
        let index = match Index::parse_from_bytes(bytes) {
            Ok(index) => index,
            Err(err) => {
                tracing::warn!(origin, %err, "unparseable SCIP shard; skipping");
                return;
            }
        };
        self.shards += 1;

        for doc in &index.documents {
            let encoding = PositionEncoding::of(doc);
            let path = doc.relative_path.clone();

            // `symbols` carries the metadata (kind, docs, relationships);
            // `occurrences` carries the positions. Join them by symbol string.
            let mut meta: FxHashMap<&str, &scip::types::SymbolInformation> = FxHashMap::default();
            for info in &doc.symbols {
                meta.insert(info.symbol.as_str(), info);
            }

            for occ in &doc.occurrences {
                let Some(range) = Range::from_scip(&occ.range) else {
                    tracing::debug!(origin, symbol = %occ.symbol, "unreadable range; skipping");
                    continue;
                };
                // Locals are per-file and useless to a global index.
                if occ.symbol.starts_with("local ") || occ.symbol.is_empty() {
                    continue;
                }
                let roles = occ.symbol_roles;
                if roles & SymbolRole::Definition as i32 != 0 {
                    let info = meta.get(occ.symbol.as_str());
                    self.push_definition(Definition {
                        symbol: occ.symbol.clone(),
                        name: short_name(&occ.symbol),
                        kind: info
                            .map(|i| SymbolKind::from_scip(i.kind.enum_value_or_default()))
                            .unwrap_or(SymbolKind::Other),
                        path: path.clone(),
                        range,
                        encoding,
                        implements: info
                            .map(|i| {
                                i.relationships
                                    .iter()
                                    .filter(|r| r.is_implementation)
                                    .map(|r| r.symbol.clone())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        documentation: info.map(|i| i.documentation.clone()).unwrap_or_default(),
                    });
                } else {
                    self.references.entry(occ.symbol.clone()).or_default().push(Reference {
                        symbol: occ.symbol.clone(),
                        path: path.clone(),
                        range,
                        encoding,
                        is_import: roles & SymbolRole::Import as i32 != 0,
                    });
                }
            }
        }
    }

    fn push_definition(&mut self, def: Definition) {
        // A symbol can be defined once. Re-indexing the same target, or two
        // shards covering one file, must not produce duplicates.
        if self.by_symbol.contains_key(&def.symbol) {
            return;
        }
        let idx = self.definitions.len();
        self.by_name.entry(def.name.to_lowercase()).or_default().push(idx);
        self.by_symbol.insert(def.symbol.clone(), idx);
        for supertype in &def.implements {
            self.implementors.entry(supertype.clone()).or_default().push(idx);
        }
        self.definitions.push(def);
    }

    pub fn shard_count(&self) -> usize {
        self.shards
    }

    pub fn definition_count(&self) -> usize {
        self.definitions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }

    /// Definitions whose short name contains `query`, case-insensitively.
    ///
    /// Exact matches sort first, then prefix matches, then the rest — so a
    /// truncated result keeps what the caller most likely meant. Ties break on
    /// name length, then path, so the order is stable across runs and a
    /// persisted index diffs cleanly.
    pub fn search(&self, query: &str) -> Vec<&Definition> {
        if query.is_empty() {
            return Vec::new();
        }
        let needle = query.to_lowercase();
        let mut hits: Vec<&Definition> =
            self.definitions.iter().filter(|d| d.name.to_lowercase().contains(&needle)).collect();
        hits.sort_by(|a, b| {
            rank(a, &needle)
                .cmp(&rank(b, &needle))
                .then_with(|| a.name.len().cmp(&b.name.len()))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.path.cmp(&b.path))
        });
        hits
    }

    pub fn definition(&self, symbol: &str) -> Option<&Definition> {
        self.by_symbol.get(symbol).map(|&i| &self.definitions[i])
    }

    /// Every reference to `symbol`, definitions excluded.
    pub fn references(&self, symbol: &str) -> &[Reference] {
        self.references.get(symbol).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Definitions declaring `symbol` as a supertype.
    pub fn implementors(&self, symbol: &str) -> Vec<&Definition> {
        self.implementors
            .get(symbol)
            .map(|idxs| idxs.iter().map(|&i| &self.definitions[i]).collect())
            .unwrap_or_default()
    }
}

/// 0 for an exact match, 1 for a prefix match, 2 otherwise.
fn rank(def: &Definition, needle: &str) -> u8 {
    let name = def.name.to_lowercase();
    if name == needle {
        0
    } else if name.starts_with(needle) {
        1
    } else {
        2
    }
}

/// The searchable short name in a SCIP symbol string.
///
/// SCIP symbols look like `semanticdb maven . . com/acme/core/RetryPolicy#` or
/// `…/Preconditions#checkNotNull().`, with trailing sigils marking what kind of
/// thing the symbol is. A client searches for `RetryPolicy`, not for any of
/// that, so the descriptor sigils and any parameter list are stripped.
fn short_name(symbol: &str) -> String {
    let tail = symbol.rsplit(['/', ' ']).next().unwrap_or(symbol);

    // A type parameter is the innermost name: `checkNotNull().[T]` is `T`.
    if let Some((_, param)) = tail.rsplit_once('[') {
        let param = param.trim_end_matches(']');
        if !param.is_empty() {
            return param.trim_matches('`').to_owned();
        }
    }

    // Drop any parameter list, then take the member after the `#` that
    // separates a type from its members. `RetryPolicy#` has no member and
    // yields the type itself.
    let tail = tail.split('(').next().unwrap_or(tail);
    let tail = tail.trim_end_matches(['#', '.', ')']);
    let tail = tail.rsplit('#').find(|part| !part.is_empty()).unwrap_or(tail);
    tail.trim_matches('`').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_strip_scip_sigils() {
        assert_eq!(short_name("semanticdb maven . . com/acme/core/RetryPolicy#"), "RetryPolicy");
        assert_eq!(
            short_name("semanticdb maven . . com/acme/util/Preconditions#checkNotNull()."),
            "checkNotNull"
        );
        assert_eq!(
            short_name("semanticdb maven . . com/acme/policy/PolicyRegistry#byName."),
            "byName"
        );
        assert_eq!(short_name("semanticdb maven jdk 26 java/util/List#"), "List");
        // Constructors are spelled with backticks.
        assert_eq!(short_name("semanticdb maven . . com/acme/A#`<init>`()."), "<init>");
        // Non-ASCII identifiers are legal Java and must survive.
        assert_eq!(short_name("semanticdb maven . . com/acme/i18n/Messages#grüße()."), "grüße");
        // A method's type parameter is named by the parameter, not the method.
        assert_eq!(
            short_name("semanticdb maven . . com/acme/util/Preconditions#checkNotNull().[T]"),
            "T"
        );
    }

    #[test]
    fn ranges_read_both_scip_encodings() {
        assert_eq!(
            Range::from_scip(&[5, 10, 20]),
            Some(Range { start_line: 5, start_col: 10, end_line: 5, end_col: 20 })
        );
        assert_eq!(
            Range::from_scip(&[5, 10, 7, 2]),
            Some(Range { start_line: 5, start_col: 10, end_line: 7, end_col: 2 })
        );
        assert_eq!(Range::from_scip(&[1, 2]), None, "too short to be a range");
        assert_eq!(Range::from_scip(&[]), None);
    }

    #[test]
    fn an_unparseable_shard_is_skipped_not_fatal() {
        // One corrupt shard should cost its own target's symbols, not the index.
        let mut index = SymbolIndex::default();
        index.add_shard(b"this is not protobuf at all", "corrupt.scip");
        assert!(index.is_empty());
        assert_eq!(index.shard_count(), 0, "a shard that did not parse was not counted");
    }

    #[test]
    fn searching_an_empty_index_finds_nothing() {
        let index = SymbolIndex::default();
        assert!(index.search("anything").is_empty());
        assert!(index.references("whatever").is_empty());
        assert!(index.implementors("whatever").is_empty());
        assert_eq!(index.definition("whatever").map(|d| &d.symbol), None);
    }

    #[test]
    fn an_empty_query_matches_nothing_rather_than_everything() {
        // `contains("")` is true for every string; returning the whole index
        // would be a very expensive way to answer a meaningless question.
        let mut index = SymbolIndex::default();
        index.push_definition(def("com/acme/A#", "A"));
        assert!(index.search("").is_empty());
    }

    fn def(symbol: &str, name: &str) -> Definition {
        Definition {
            symbol: symbol.to_owned(),
            name: name.to_owned(),
            kind: SymbolKind::Class,
            path: format!("java/{name}.java"),
            range: Range { start_line: 0, start_col: 0, end_line: 0, end_col: 1 },
            encoding: PositionEncoding::Utf16,
            implements: Vec::new(),
            documentation: Vec::new(),
        }
    }

    #[test]
    fn search_ranks_exact_then_prefix_then_substring() {
        let mut index = SymbolIndex::default();
        for (symbol, name) in [
            ("com/acme/AbstractRetryPolicyBase#", "AbstractRetryPolicyBase"),
            ("com/acme/RetryPolicy#", "RetryPolicy"),
            ("com/acme/RetryPolicyFactory#", "RetryPolicyFactory"),
        ] {
            index.push_definition(def(symbol, name));
        }
        let names: Vec<_> = index.search("retrypolicy").iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["RetryPolicy", "RetryPolicyFactory", "AbstractRetryPolicyBase"]);
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut index = SymbolIndex::default();
        index.push_definition(def("com/acme/RetryPolicy#", "RetryPolicy"));
        assert_eq!(index.search("RETRYPOLICY").len(), 1);
        assert_eq!(index.search("retrypolicy").len(), 1);
        assert_eq!(index.search("Policy").len(), 1);
    }

    #[test]
    fn a_symbol_defined_twice_is_stored_once() {
        // Re-indexing a target must not double every symbol in it.
        let mut index = SymbolIndex::default();
        index.push_definition(def("com/acme/A#", "A"));
        index.push_definition(def("com/acme/A#", "A"));
        assert_eq!(index.definition_count(), 1);
        assert_eq!(index.search("A").len(), 1);
    }

    #[test]
    fn implementors_are_indexed_by_supertype() {
        let mut index = SymbolIndex::default();
        let mut impl_def = def("com/acme/DefaultRetryPolicy#", "DefaultRetryPolicy");
        impl_def.implements = vec!["com/acme/RetryPolicy#".to_owned()];
        index.push_definition(impl_def);

        let found = index.implementors("com/acme/RetryPolicy#");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "DefaultRetryPolicy");
        assert!(index.implementors("com/acme/Unrelated#").is_empty());
    }
}
