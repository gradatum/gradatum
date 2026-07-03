//! Tree-sitter parser for Bash files (feature `code-bash`).
//!
//! ## Extracted entities
//!
//! - Functions (`function_definition`) → `DerivedSymbol` with `kind = "fn"`.
//!   Both Bash forms are identical in the tree-sitter-bash 0.25.1 AST:
//!   `foo() { ... }` and `function bar { ... }` both produce a
//!   `function_definition` node with a named field `name` of type `word`.
//! - Top-level assignments (`variable_assignment`) → `kind = "const"` (best-effort:
//!   Bash has no constant concept, but this is the closest available kind).
//!
//! ## Visibility
//!
//! Bash has no syntactic visibility modifier. Everything is `"pub"`.
//!
//! ## Signature
//!
//! Bash does not declare typed parameters — they are positional (`$1`, `$2`...).
//! `signature = None` by design (accuracy > coverage: a fabricated signature would be misleading).
//!
//! ## Doc-comments
//!
//! `comment` nodes (lines starting with `#`) that immediately precede a `function_definition`
//! as siblings of the parent node are extracted as the doc-comment.
//! Capped at 5 lines, consistent with the other parsers.
//!
//! ## Deps (call graph)
//!
//! `deps = vec![]` — extraction of callees (`command_name` nodes under `command`) is deferred.
//! The AST structure would allow extraction, but accuracy > coverage means uncertain
//! dependencies (Bash aliases, shell builtins, etc.) are intentionally omitted.
//!
//! ## Not extracted (accuracy > coverage)
//!
//! - Assignments inside sub-blocks (inside a function): only the root `program` level.
//! - Dynamically defined functions (via `eval`, `export -f`): invisible to tree-sitter.
//! - Functions imported via `source` or `.`: ignored.

use tree_sitter::Node;

use crate::DerivedSymbol;

/// Returns the UTF-8 text of a tree-sitter node.
///
/// ## Safety invariant
///
/// `source` must be the SAME buffer passed to `parser.parse(content, None)`.
/// The AST byte offsets are guaranteed to lie within this slice.
/// `.unwrap_or("")` is defensive but should never trigger.
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Computes the 1-based inclusive span `(start_line, end_line)` of a node.
///
/// Mirrors the same function in `python_parser.rs` and `rust_parser.rs`.
fn extract_node_span(node: Node<'_>) -> Option<(u32, u32)> {
    let start_line = (node.start_position().row as u32).saturating_add(1);
    let end_line = if node.end_position().column == 0 && node.end_position().row > 0 {
        node.end_position().row as u32
    } else {
        (node.end_position().row as u32).saturating_add(1)
    };

    if start_line == 0 || start_line > end_line {
        return None;
    }
    Some((start_line, end_line))
}

