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

use std::collections::BinaryHeap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use protobuf::Message as _;
use rustc_hash::{FxHashMap, FxHashSet};
use scip::types::{Index, SymbolRole};
use serde::{Deserialize, Serialize};

/// Where a symbol is, in the index's own coordinates.
///
/// Columns are in whatever [`PositionEncoding`] the containing document used.
/// Converting to a client's negotiated encoding is the server's job, not this
/// crate's — it has the file text and this does not.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
        // An absent enclosing_range arrives as an empty vector, not as a
        // missing field, so emptiness is the normal case rather than an error.
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
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// The signature as it should be shown in a tooltip, e.g.
    /// `public boolean shouldRetry(int, Throwable)`. Empty when the indexer
    /// did not record one.
    pub signature: String,
    /// The span of the whole declaration, not just its name — the class body,
    /// the method including its body.
    ///
    /// Absent for symbols the indexer did not scope, and for anything whose
    /// declaration is its name. Used to nest symbols in `documentSymbol`.
    pub enclosing: Option<Range>,
}

/// One borrowed use of a symbol somewhere other than its definition.
///
/// References are stored compactly inside [`SymbolIndex`]. This view preserves
/// the public query fields without allocating a symbol or path for every row.
#[derive(Copy, Clone, Debug)]
pub struct Reference<'a> {
    pub symbol: &'a str,
    pub path: &'a str,
    pub range: Range,
    pub encoding: PositionEncoding,
    /// True when the occurrence is an `import`, which a call-graph query wants
    /// to skip and a rename does not.
    pub is_import: bool,
}

/// The compact representation persisted in the index snapshot.
///
/// The containing map supplies the symbol and `reference_paths` supplies the
/// path. At monolith scale this avoids two owned strings per reference.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct StoredReference {
    path: u32,
    range: Range,
    encoding: PositionEncoding,
    is_import: bool,
}

/// Allocation-free borrowed references for one symbol.
#[derive(Clone)]
pub struct References<'a> {
    symbol: &'a str,
    paths: &'a [String],
    inner: std::slice::Iter<'a, StoredReference>,
}

impl<'a> References<'a> {
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    pub fn iter(&self) -> Self {
        self.clone()
    }
}

