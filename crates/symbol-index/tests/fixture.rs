//! Reads real SCIP shards and checks them against `EXPECTATIONS.md`.
//!
//! The shards are produced by the aspect in `crates/build-model/aspects/`, and
//! `EXPECTATIONS.md` states the golden answers independently of any indexer.
//! Together they make this a conformance suite rather than a change detector:
//! the numbers here were written down before an index existed.
//!
//! Generating the shards needs Bazel, a JDK, and `scip-java`, so they are not
//! checked in. Point `JABAR_SCIP_DIR` at a directory of `*.scip` files to run
//! these; without it they skip. See `crates/build-model/aspects/README.md`.

use symbol_index::{SymbolIndex, SymbolKind};

/// The fixture index, or `None` when the environment cannot produce one.
fn index() -> Option<SymbolIndex> {
    let dir = std::env::var_os("JABAR_SCIP_DIR")?;
    let index = SymbolIndex::from_dir(std::path::Path::new(&dir))
        .unwrap_or_else(|e| panic!("JABAR_SCIP_DIR is set but unreadable: {e}"));
    assert!(!index.is_empty(), "JABAR_SCIP_DIR contained no usable shards");
    Some(index)
}

/// `EXPECTATIONS.md`: one shard per Java target, 11 of them.
#[test]
fn every_java_target_produced_a_shard() {
    let Some(index) = index() else { return };
    assert_eq!(index.shard_count(), 11, "expected one shard per Java target");
}

/// `EXPECTATIONS.md`: 1 interface in `core`, 3 implementations across `policy`
/// (×2) and `service/payment` (×1).
#[test]
fn retry_policy_has_three_implementations_across_two_targets() {
    let Some(index) = index() else { return };

    let interface = index
        .search("RetryPolicy")
        .into_iter()
        .find(|d| d.name == "RetryPolicy")
        .expect("the interface itself should be indexed");
    assert_eq!(interface.kind, SymbolKind::Interface);
    assert!(interface.path.ends_with("core/RetryPolicy.java"), "path: {}", interface.path);

    let mut implementors: Vec<_> =
        index.implementors(&interface.symbol).iter().map(|d| d.name.clone()).collect();
    implementors.sort();
    assert_eq!(
        implementors,
        ["CircuitBreakerPolicy", "DefaultRetryPolicy", "PaymentRetryPolicy"],
        "implementations span targets that do not depend on each other"
    );
}

/// `EXPECTATIONS.md`: 3 implementations, all in one target.
#[test]
fn backoff_has_three_implementations_in_one_target() {
    let Some(index) = index() else { return };
    let interface = index
        .search("Backoff")
        .into_iter()
        .find(|d| d.name == "Backoff")
        .expect("Backoff should be indexed");

    let mut names: Vec<_> =
        index.implementors(&interface.symbol).iter().map(|d| d.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["ExponentialBackoff", "FixedBackoff", "JitteredBackoff"]);
}