/// Extracts comment lines that immediately precede the target node.
///
/// Walks the node's previous siblings in its immediate parent.
/// Collects only contiguous `comment` nodes (stops as soon as a non-comment
/// sibling is encountered when going backwards). Capped at 5 lines.
fn extract_preceding_comments(node: Node<'_>, source: &[u8]) -> Option<String> {
    // Collecter les siblings précédents dans l'ordre inverse.
    let parent = node.parent()?;
    let target_id = node.id();

    // Accumuler tous les siblings de ce parent, trouver notre nœud, puis remonter.
    let mut cursor = parent.walk();
    let children: Vec<Node<'_>> = parent.children(&mut cursor).collect();

    // Trouver la position de notre nœud cible.
    let pos = children.iter().position(|c| c.id() == target_id)?;

    // Remonter les siblings en cherchant des `comment` contigus.
    let mut comments: Vec<&str> = Vec::new();
    let mut i = pos.saturating_sub(1);
    loop {
        let sibling = children[i];
        if sibling.kind() == "comment" {
            comments.push(node_text(sibling, source));
        } else {
            // Un sibling non-comment casse la contiguïté.
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }

    if comments.is_empty() {
        return None;
    }

    // Les commentaires ont été collectés en ordre inverse → inverser.
    comments.reverse();

    // Nettoyer le préfixe `#` et les espaces.
    let lines: Vec<String> = comments
        .iter()
        .take(5)
        .map(|c| c.trim_start_matches('#').trim().to_string())
        .collect();

    Some(lines.join("\n"))
}

/// Extracts a Bash `function_definition`.
///
/// In the tree-sitter-bash 0.25.1 AST, `function_definition` has a named field `name`
/// of type `word` (the function name) and a field `body` of type `compound_statement`.
/// Both syntactic forms (`foo(){}` and `function foo {}`) produce the same AST node —
/// no disambiguation is needed.
fn extract_function(node: Node<'_>, source: &[u8], source_path: &str) -> Option<DerivedSymbol> {
    // Champ nommé `name` (type `word`) — le nom de la fonction.
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);

    if name.is_empty() {
        return None;
    }

    // Doc-comment : comments précédant ce nœud dans son parent.
    let doc_comment = extract_preceding_comments(node, source);
    let span = extract_node_span(node);

    Some(DerivedSymbol {
        qualified_name: name.to_string(),
        kind: "fn".to_string(),
        // Bash = pas de signature typée (accuracy > coverage).
        signature: None,
        doc_comment,
        // Deps différés (voir module-doc).
        deps: Vec::new(),
        source_path: source_path.to_string(),
        // Bash n'a pas de modificateur de visibilité — tout est "pub".
        visibility: "pub".to_string(),
        span,
        ambiguous: false,
    })
}

/// Extracts a top-level `variable_assignment`.
///
/// In the tree-sitter-bash 0.25.1 AST, `variable_assignment` has:
/// - A named field `name` of type `variable_name`.
/// - An optional named field `value`.
///
/// Kind = `"const"` (best-effort — Bash has no syntactic constant concept,
/// but top-level assignments typically represent configuration variables).
fn extract_variable_assignment(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
) -> Option<DerivedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, source);

    if name.is_empty() {
        return None;
    }

    let span = extract_node_span(node);

    Some(DerivedSymbol {
        qualified_name: name.to_string(),
        kind: "const".to_string(),
        signature: None,
        doc_comment: None,
        deps: Vec::new(),
        source_path: source_path.to_string(),
        visibility: "pub".to_string(),
        span,
        ambiguous: false,
    })
}

/// Extracts top-level items from a Bash program.
///
/// Iterates over the direct children of the `program` node (root of the Bash AST).
fn extract_program_items(
    program_node: Node<'_>,
    source: &[u8],
    source_path: &str,
    symbols: &mut Vec<DerivedSymbol>,
) {
    let mut cursor = program_node.walk();
    for child in program_node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                if let Some(sym) = extract_function(child, source, source_path) {
                    symbols.push(sym);
                }
            }
            "variable_assignment" => {
                if let Some(sym) = extract_variable_assignment(child, source, source_path) {
                    symbols.push(sym);
                }
            }
            // comment, command, if_statement, etc. → ignorés au niveau top-level
            _ => {}
        }
    }
}

/// [`crate::language_parser::LanguageParser`] implementation for Bash (tree-sitter-bash).
///
/// Encapsulates Bash grammar knowledge: node kinds and symbol extraction.
/// No `include_private` field: Bash has no syntactic visibility modifier.
pub(crate) struct BashParser;

impl crate::language_parser::LanguageParser for BashParser {
    fn ts_language(&self) -> tree_sitter::Language {
        tree_sitter_bash::LANGUAGE.into()
    }

    fn extract_symbols(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        source_path: &str,
    ) -> Vec<DerivedSymbol> {
        let root = tree.root_node();
        let mut symbols = Vec::new();
        extract_program_items(root, source, source_path, &mut symbols);
        symbols
    }
}