impl<'a> Iterator for References<'a> {
    type Item = Reference<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let stored = self.inner.next()?;
        // Constructed indexes check this conversion when interning. Snapshot
        // indexes validate every id before they are returned from the reader.
        let path = &self.paths[stored.path as usize];
        Some(Reference {
            symbol: self.symbol,
            path,
            range: stored.range,
            encoding: stored.encoding,
            is_import: stored.is_import,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for References<'_> {}

/// One symbol occurrence in a file, for position lookup.
///
/// Symbols are held as ids into [`SymbolIndex::symbol_names`] rather than as
/// strings: a large repo has far more occurrences than distinct symbols, and a
/// SCIP symbol string runs to sixty-odd bytes.
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
struct Occurrence {
    range: Range,
    symbol: u32,
}

/// Symbols and references, keyed for lookup.
#[derive(Default, Serialize, Deserialize)]
pub struct SymbolIndex {
    definitions: Vec<Definition>,
    /// Lowercased short name to definition indices, for case-insensitive search.
    by_name: FxHashMap<String, Vec<usize>>,
    /// Symbol string to definition index.
    by_symbol: FxHashMap<String, usize>,
    /// Symbol string to every compact reference to it.
    references: FxHashMap<String, Vec<StoredReference>>,
    /// Interned paths used by `references`.
    reference_paths: Vec<String>,
    /// Construction-only reverse lookup for `reference_paths`. Cache reads do
    /// not rebuild it because loaded indexes are immutable query snapshots.
    #[serde(skip)]
    reference_path_ids: FxHashMap<String, u32>,
    /// Construction-only identity set. Rebuilt if shards are appended to a snapshot.
    #[serde(skip)]
    reference_sites: FxHashSet<(u32, StoredReference)>,
    /// Supertype symbol to the symbols implementing it.
    implementors: FxHashMap<String, Vec<usize>>,
    /// File path to the definitions it contains.
    by_path: FxHashMap<String, Vec<usize>>,
    /// Every occurrence in a file, ordered by position, for cursor lookup.
    occurrences: FxHashMap<String, Vec<Occurrence>>,
    /// Interned symbol strings, indexed by the ids in [`Occurrence`].
    symbol_names: Vec<String>,
    symbol_ids: FxHashMap<String, u32>,
    shards: usize,
}

/// The cheap filesystem identity of one aggregated shard. This detects shard
/// additions and normal rewrites without decoding them during reconciliation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardMetadata {
    pub path: PathBuf,
    pub len: u64,
    pub modified_ns: u64,
}

/// The result of loading a previously scanned shard set.
///
/// A partial index can still be useful, but callers must not persist it or mark
/// it verified. `failed` names every shard that could not be read or decoded so
/// the caller can retain an older complete index and retry later.
pub struct ShardLoad {
    pub index: SymbolIndex,
    pub failed: Vec<PathBuf>,
}

/// One complete, internally consistent generation of SCIP shards.
pub struct ValidatedIndex {
    pub index: SymbolIndex,
    pub shards: Vec<ShardMetadata>,
}

impl SymbolIndex {
    /// Reads aggregated `*.scip` shards under `dir`, recursively.
    /// Intermediate targetroot directories contain per-source shards already
    /// included in their sibling target shard, so they must be skipped.
    ///
    /// A corrupt, unreadable, or concurrently rewritten shard rejects the load
    /// so a caller with an older complete index can keep serving it.
    pub fn from_dir(dir: &Path) -> std::io::Result<SymbolIndex> {
        Ok(Self::load_validated(dir)?.index)
    }

    /// Walks the tree without reading or decoding shard contents.
    pub fn scan_shards(dir: &Path) -> std::io::Result<Vec<ShardMetadata>> {
        let mut shards = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            for entry in std::fs::read_dir(&current)? {
                let entry = entry?;
                let path = entry.path();
                // Do not follow directory symlinks into Bazel output loops.
                let meta = std::fs::symlink_metadata(&path)?;
                if meta.is_dir() {
                    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
                    if !name.ends_with(".scip-targetroot") && !name.ends_with(".semanticdb") {
                        stack.push(path);
                    }
                } else if path.extension().is_some_and(|e| e == "scip") {
                    // Follow file symlinks: the loader reads their targets, so
                    // the metadata must describe those same bytes.
                    let meta = std::fs::metadata(&path)?;
                    if !meta.is_file() {
                        continue;
                    }
                    let modified_ns = meta
                        .modified()?
                        .duration_since(UNIX_EPOCH)
                        .map_err(io::Error::other)
                        .and_then(|duration| {
                            u64::try_from(duration.as_nanos()).map_err(io::Error::other)
                        })?;
                    let relative = path.strip_prefix(dir).map_err(io::Error::other)?;
                    shards.push(ShardMetadata {
                        path: relative.to_path_buf(),
                        len: meta.len(),
                        modified_ns,
                    });
                }
            }
        }
        shards.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(shards)
    }

    /// Scans and decodes exactly one complete shard generation.
    ///
    /// The second scan detects a build rewriting the output tree while shards
    /// are being decoded. Any scan, read, parse, or consistency failure rejects
    /// the whole generation so callers can retain the previous index.
    pub fn load_validated(dir: &Path) -> io::Result<ValidatedIndex> {
        let shards = Self::scan_shards(dir)?;
        Self::load_validated_shards(dir, shards)
    }

    /// Decodes a clean scan result and verifies that it did not change.
    ///
    /// Refresh workers use this after comparing the initial scan with their
    /// previous generation, avoiding a redundant third walk of a large output
    /// tree when a reload is required.
    pub fn load_validated_shards(
        dir: &Path,
        shards: Vec<ShardMetadata>,
    ) -> io::Result<ValidatedIndex> {
        Self::load_validated_shards_with(dir, shards, || {})
    }

    fn load_validated_shards_with(
        dir: &Path,
        shards: Vec<ShardMetadata>,
        after_decode: impl FnOnce(),
    ) -> io::Result<ValidatedIndex> {
        let loaded = Self::load_shards(dir, &shards);
        if !loaded.failed.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} SCIP shards could not be loaded", loaded.failed.len()),
            ));
        }
        after_decode();
        if Self::scan_shards(dir)? != shards {
            return Err(io::Error::other("SCIP shards changed while they were being loaded"));
        }
        Ok(ValidatedIndex { index: loaded.index, shards })
    }

    /// Reads a previously discovered shard list without walking the tree again.
    ///
    /// This low-level compatibility API may return a partial index. Server and
    /// cache paths should use [`Self::load_validated`] or
    /// [`Self::load_validated_shards`] instead.
    pub fn from_shards(dir: &Path, shards: &[ShardMetadata]) -> SymbolIndex {
        Self::load_shards(dir, shards).index
    }

    /// Reads a shard list and reports whether every shard contributed.
    pub fn load_shards(dir: &Path, shards: &[ShardMetadata]) -> ShardLoad {
        let mut index = SymbolIndex::default();
        let mut failed = Vec::new();
        for shard in shards {
            let path = dir.join(&shard.path);
            match std::fs::read(&path) {
                Ok(bytes) => {
                    if !index.add_shard(&bytes, &path.display().to_string()) {
                        failed.push(shard.path.clone());
                    }
                }
                Err(err) => {
                    tracing::warn!(?path, %err, "unreadable shard");
                    failed.push(shard.path.clone());
                }
            }
        }
        // Query snapshots do not need the construction set. Keep the compact
        // reference rows and release the temporary deduplication table.
        index.reference_sites = FxHashSet::default();
        ShardLoad { index, failed }
    }

    /// Persists all lookup tables so a cache hit needs no SCIP decode or map build.
    pub fn write_snapshot(&self, mut writer: impl Write) -> std::io::Result<()> {
        rmp_serde::encode::write(&mut writer, self).map_err(std::io::Error::other)
    }

    pub fn read_snapshot(reader: impl Read) -> std::io::Result<SymbolIndex> {
        let mut index: SymbolIndex = rmp_serde::from_read(reader).map_err(std::io::Error::other)?;
        index.validate_snapshot()?;
        // Older snapshots can contain duplicate rows from overlapping shards.
        // Normalize them on read while keeping the serialized shape unchanged.
        for references in index.references.values_mut() {
            let mut seen = FxHashSet::default();
            references.retain(|reference| seen.insert(reference.clone()));
        }
        for occurrences in index.occurrences.values_mut() {
            occurrences.sort_by_key(|o| {
                (o.range.start_line, o.range.start_col, o.range.end_line, o.range.end_col, o.symbol)
            });
            occurrences.dedup_by(|a, b| a.range == b.range && a.symbol == b.symbol);
        }
        Ok(index)
    }

    /// Adds one shard's contents. `origin` is used only for diagnostics.
    pub fn add_shard(&mut self, bytes: &[u8], origin: &str) -> bool {
        let index = match Index::parse_from_bytes(bytes) {
            Ok(index) => index,
            Err(err) => {
                tracing::warn!(origin, %err, "unparseable SCIP shard; skipping");
                return false;
            }
        };
        self.shards += 1;

        // Snapshot serialization omits the construction set. Restore it only
        // when a caller appends another shard to a loaded snapshot.
        if self.reference_sites.is_empty() && !self.references.is_empty() {
            for (symbol, references) in &self.references {
                if let Some(&symbol_id) = self.symbol_ids.get(symbol) {
                    self.reference_sites
                        .extend(references.iter().cloned().map(|reference| (symbol_id, reference)));
                }
            }
        }

        for doc in &index.documents {
            let encoding = PositionEncoding::of(doc);
            let path = doc.relative_path.clone();
            let mut reference_path_id = None;

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
                let symbol_id = self.intern_symbol(&occ.symbol);
                self.occurrences
                    .entry(path.clone())
                    .or_default()
                    .push(Occurrence { range, symbol: symbol_id });

                if roles & SymbolRole::Definition as i32 != 0 {
                    let info = meta.get(occ.symbol.as_str());
                    self.insert(Definition {
                        symbol: occ.symbol.clone(),
                        // `display_name` is what the indexer intends a human to
                        // see. Falling back to parsing the symbol string is for
                        // shards that omit it.
                        name: info
                            .map(|i| i.display_name.clone())
                            .filter(|n| !n.is_empty())
                            .unwrap_or_else(|| short_name(&occ.symbol)),
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
                        signature: info
                            .and_then(|i| i.signature_documentation.as_ref())
                            .map(|sig| sig.text.clone())
                            .unwrap_or_default(),
                        // Carried by the occurrence, not the symbol: the same
                        // symbol can be declared in more than one place.
                        enclosing: Range::from_scip(&occ.enclosing_range),
                    });
                } else {
                    let path_id = match reference_path_id {
                        Some(path_id) => path_id,
                        None => {
                            let Some(path_id) = self.intern_reference_path(&path) else {
                                tracing::warn!(
                                    origin,
                                    "too many distinct reference paths; rejecting shard"
                                );
                                return false;
                            };
                            reference_path_id = Some(path_id);
                            path_id
                        }
                    };
                    let reference = StoredReference {
                        path: path_id,
                        range,
                        encoding,
                        is_import: roles & SymbolRole::Import as i32 != 0,
                    };
                    if self.reference_sites.insert((symbol_id, reference.clone())) {
                        self.references.entry(occ.symbol.clone()).or_default().push(reference);
                    }
                }
            }

            // Ordered once per document so lookup can stop early. SCIP emits
            // occurrences in source order already, but nothing guarantees it.
            if let Some(occurrences) = self.occurrences.get_mut(&path) {
                occurrences.sort_by_key(|o| {
                    (
                        o.range.start_line,
                        o.range.start_col,
                        o.range.end_line,
                        o.range.end_col,
                        o.symbol,
                    )
                });
                occurrences.dedup_by(|a, b| a.range == b.range && a.symbol == b.symbol);
            }
        }
        true
    }

    fn intern_symbol(&mut self, symbol: &str) -> u32 {
        if let Some(&id) = self.symbol_ids.get(symbol) {
            return id;
        }
        let id = self.symbol_names.len() as u32;
        self.symbol_names.push(symbol.to_owned());
        self.symbol_ids.insert(symbol.to_owned(), id);
        id
    }

    fn intern_reference_path(&mut self, path: &str) -> Option<u32> {
        // Snapshot reads skip the construction-only reverse map. Rebuild it
        // only if a caller later chooses to append another shard.
        if self.reference_path_ids.is_empty() && !self.reference_paths.is_empty() {
            for (id, existing) in self.reference_paths.iter().enumerate() {
                let id = u32::try_from(id).ok()?;
                self.reference_path_ids.insert(existing.clone(), id);
            }
        }
        if let Some(&id) = self.reference_path_ids.get(path) {
            return Some(id);
        }
        let id = u32::try_from(self.reference_paths.len()).ok()?;
        self.reference_paths.push(path.to_owned());
        self.reference_path_ids.insert(path.to_owned(), id);
        Some(id)
    }

    fn validate_snapshot(&self) -> io::Result<()> {
        let path_count = self.reference_paths.len();
        if self.references.values().flatten().any(|reference| reference.path as usize >= path_count)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot contains an invalid reference path id",
            ));
        }
        Ok(())
    }

    /// Adds a definition directly, for callers that build an index from
    /// something other than a SCIP shard — a dirty-file overlay, or a test.
    ///
    /// An already indexed declaration site is ignored, so overlapping targets
    /// cannot double it while distinct sites for one symbol remain visible.
    pub fn insert(&mut self, def: Definition) {
        // One symbol can have several real declaration sites. Only the same
        // symbol at the same path and range is a duplicate shard row.
        if self.by_path.get(&def.path).is_some_and(|sites| {
            sites.iter().any(|&i| {
                self.definitions[i].symbol == def.symbol && self.definitions[i].range == def.range
            })
        }) {
            return;
        }
        let idx = self.definitions.len();
        self.by_path.entry(def.path.clone()).or_default().push(idx);
        self.by_name.entry(def.name.to_lowercase()).or_default().push(idx);
        self.by_symbol.entry(def.symbol.clone()).or_insert(idx);
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

    /// Total references across every symbol, for benchmark parity checks.
    ///
    /// Sums the per-symbol reference lists rather than counting distinct
    /// symbols, so a cache-loaded index and a shard-built one can be compared
    /// on the same count the measurement protocol requires.
    pub fn reference_count(&self) -> usize {
        self.references.values().map(Vec::len).sum()
    }

    /// Number of distinct paths retained for reference rows.
    pub fn reference_path_count(&self) -> usize {
        self.reference_paths.len()
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
        let mut hits: Vec<RankedDefinition<'_>> = self
            .by_name
            .iter()
            .filter(|(name, _)| name.contains(&needle))
            .flat_map(|(name, indices)| {
                indices.iter().map(|&idx| RankedDefinition {
                    rank: rank(name, &needle),
                    definition: &self.definitions[idx],
                })
            })
            .collect();
        hits.sort_unstable();
        hits.into_iter().map(|hit| hit.definition).collect()
    }

    /// The best `limit` search results and the total number of matches.
    ///
    /// This scans the lowercased name table without allocating a lowercased
    /// string per definition. It keeps only the best `limit` definitions in
    /// memory, which bounds broad-query memory use for large workspaces.
    pub fn search_limited(
        &self,
        query: &str,
        limit: usize,
        mut include: impl FnMut(&Definition) -> bool,
    ) -> (Vec<&Definition>, usize) {
        if query.is_empty() {
            return (Vec::new(), 0);
        }
        let needle = query.to_lowercase();
        let mut best = BinaryHeap::with_capacity(limit.min(self.definitions.len()));
        let mut total = 0;

        for (name, indices) in &self.by_name {
            if !name.contains(&needle) {
                continue;
            }
            let rank = rank(name, &needle);
            for &idx in indices {
                let definition = &self.definitions[idx];
                if !include(definition) {
                    continue;
                }
                total += 1;
                if limit == 0 {
                    continue;
                }
                let candidate = RankedDefinition { rank, definition };
                if best.len() < limit {
                    best.push(candidate);
                } else if best.peek().is_some_and(|worst| candidate < *worst) {
                    best.pop();
                    best.push(candidate);
                }
            }
        }

        let results = best.into_sorted_vec().into_iter().map(|hit| hit.definition).collect();
        (results, total)
    }

    /// The symbol whose occurrence covers `(line, col)` in `path`.
    ///
    /// `col` is a UTF-16 code unit offset, matching what the index stores — the
    /// caller converts from the client's encoding first.
    ///
    /// Ranges are half-open at the end, so a cursor resting immediately after an
    /// identifier does not select it. That matches how editors place a caret and
    /// avoids `foo|(bar)` resolving to `foo` when the user means the call.
    ///
    /// When occurrences overlap — a generic type argument inside a wider type
    /// reference — the narrowest wins, since that is what the cursor is most
    /// precisely on.
    pub fn symbol_at(&self, path: &str, line: u32, col: u32) -> Option<&str> {
        let occurrences = self.occurrences.get(path)?;
        let mut best: Option<&Occurrence> = None;
        for occ in occurrences {
            // Sorted by start, so once a start is past the cursor's line we are
            // done -- multi-line ranges starting earlier are already seen.
            if occ.range.start_line > line {
                break;
            }
            if !covers(&occ.range, line, col) {
                continue;
            }
            let narrower = match best {
                None => true,
                Some(current) => span_len(&occ.range) < span_len(&current.range),
            };
            if narrower {
                best = Some(occ);
            }
        }
        best.map(|o| self.symbol_names[o.symbol as usize].as_str())
    }

    /// The innermost callable whose declaration span contains `range`.
    ///
    /// This is what turns a reference into a caller: an occurrence of `foo`
    /// sitting inside `bar`'s span means `bar` calls `foo`. Only methods and
    /// constructors qualify — a reference inside a class body but outside any
    /// method is a field initialiser, which has no caller to name.
    ///
    /// Innermost wins, so a call inside a nested class's method attributes to
    /// that method rather than to the outer one containing it.
    pub fn enclosing_callable(&self, path: &str, range: Range) -> Option<&Definition> {
        self.by_path
            .get(path)?
            .iter()
            .map(|&i| &self.definitions[i])
            .filter(|def| matches!(def.kind, SymbolKind::Method | SymbolKind::Constructor))
            .filter(|def| def.enclosing.is_some_and(|span| encloses(&span, &range)))
            .min_by_key(|def| def.enclosing.map(|s| span_len(&s)).unwrap_or(u64::MAX))
    }

    /// Symbols referenced within `span` in `path`, excluding definitions.
    ///
    /// The reverse direction: what a method's body mentions. Returns each
    /// distinct symbol once, with the first position it appears at, because a
    /// call hierarchy wants "calls X" rather than "calls X four times".
    pub fn references_within(&self, path: &str, span: Range) -> Vec<(&str, Range)> {
        self.references_within_owner(path, span, None)
    }

    /// References whose innermost callable is `owner`.
    ///
    /// A method's declaration span can contain a nested class and its methods.
    /// Their calls belong to those nested methods, not to the outer method.
    pub fn references_within_callable(
        &self,
        path: &str,
        span: Range,
        owner: &str,
    ) -> Vec<(&str, Range)> {
        self.references_within_owner(path, span, Some(owner))
    }

    fn references_within_owner(
        &self,
        path: &str,
        span: Range,
        owner: Option<&str>,
    ) -> Vec<(&str, Range)> {
        let Some(occurrences) = self.occurrences.get(path) else { return Vec::new() };
        let mut seen_ids = FxHashSet::default();
        let mut seen = Vec::new();
        for occ in occurrences {
            if occ.range.start_line > span.end_line {
                break;
            }
            if !encloses(&span, &occ.range) {
                continue;
            }
            if let Some(owner) = owner
                && self
                    .enclosing_callable(path, occ.range)
                    .is_some_and(|callable| callable.symbol != owner)
            {
                continue;
            }
            let symbol = self.symbol_names[occ.symbol as usize].as_str();
            // A definition inside the span is the method itself, or something
            // declared in it -- neither is a call.
            if self.by_path.get(path).is_some_and(|sites| {
                sites.iter().any(|&i| {
                    self.definitions[i].symbol == symbol && self.definitions[i].range == occ.range
                })
            }) {
                continue;
            }
            if seen_ids.insert(occ.symbol) {
                seen.push((symbol, occ.range));
            }
        }
        seen
    }

    /// Total occurrences held, across every file.
    pub fn occurrence_count(&self) -> usize {
        self.occurrences.values().map(Vec::len).sum()
    }

    /// Every definition in one file, in source order.
    ///
    /// Ordered by position so a caller can nest them: a definition whose range
    /// falls inside a preceding one's `enclosing` span is a child of it.
    pub fn definitions_in(&self, path: &str) -> Vec<&Definition> {
        let mut defs: Vec<&Definition> = self
            .by_path
            .get(path)
            .map(|idxs| idxs.iter().map(|&i| &self.definitions[i]).collect())
            .unwrap_or_default();
        defs.sort_by_key(|d| (d.range.start_line, d.range.start_col));
        defs
    }

    pub fn definition(&self, symbol: &str) -> Option<&Definition> {
        self.by_symbol.get(symbol).map(|&i| &self.definitions[i])
    }

    /// Every reference to `symbol`, definitions excluded.
    pub fn references<'a>(&'a self, symbol: &str) -> References<'a> {
        let (symbol, references) = self
            .references
            .get_key_value(symbol)
            .map(|(symbol, references)| (symbol.as_str(), references.as_slice()))
            .unwrap_or(("", &[]));
        References { symbol, paths: &self.reference_paths, inner: references.iter() }
    }

    /// Definitions declaring `symbol` as a supertype.
    pub fn implementors(&self, symbol: &str) -> Vec<&Definition> {
        self.implementors
            .get(symbol)
            .map(|idxs| idxs.iter().map(|&i| &self.definitions[i]).collect())
            .unwrap_or_default()
    }
}

