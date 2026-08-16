//! Declarations parsed from source, for files the index cannot see yet.
//!
//! The SCIP index is a build output, so it describes the tree as it was when
//! the aspect last ran. Everything an agent has just written is missing from it.
//! That gap is the one place the server would otherwise say something untrue: a
//! query for a class written thirty seconds ago finds nothing, and "nothing
//! found" is indistinguishable from "no such class".
//!
//! This closes it for the operations that only need declarations —
//! `documentSymbol`, and a symbol's name and position. It deliberately does not
//! attempt more.
//!
//! # What it cannot do
//!
//! tree-sitter parses; it does not resolve. So an overlay symbol has a name and
//! a range and nothing else: no supertypes, no references, no idea whether two
//! `Foo`s in different files are the same `Foo`. Those need a compiler, which is
//! what the index has and this does not.
//!
//! The symbols it produces are therefore marked distinctly (see
//! [`OVERLAY_PREFIX`]) so nothing downstream joins them to indexed symbols by
//! accident. An overlay `Foo` and an indexed `Foo` are different values, and
//! treating them as one would produce exactly the confident-wrong-answer this
//! crate exists to prevent.
//!
//! # Positions
//!
//! tree-sitter counts columns in bytes, so definitions are emitted as
//! [`PositionEncoding::Utf8`]. The index emits UTF-16. Both flow through the
//! same conversion on the way out, which is why that field exists per
//! definition rather than per index.

use symbol_index::{Definition, PositionEncoding, Range, SymbolKind};
use tree_sitter::{Node, Parser};

/// Marks a symbol as parsed rather than indexed.
///
/// SCIP symbols begin with a scheme like `semanticdb maven`. This prefix cannot
/// collide with one, and makes an overlay symbol obvious in a log or a test.
pub const OVERLAY_PREFIX: &str = "jabar-overlay";

/// Parses Java source into declarations.
///
/// Holds a parser because constructing one loads the grammar, which is wasted
/// work per file.
pub struct Overlay {
    parser: Parser,
}

impl Default for Overlay {
    fn default() -> Overlay {
        Overlay::new()
    }
}

impl Overlay {
    pub fn new() -> Overlay {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("the bundled Java grammar is compatible with the bundled tree-sitter");
        Overlay { parser }
    }

    /// Declarations in `text`, in source order.
    ///
    /// `path` is workspace-relative and used both to build symbol names and to
    /// populate [`Definition::path`].
    ///
    /// Never fails. tree-sitter recovers from syntax errors, which is the whole
    /// reason it is here: a file mid-edit is the normal case, and returning
    /// nothing for it would put us back where we started.
    pub fn parse(&mut self, path: &str, text: &str) -> Vec<Definition> {
        let Some(tree) = self.parser.parse(text, None) else {
            // Documented as only happening on timeout or cancellation, neither
            // of which is set here.
            tracing::warn!(path, "tree-sitter returned no tree");
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut scope: Vec<String> = Vec::new();
        collect(tree.root_node(), text.as_bytes(), path, &mut scope, &mut out);
        out
    }
}

/// Walks the tree, emitting a definition per declaration.
///
/// `scope` is the enclosing type names, so a nested class contributes
/// `Outer.Nested` rather than a bare `Nested` that collides across files.
fn collect(
    node: Node<'_>,
    src: &[u8],
    path: &str,
    scope: &mut Vec<String>,
    out: &mut Vec<Definition>,
) {
    let kind = node.kind();

    // A field's name lives on a nested `variable_declarator`, not on the
    // `field_declaration` itself, and one declaration can bind several names.
    if kind == "field_declaration" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_declarator"
                && let Some(def) = definition(child, src, path, scope, SymbolKind::Field, node)
            {
                out.push(def);
            }
        }
        return;
    }

    let symbol_kind = match kind {
        "class_declaration" => Some(SymbolKind::Class),
        "interface_declaration" => Some(SymbolKind::Interface),
        "enum_declaration" => Some(SymbolKind::Enum),
        // A record is a class as far as any client is concerned.
        "record_declaration" => Some(SymbolKind::Class),
        "method_declaration" => Some(SymbolKind::Method),
        "constructor_declaration" => Some(SymbolKind::Constructor),
        "enum_constant" => Some(SymbolKind::Field),
        _ => None,
    };

    let mut pushed_scope = false;
    if let Some(symbol_kind) = symbol_kind
        && let Some(def) = definition(node, src, path, scope, symbol_kind, node)
    {
        // Only types introduce a scope; a method's locals are not indexed.
        if matches!(symbol_kind, SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum) {
            scope.push(def.name.clone());
            pushed_scope = true;
        }
        out.push(def);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(child, src, path, scope, out);
    }
    if pushed_scope {
        scope.pop();
    }
}