/// `EXPECTATIONS.md`: the high fan-out case — 30 call sites across 15 files in
/// 8 targets. This is the truncation test, so the count has to be right before
/// ranking can matter.
#[test]
fn check_not_null_has_thirty_references_across_fifteen_files() {
    let Some(index) = index() else { return };

    let def = index
        .search("checkNotNull")
        .into_iter()
        .find(|d| d.kind == SymbolKind::Method)
        .expect("checkNotNull should be indexed as a method");
    assert!(def.path.ends_with("util/Preconditions.java"), "path: {}", def.path);

    let references = index.references(&def.symbol);
    assert_eq!(references.len(), 30, "EXPECTATIONS.md records 30 call sites");

    let mut files: Vec<_> = references.iter().map(|r| r.path.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    assert_eq!(files.len(), 15, "across 15 files");

    // Imports are a different kind of reference from calls. `Preconditions` is
    // imported, but `checkNotNull` itself is called, never imported.
    assert!(references.iter().all(|r| !r.is_import), "call sites should not be marked as imports");
}

/// The contrast case to `checkNotNull`: small enough to return whole.
///
/// Also the case that corrected `EXPECTATIONS.md`. `Clock.nowMillis` is
/// mentioned four times — declared, overridden, and called twice — and the
/// override is a *definition* of `SystemClock#nowMillis`, not a reference. Only
/// the two call sites are references.
#[test]
fn an_override_is_a_definition_not_a_reference() {
    let Some(index) = index() else { return };

    let interface_method = index
        .search("nowMillis")
        .into_iter()
        .find(|d| d.symbol.contains("core/Clock#"))
        .expect("Clock.nowMillis should be indexed");
    assert_eq!(index.references(&interface_method.symbol).len(), 2, "two call sites");

    // The override is indexed separately, as its own definition.
    let override_def = index
        .search("nowMillis")
        .into_iter()
        .find(|d| d.symbol.contains("core/SystemClock#"))
        .expect("the override should be its own definition");
    assert_ne!(override_def.symbol, interface_method.symbol);
}

/// The non-ASCII identifier. `EXPECTATIONS.md` keeps it because symbol indexing
/// has to survive one, and Error Prone rejects them by default.
#[test]
fn the_non_ascii_identifier_is_searchable() {
    let Some(index) = index() else { return };
    let hits = index.search("grüße");
    assert_eq!(hits.len(), 1, "grüße should be findable by its own name");
    assert!(hits[0].path.ends_with("i18n/Messages.java"));

    // And by a fragment, since search is substring-based.
    assert_eq!(index.search("GRÜSSE").len(), 0, "ß does not case-fold to SS here");
    assert_eq!(index.search("grüß").len(), 1);
}

/// The generated source exists only under `bazel-out`. A filesystem walk of the
/// source tree never finds it; only the build graph does.
#[test]
fn the_generated_source_is_indexed() {
    let Some(index) = index() else { return };
    let hits = index.search("BuildVersion");
    assert!(!hits.is_empty(), "the genrule-produced class should be indexed");
    assert_eq!(hits[0].kind, SymbolKind::Class);
}

/// JDK types resolve, which is what dissolves the jimage problem: the indexer
/// runs inside javac, so no reader for `$JAVA_HOME/lib/modules` is needed.
#[test]
fn jdk_symbols_resolve_without_a_jimage_reader() {
    let Some(index) = index() else { return };
    // `java.util.Optional` is used by `PolicyRegistry`.
    let optional =
        index.search("Optional").into_iter().find(|d| d.symbol.contains("java/util/Optional"));
    // The JDK is referenced, not defined, by this workspace — so it appears in
    // references even when no shard defines it.
    let referenced =
        index.references("semanticdb maven jdk 26 java/util/Optional#").is_empty().not_then(|| ());
    assert!(
        optional.is_some() || referenced.is_some(),
        "JDK symbols should be reachable, as definitions or references"
    );
}

/// Every range must be usable. A column past the end of its line, or a start
/// after its end, would produce a nonsense LSP location.
#[test]
fn all_ranges_are_well_formed() {
    let Some(index) = index() else { return };
    for name in ["RetryPolicy", "checkNotNull", "grüße", "BuildVersion"] {
        for def in index.search(name) {
            let r = def.range;
            assert!(
                r.start_line < r.end_line
                    || (r.start_line == r.end_line && r.start_col <= r.end_col),
                "{} has an inverted range: {r:?}",
                def.name
            );
        }
    }
}

/// The workspace the shards were built from, for tests that need source text.
///
/// `JABAR_FIXTURE_DIR` is optional: the position tests skip without it, since
/// they need to locate an identifier in the file to know where to put a cursor.
fn fixture_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("JABAR_FIXTURE_DIR").map(std::path::PathBuf::from)
}

/// Finds `(line, utf16_col)` of `needle` on the first line containing `marker`.
fn cursor_on(relative: &str, marker: &str, needle: &str) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(fixture_dir()?.join(relative)).ok()?;
    let (line_no, line) = text.lines().enumerate().find(|(_, l)| l.contains(marker))?;
    let byte_col = line.find(needle)?;
    // The index stores UTF-16 columns, so count code units, not bytes.
    let utf16_col = line[..byte_col].encode_utf16().count();
    // Land inside the identifier rather than on its first character, so the
    // test also proves the range is treated as a span.
    Some((line_no as u32, (utf16_col + needle.len().min(2)) as u32))
}

/// The lookup behind `goToDefinition`: a cursor resolves to a symbol.
#[test]
fn a_cursor_inside_an_identifier_resolves_to_its_symbol() {
    let Some(index) = index() else { return };
    let Some((line, col)) = cursor_on(
        "java/com/acme/policy/DefaultRetryPolicy.java",
        "class DefaultRetryPolicy",
        "DefaultRetryPolicy",
    ) else {
        return;
    };

    let symbol = index
        .symbol_at("java/com/acme/policy/DefaultRetryPolicy.java", line, col)
        .expect("the cursor is inside the class name");
    assert!(symbol.contains("DefaultRetryPolicy#"), "resolved to {symbol}");

    // And that symbol has a definition, which is what goToDefinition returns.
    let def = index.definition(symbol).expect("the symbol should be defined");
    assert_eq!(def.name, "DefaultRetryPolicy");
    assert_eq!(def.range.start_line, line, "the definition is on the same line");
}

/// A cursor in whitespace resolves to nothing, rather than to whatever is near.
#[test]
fn a_cursor_outside_any_identifier_resolves_to_nothing() {
    let Some(index) = index() else { return };
    if fixture_dir().is_none() {
        return;
    }
    // Column 0 of a class declaration line is indentation.
    let path = "java/com/acme/policy/DefaultRetryPolicy.java";
    let Some((line, _)) = cursor_on(path, "class DefaultRetryPolicy", "class") else { return };
    assert_eq!(index.symbol_at(path, line, 0), None, "indentation is not a symbol");
}