/// Whether `outer` wholly contains `inner`.
fn encloses(outer: &Range, inner: &Range) -> bool {
    (inner.start_line, inner.start_col) >= (outer.start_line, outer.start_col)
        && (inner.end_line, inner.end_col) <= (outer.end_line, outer.end_col)
}

/// Whether `range` covers `(line, col)`, treating the end as exclusive.
fn covers(range: &Range, line: u32, col: u32) -> bool {
    if line < range.start_line || line > range.end_line {
        return false;
    }
    if line == range.start_line && col < range.start_col {
        return false;
    }
    if line == range.end_line && col >= range.end_col {
        return false;
    }
    true
}

/// A comparable size for a range, for picking the narrowest of several.
///
/// Multi-line ranges are always wider than single-line ones, so the line span
/// dominates and the column span breaks ties.
fn span_len(range: &Range) -> u64 {
    let lines = (range.end_line - range.start_line) as u64;
    let cols = range.end_col.saturating_sub(range.start_col) as u64;
    lines * u64::from(u32::MAX) + cols
}

/// 0 for an exact match, 1 for a prefix match, 2 otherwise.
fn rank(name: &str, needle: &str) -> u8 {
    if name == needle {
        0
    } else if name.starts_with(needle) {
        1
    } else {
        2
    }
}