/// Builds a definition from a named node.
///
/// `declaration` is the node whose span becomes [`Definition::enclosing`] — for
/// a field that is the whole `field_declaration`, not the single declarator.
fn definition(
    node: Node<'_>,
    src: &[u8],
    path: &str,
    scope: &[String],
    kind: SymbolKind,
    declaration: Node<'_>,
) -> Option<Definition> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(src).ok()?.to_owned();
    if name.is_empty() {
        return None;
    }

    let qualified =
        if scope.is_empty() { name.clone() } else { format!("{}.{}", scope.join("."), name) };

    Some(Definition {
        symbol: format!("{OVERLAY_PREFIX} {path}#{qualified}"),
        name,
        kind,
        path: path.to_owned(),
        range: to_range(name_node),
        // tree-sitter counts columns in bytes.
        encoding: PositionEncoding::Utf8,
        // Nothing below is knowable without resolution.
        implements: Vec::new(),
        documentation: Vec::new(),
        signature: String::new(),
        enclosing: Some(to_range(declaration)),
    })
}

fn to_range(node: Node<'_>) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range {
        start_line: start.row as u32,
        start_col: start.column as u32,
        end_line: end.row as u32,
        end_col: end.column as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Vec<Definition> {
        Overlay::new().parse("java/com/acme/A.java", source)
    }

    fn names(defs: &[Definition]) -> Vec<&str> {
        defs.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn declarations_come_back_in_source_order() {
        let defs = parse(
            "package com.acme;\n\
             public class Outer {\n\
               private int count;\n\
               public Outer() {}\n\
               public void run() {}\n\
             }\n",
        );
        assert_eq!(names(&defs), ["Outer", "count", "Outer", "run"]);
        let lines: Vec<u32> = defs.iter().map(|d| d.range.start_line).collect();
        assert!(lines.windows(2).all(|w| w[0] <= w[1]), "not in source order: {lines:?}");
    }

    #[test]
    fn kinds_are_distinguished() {
        let defs = parse(
            "class C {}\n\
             interface I {}\n\
             enum E { A }\n\
             record R(int x) {}\n",
        );
        let kinds: Vec<SymbolKind> = defs.iter().map(|d| d.kind).collect();
        assert_eq!(kinds[0], SymbolKind::Class);
        assert_eq!(kinds[1], SymbolKind::Interface);
        assert_eq!(kinds[2], SymbolKind::Enum);
        // An enum constant is a member, and a record is a class to any client.
        assert!(kinds.contains(&SymbolKind::Field));
        assert_eq!(*kinds.last().unwrap(), SymbolKind::Class);
    }

    #[test]
    fn nested_types_are_qualified_but_keep_their_short_name() {
        let defs = parse("class Outer {\n  static class Nested {\n    void inner() {}\n  }\n}\n");
        let nested = defs.iter().find(|d| d.name == "Nested").expect("the nested class");
        // The short name is what a client searches for.
        assert_eq!(nested.name, "Nested");
        // The symbol carries the scope, so two files' `Nested` do not collide.
        assert!(nested.symbol.ends_with("#Outer.Nested"), "symbol: {}", nested.symbol);

        let inner = defs.iter().find(|d| d.name == "inner").expect("the method");
        assert!(inner.symbol.ends_with("#Outer.Nested.inner"), "symbol: {}", inner.symbol);
    }

    #[test]
    fn one_field_declaration_can_bind_several_names() {
        let defs = parse("class C { private int a, b, c; }\n");
        assert_eq!(names(&defs), ["C", "a", "b", "c"]);
    }

    #[test]
    fn a_half_written_file_still_yields_what_it_can() {
        // The reason tree-sitter is here rather than a stricter parser: a file
        // being edited is the normal case, and returning nothing for it puts us
        // back to answering "no such symbol".
        //
        // Recovery is partial, and this pins how partial. Declarations before
        // the error survive, including the broken one's own name. What follows
        // a malformed signature does not: the open paren swallows the rest of
        // the class body into an error node. So the overlay degrades toward
        // fewer symbols, never toward wrong ones -- which is the direction that
        // matters, since a missing symbol is a worse answer only than a correct
        // one, while a fabricated symbol is worse than nothing.
        let defs = parse(
            "public class Broken {\n\
               public void done() {}\n\
               public void half( {\n\
               public void after() {}\n",
        );
        let found = names(&defs);
        assert!(found.contains(&"Broken"), "the class survives: {found:?}");
        assert!(found.contains(&"done"), "declarations before the error survive: {found:?}");
        assert!(found.contains(&"half"), "including the broken one's name: {found:?}");
        assert!(
            !found.contains(&"after"),
            "recovery does not reach past a malformed signature; if this starts \
             passing, tree-sitter improved and the docs above should say so: {found:?}"
        );
    }

    #[test]
    fn an_unclosed_brace_does_not_lose_the_file() {
        // The commoner shape of "mid-edit": a body not yet closed. Everything
        // declared so far is still found.
        let defs = parse("class C {\n  void a() {}\n  void b() {\n");
        let found = names(&defs);
        assert!(found.contains(&"C") && found.contains(&"a") && found.contains(&"b"), "{found:?}");
    }

    #[test]
    fn an_empty_or_nonsense_file_is_not_an_error() {
        assert!(parse("").is_empty());
        assert!(parse("\u{0}\u{1}not java at all").is_empty());
    }

    #[test]
    fn columns_are_bytes_and_the_definition_says_so() {
        // The whole reason `encoding` is per definition. tree-sitter counts
        // bytes; the index counts UTF-16 units. Both convert on the way out.
        let source = "class Grüße { void après() {} }\n";
        let defs = parse(source);
        let method = defs.iter().find(|d| d.name == "après").expect("the method");
        assert_eq!(method.encoding, PositionEncoding::Utf8);

        let line = source.lines().next().unwrap();
        let start = method.range.start_col as usize;
        assert_eq!(&line[start..start + "après".len()], "après", "byte columns, not UTF-16");

        // And the two genuinely disagree on this line, which is what makes the
        // per-definition encoding field load-bearing rather than decorative:
        // `Grüße` costs two extra bytes before this column.
        let utf16_col = line[..start].encode_utf16().count();
        assert_eq!((start, utf16_col), (21, 19));
    }

    #[test]
    fn overlay_symbols_cannot_be_mistaken_for_indexed_ones() {
        // Joining an overlay symbol to an indexed one would fabricate a
        // relationship no compiler established.
        let defs = parse("class C {}\n");
        assert!(defs[0].symbol.starts_with(OVERLAY_PREFIX));
        assert!(!defs[0].symbol.starts_with("semanticdb"));
    }

    #[test]
    fn nothing_unresolvable_is_invented() {
        // tree-sitter parses; it does not resolve. Claiming otherwise is how a
        // plausible wrong answer gets made.
        let defs = parse("class C implements Runnable {\n  /** Docs. */ public void run() {}\n}\n");
        let class = &defs[0];
        assert!(class.implements.is_empty(), "supertypes need resolution");
        assert!(class.documentation.is_empty(), "javadoc is not extracted");
        assert!(class.signature.is_empty(), "signatures need types");
    }

    #[test]
    fn the_declaration_span_contains_the_name() {
        // `documentSymbol` nests by this, and LSP requires the outer range to
        // contain the selection range.
        let defs = parse("class Outer {\n  void method() {\n    int x = 1;\n  }\n}\n");
        for def in &defs {
            let span = def.enclosing.expect("every declaration has a span");
            assert!(
                (span.start_line, span.start_col) <= (def.range.start_line, def.range.start_col),
                "{} span starts after its name",
                def.name
            );
            assert!(
                (def.range.end_line, def.range.end_col) <= (span.end_line, span.end_col),
                "{} name ends after its span",
                def.name
            );
        }
    }
}