/// The lookup behind `findReferences`, end to end from a cursor.
#[test]
fn a_cursor_on_a_call_site_finds_every_reference() {
    let Some(index) = index() else { return };
    // A `checkNotNull(` call inside ExponentialBackoff.
    let path = "java/com/acme/backoff/ExponentialBackoff.java";
    let Some((line, col)) = cursor_on(path, "checkNotNull(base", "checkNotNull") else { return };

    let symbol = index.symbol_at(path, line, col).expect("cursor is on the call");
    assert!(symbol.contains("checkNotNull"), "resolved to {symbol}");
    // EXPECTATIONS.md: 30 call sites. Reached from a cursor rather than by
    // searching for the name, which is the path a client actually takes.
    assert_eq!(index.references(symbol).len(), 30);
}

/// `documentSymbol` reads one file's declarations, in source order.
#[test]
fn a_files_declarations_are_listed_in_source_order() {
    let Some(index) = index() else { return };
    let defs = index.definitions_in("java/com/acme/policy/PolicyRegistry.java");
    assert!(!defs.is_empty(), "the file should contribute definitions");

    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"PolicyRegistry"), "the class itself: {names:?}");
    assert!(names.contains(&"register"), "its methods: {names:?}");

    // Source order, so a caller can nest by enclosing span in one pass.
    let lines: Vec<u32> = defs.iter().map(|d| d.range.start_line).collect();
    assert!(lines.windows(2).all(|w| w[0] <= w[1]), "not in source order: {lines:?}");

    // The class encloses its members, which is what makes nesting possible.
    let class = defs.iter().find(|d| d.name == "PolicyRegistry").expect("the class");
    let method = defs.iter().find(|d| d.name == "register").expect("a method");
    let span = class.enclosing.expect("a class declaration has a span");
    assert!(
        span.start_line <= method.range.start_line && method.range.end_line <= span.end_line,
        "the class span {span:?} should contain the method at {:?}",
        method.range
    );
}

/// `hover` needs a signature and javadoc; both come from the indexer.
#[test]
fn definitions_carry_a_signature_and_documentation() {
    let Some(index) = index() else { return };
    let def = index
        .search("RetryPolicy")
        .into_iter()
        .find(|d| d.name == "RetryPolicy")
        .expect("the interface");

    assert!(def.signature.contains("RetryPolicy"), "signature: {:?}", def.signature);
    assert!(
        def.documentation.iter().any(|d| d.contains("failed attempt")),
        "the javadoc should survive: {:?}",
        def.documentation
    );
}

/// The lookup behind `incomingCalls`: a reference attributes to the method
/// containing it.
///
/// `EXPECTATIONS.md`: `RetryingHttpClient.send` calls `Backoff.nextDelay`.
#[test]
fn a_reference_attributes_to_its_enclosing_method() {
    let Some(index) = index() else { return };

    let next_delay = index
        .search("nextDelay")
        .into_iter()
        .find(|d| d.symbol.contains("core/Backoff#"))
        .expect("Backoff.nextDelay should be indexed");

    let callers: Vec<String> = index
        .references(&next_delay.symbol)
        .iter()
        .filter_map(|r| index.enclosing_callable(&r.path, r.range))
        .map(|d| d.name.clone())
        .collect();

    assert!(callers.contains(&"send".to_owned()), "callers: {callers:?}");
}

/// The lookup behind `outgoingCalls`: what a method's body mentions.
#[test]
fn a_method_body_yields_what_it_calls() {
    let Some(index) = index() else { return };

    let send = index
        .search("send")
        .into_iter()
        .find(|d| d.symbol.contains("RetryingHttpClient#send"))
        .expect("RetryingHttpClient.send should be indexed");
    let span = send.enclosing.expect("a method has a declaration span");

    let called: Vec<String> = index
        .references_within(&send.path, span)
        .into_iter()
        .filter_map(|(symbol, _)| index.definition(symbol))
        .map(|d| d.name.clone())
        .collect();

    // Per EXPECTATIONS.md this crosses three target boundaries.
    assert!(called.contains(&"nextDelay".to_owned()), "called: {called:?}");
    assert!(called.contains(&"shouldRetry".to_owned()), "called: {called:?}");
}

/// A helper the JDK test wants and `Option` does not have.
trait NotThen {
    fn not_then<T>(self, f: impl FnOnce() -> T) -> Option<T>;
}

impl NotThen for bool {
    fn not_then<T>(self, f: impl FnOnce() -> T) -> Option<T> {
        (!self).then(f)
    }
}