/// One search candidate, ordered from best to worst. A [`BinaryHeap`] keeps
/// the worst retained candidate at its head so a better one can replace it.
#[derive(Copy, Clone)]
struct RankedDefinition<'a> {
    rank: u8,
    definition: &'a Definition,
}

impl PartialEq for RankedDefinition<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for RankedDefinition<'_> {}

impl PartialOrd for RankedDefinition<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedDefinition<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank
            .cmp(&other.rank)
            .then_with(|| self.definition.name.len().cmp(&other.definition.name.len()))
            .then_with(|| self.definition.name.cmp(&other.definition.name))
            .then_with(|| self.definition.path.cmp(&other.definition.path))
            .then_with(|| self.definition.symbol.cmp(&other.definition.symbol))
    }
}

/// The searchable short name in a SCIP symbol string.
///
/// Public as [`short_name_of`] for callers rendering a symbol they only have as
/// a string — a supertype named in a relationship, say, which may be defined in
/// a shard this index never loaded.
pub fn short_name_of(symbol: &str) -> String {
    short_name(symbol)
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
    fn references_intern_paths_and_round_trip_without_allocating_views() {
        let symbol = "semanticdb maven example lib example/Library#call().";
        let mut shard = Index::new();
        let mut document = scip::types::Document::new();
        document.relative_path = "src/Caller.java".to_owned();
        document.position_encoding =
            scip::types::PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into();
        for (line, roles) in [(3, 0), (7, SymbolRole::Import as i32)] {
            let mut occurrence = scip::types::Occurrence::new();
            occurrence.range = vec![line, 4, 8];
            occurrence.symbol = symbol.to_owned();
            occurrence.symbol_roles = roles;
            document.occurrences.push(occurrence);
        }
        shard.documents.push(document);

        let mut index = SymbolIndex::default();
        assert!(index.add_shard(&shard.write_to_bytes().unwrap(), "references.scip"));
        assert_eq!(index.reference_count(), 2);
        assert_eq!(index.reference_path_count(), 1);
        assert!(std::mem::size_of::<StoredReference>() <= 24);

        let before: Vec<_> = index
            .references(symbol)
            .map(|reference| {
                (
                    reference.symbol.to_owned(),
                    reference.path.to_owned(),
                    reference.range,
                    reference.encoding,
                    reference.is_import,
                )
            })
            .collect();
        assert_eq!(before[0].1, "src/Caller.java");
        assert!(!before[0].4);
        assert!(before[1].4);

        let mut snapshot = Vec::new();
        index.write_snapshot(&mut snapshot).unwrap();
        let restored = SymbolIndex::read_snapshot(snapshot.as_slice()).unwrap();
        let mut restored = restored;
        let after: Vec<_> = restored
            .references(symbol)
            .map(|reference| {
                (
                    reference.symbol.to_owned(),
                    reference.path.to_owned(),
                    reference.range,
                    reference.encoding,
                    reference.is_import,
                )
            })
            .collect();
        assert_eq!(after, before);
        assert_eq!(restored.reference_path_count(), 1);

        // A loaded index remains safely appendable without duplicating an
        // already-interned path when the skipped reverse map is rebuilt.
        assert!(restored.add_shard(&shard.write_to_bytes().unwrap(), "again.scip"));
        assert_eq!(restored.reference_path_count(), 1);
        assert_eq!(restored.reference_count(), 2, "appending the same shard remains idempotent");
    }

    #[test]
    fn overlapping_shards_deduplicate_sites_without_losing_distinct_resolutions() {
        let symbol = "semanticdb maven example lib example/Library#call().";
        let other = "semanticdb maven example lib example/Other#call().";
        let mut first = Index::new();
        let mut doc = scip::types::Document::new();
        doc.relative_path = "src/Caller.java".to_owned();
        for (name, line, roles) in
            [(symbol, 1, SymbolRole::Definition as i32), (symbol, 4, 0), (other, 4, 0)]
        {
            let mut occurrence = scip::types::Occurrence::new();
            occurrence.range = vec![line, 2, 6];
            occurrence.symbol = name.to_owned();
            occurrence.symbol_roles = roles;
            doc.occurrences.push(occurrence);
        }
        first.documents.push(doc);

        let mut second = first.clone();
        let doc = &mut second.documents[0];
        let mut distinct_site = doc.occurrences[0].clone();
        distinct_site.range = vec![2, 2, 6];
        doc.occurrences.push(distinct_site);
        let mut distinct_reference = doc.occurrences[1].clone();
        distinct_reference.range = vec![5, 2, 6];
        doc.occurrences.push(distinct_reference);

        let mut index = SymbolIndex::default();
        assert!(index.add_shard(&first.write_to_bytes().unwrap(), "first.scip"));
        assert!(index.add_shard(&second.write_to_bytes().unwrap(), "second.scip"));
        assert_eq!(index.definition_count(), 2);
        assert_eq!(index.definitions_in("src/Caller.java").len(), 2);
        assert_eq!(index.search("call").len(), 2);
        assert_eq!(index.reference_count(), 3);
        assert_eq!(index.references(symbol).len(), 2);
        assert_eq!(index.references(other).len(), 1);
        assert_eq!(index.occurrence_count(), 5);

        let mut snapshot = Vec::new();
        index.write_snapshot(&mut snapshot).unwrap();
        let mut restored = SymbolIndex::read_snapshot(snapshot.as_slice()).unwrap();
        assert!(restored.add_shard(&second.write_to_bytes().unwrap(), "again.scip"));
        assert_eq!(restored.definition_count(), 2);
        assert_eq!(restored.reference_count(), 3);
        assert_eq!(restored.occurrence_count(), 5);
    }

    #[test]
    fn older_snapshot_duplicate_rows_are_normalized_on_read() {
        let mut index = SymbolIndex::default();
        let symbol = "external symbol";
        let symbol_id = index.intern_symbol(symbol);
        let path_id = index.intern_reference_path("A.java").unwrap();
        let site = StoredReference {
            path: path_id,
            range: Range { start_line: 1, start_col: 2, end_line: 1, end_col: 3 },
            encoding: PositionEncoding::Utf16,
            is_import: false,
        };
        index.references.insert(symbol.to_owned(), vec![site.clone(), site]);
        let occurrence = Occurrence {
            range: Range { start_line: 1, start_col: 2, end_line: 1, end_col: 3 },
            symbol: symbol_id,
        };
        index.occurrences.insert("A.java".to_owned(), vec![occurrence, occurrence]);

        let mut snapshot = Vec::new();
        index.write_snapshot(&mut snapshot).unwrap();
        let restored = SymbolIndex::read_snapshot(snapshot.as_slice()).unwrap();
        assert_eq!(restored.reference_count(), 1);
        assert_eq!(restored.occurrence_count(), 1);
    }

    #[test]
    fn snapshot_rejects_an_invalid_reference_path_id() {
        let mut index = SymbolIndex::default();
        index.references.insert(
            "external symbol".to_owned(),
            vec![StoredReference {
                path: 1,
                range: Range { start_line: 0, start_col: 0, end_line: 0, end_col: 1 },
                encoding: PositionEncoding::Utf16,
                is_import: false,
            }],
        );

        let mut snapshot = Vec::new();
        index.write_snapshot(&mut snapshot).unwrap();
        let error = match SymbolIndex::read_snapshot(snapshot.as_slice()) {
            Ok(_) => panic!("invalid path id should reject the snapshot"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn an_unparseable_shard_is_skipped_not_fatal() {
        // The low-level decoder reports failure without retaining a malformed
        // shard. Complete-generation callers reject the encompassing load.
        let mut index = SymbolIndex::default();
        assert!(!index.add_shard(b"this is not protobuf at all", "corrupt.scip"));
        assert!(index.is_empty());
        assert_eq!(index.shard_count(), 0, "a shard that did not parse was not counted");
    }

    #[test]
    fn a_partial_shard_load_reports_every_failed_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("good.scip"), Index::new().write_to_bytes().unwrap())
            .unwrap();
        std::fs::write(dir.path().join("bad.scip"), b"not protobuf").unwrap();
        let shards = vec![
            ShardMetadata { path: "good.scip".into(), len: 0, modified_ns: 0 },
            ShardMetadata { path: "bad.scip".into(), len: 0, modified_ns: 0 },
            ShardMetadata { path: "missing.scip".into(), len: 0, modified_ns: 0 },
        ];

        let loaded = SymbolIndex::load_shards(dir.path(), &shards);
        assert_eq!(loaded.index.shard_count(), 1);
        assert_eq!(loaded.failed, [PathBuf::from("bad.scip"), PathBuf::from("missing.scip")]);
    }

    #[test]
    fn a_validated_generation_rejects_a_partial_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("good.scip"), Index::new().write_to_bytes().unwrap())
            .unwrap();
        std::fs::write(dir.path().join("bad.scip"), b"not protobuf").unwrap();

        assert!(SymbolIndex::load_validated(dir.path()).is_err());
    }

    #[test]
    fn intermediate_scip_shards_are_not_loaded_twice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let bytes = Index::new().write_to_bytes().expect("encode SCIP");
        std::fs::write(dir.path().join("target.scip"), &bytes).expect("target shard");
        for suffix in ["scip-targetroot", "semanticdb"] {
            let intermediate = dir.path().join(format!("target.{suffix}"));
            std::fs::create_dir(&intermediate).expect("targetroot");
            std::fs::write(intermediate.join("source.scip"), &bytes).expect("source shard");
        }
        let index = SymbolIndex::from_dir(dir.path()).expect("load index");
        assert_eq!(index.shard_count(), 1, "only the aggregated target shard is loaded");
    }

    #[cfg(unix)]
    #[test]
    fn shard_metadata_follows_file_symlink_targets() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"one").unwrap();
        symlink(&target, dir.path().join("target.scip")).unwrap();
        let before = SymbolIndex::scan_shards(dir.path()).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].len, 3);

        std::fs::write(&target, b"longer shard").unwrap();
        let after = SymbolIndex::scan_shards(dir.path()).unwrap();
        assert_eq!(after[0].len, 12);
        assert_ne!(before, after);
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_shard_symlink_makes_the_scan_incomplete() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        symlink(dir.path().join("missing"), dir.path().join("broken.scip")).unwrap();

        assert!(SymbolIndex::scan_shards(dir.path()).is_err());
    }

    #[test]
    fn a_generation_changed_after_decode_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let shard = dir.path().join("target.scip");
        std::fs::write(&shard, Index::new().write_to_bytes().unwrap()).unwrap();

        let shards = SymbolIndex::scan_shards(dir.path()).unwrap();
        let result = SymbolIndex::load_validated_shards_with(dir.path(), shards, || {
            std::fs::write(&shard, b"a different generation").unwrap();
        });

        assert!(result.is_err());
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
        index.insert(def("com/acme/A#", "A"));
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
            signature: String::new(),
            enclosing: None,
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
            index.insert(def(symbol, name));
        }
        let names: Vec<_> = index.search("retrypolicy").iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["RetryPolicy", "RetryPolicyFactory", "AbstractRetryPolicyBase"]);
    }

    #[test]
    fn limited_search_keeps_the_best_results_and_counts_all_matches() {
        let mut index = SymbolIndex::default();
        for (symbol, name) in [
            ("com/acme/AbstractRetryPolicyBase#", "AbstractRetryPolicyBase"),
            ("com/acme/RetryPolicy#", "RetryPolicy"),
            ("com/acme/RetryPolicyFactory#", "RetryPolicyFactory"),
        ] {
            index.insert(def(symbol, name));
        }

        let (hits, total) = index.search_limited("retrypolicy", 2, |_| true);
        let names: Vec<_> = hits.iter().map(|def| def.name.as_str()).collect();
        assert_eq!(names, ["RetryPolicy", "RetryPolicyFactory"]);
        assert_eq!(total, 3);
    }

    #[test]
    fn limited_search_counts_only_included_matches_even_at_a_zero_limit() {
        let mut index = SymbolIndex::default();
        index.insert(def("com/acme/RetryPolicy#", "RetryPolicy"));
        index.insert(def("com/acme/RetryPolicyFactory#", "RetryPolicyFactory"));

        let (hits, total) =
            index.search_limited("retry", 0, |definition| definition.name != "RetryPolicy");
        assert!(hits.is_empty());
        assert_eq!(total, 1);
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut index = SymbolIndex::default();
        index.insert(def("com/acme/RetryPolicy#", "RetryPolicy"));
        assert_eq!(index.search("RETRYPOLICY").len(), 1);
        assert_eq!(index.search("retrypolicy").len(), 1);
        assert_eq!(index.search("Policy").len(), 1);
    }

    #[test]
    fn a_symbol_defined_twice_is_stored_once() {
        // Re-indexing a target must not double every symbol in it.
        let mut index = SymbolIndex::default();
        index.insert(def("com/acme/A#", "A"));
        index.insert(def("com/acme/A#", "A"));
        assert_eq!(index.definition_count(), 1);
        assert_eq!(index.search("A").len(), 1);
    }

    fn r(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range { start_line: sl, start_col: sc, end_line: el, end_col: ec }
    }

    #[test]
    fn a_range_covers_its_own_span_but_not_the_character_after() {
        let range = r(5, 10, 5, 15);
        assert!(covers(&range, 5, 10), "the first character is inside");
        assert!(covers(&range, 5, 14), "the last character is inside");
        // Half-open: a caret resting after an identifier does not select it,
        // which is how editors place the cursor after a click at a word's end.
        assert!(!covers(&range, 5, 15));
        assert!(!covers(&range, 5, 9));
        assert!(!covers(&range, 4, 12), "wrong line");
        assert!(!covers(&range, 6, 12));
    }

    #[test]
    fn multi_line_ranges_cover_their_interior() {
        let range = r(2, 30, 4, 5);
        assert!(covers(&range, 2, 30), "start of the first line");
        assert!(!covers(&range, 2, 29), "before the start on the first line");
        assert!(covers(&range, 3, 0), "any column on an interior line");
        assert!(covers(&range, 3, 9999));
        assert!(covers(&range, 4, 4), "up to the end on the last line");
        assert!(!covers(&range, 4, 5));
    }

    #[test]
    fn the_narrowest_overlapping_range_wins() {
        // `Map<String, RetryPolicy>` -- the cursor on `RetryPolicy` sits inside
        // both the whole type reference and the argument. The argument is what
        // the user means.
        assert!(span_len(&r(0, 10, 0, 21)) < span_len(&r(0, 0, 0, 30)));
        // A multi-line range is always wider than a single-line one.
        assert!(span_len(&r(0, 0, 0, 999)) < span_len(&r(0, 0, 1, 0)));
    }

    #[test]
    fn enclosing_picks_the_innermost_callable() {
        assert!(encloses(&r(0, 0, 10, 0), &r(5, 4, 5, 9)));
        assert!(!encloses(&r(0, 0, 10, 0), &r(11, 0, 11, 5)));
        // A method's span is narrower than its class's, so it wins.
        assert!(span_len(&r(5, 2, 8, 3)) < span_len(&r(0, 0, 20, 1)));
    }

    #[test]
    fn call_hierarchy_lookups_are_empty_on_an_unknown_file() {
        let index = SymbolIndex::default();
        assert!(index.enclosing_callable("nowhere.java", r(0, 0, 0, 1)).is_none());
        assert!(index.references_within("nowhere.java", r(0, 0, 99, 0)).is_empty());
    }

    #[test]
    fn an_outer_callable_does_not_claim_calls_from_a_nested_callable() {
        let mut index = SymbolIndex::default();
        let mut outer = def("com/acme/A#outer().", "outer");
        outer.kind = SymbolKind::Method;
        outer.path = "java/A.java".into();
        outer.range = r(0, 5, 0, 10);
        outer.enclosing = Some(r(0, 0, 20, 1));
        let outer_symbol = outer.symbol.clone();
        index.insert(outer);

        let mut nested = def("com/acme/A$Nested#inner().", "inner");
        nested.kind = SymbolKind::Method;
        nested.path = "java/A.java".into();
        nested.range = r(5, 5, 5, 10);
        nested.enclosing = Some(r(5, 0, 10, 1));
        index.insert(nested);

        let outer_call = index.intern_symbol("com/acme/B#outerCallee().");
        let nested_call = index.intern_symbol("com/acme/B#nestedCallee().");
        index.occurrences.insert(
            "java/A.java".into(),
            vec![
                Occurrence { range: r(2, 4, 2, 15), symbol: outer_call },
                Occurrence { range: r(7, 4, 7, 16), symbol: nested_call },
            ],
        );

        let calls = index.references_within_callable("java/A.java", r(0, 0, 20, 1), &outer_symbol);
        assert_eq!(calls, [("com/acme/B#outerCallee().", r(2, 4, 2, 15))]);
    }

    #[test]
    fn a_position_in_an_unknown_file_resolves_to_nothing() {
        let index = SymbolIndex::default();
        assert_eq!(index.symbol_at("nowhere/A.java", 0, 0), None);
        assert_eq!(index.occurrence_count(), 0);
    }

    #[test]
    fn implementors_are_indexed_by_supertype() {
        let mut index = SymbolIndex::default();
        let mut impl_def = def("com/acme/DefaultRetryPolicy#", "DefaultRetryPolicy");
        impl_def.implements = vec!["com/acme/RetryPolicy#".to_owned()];
        index.insert(impl_def);

        let found = index.implementors("com/acme/RetryPolicy#");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "DefaultRetryPolicy");
        assert!(index.implementors("com/acme/Unrelated#").is_empty());
    }
}
