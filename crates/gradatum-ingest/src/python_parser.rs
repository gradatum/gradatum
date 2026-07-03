//! Tree-sitter parser for Python files (feature `code-python`).
//!
//! ## Extracted entities
//!
//! - Top-level functions (`function_definition`) → `DerivedSymbol` with `kind = "fn"`
//! - Top-level classes (`class_definition`) → `DerivedSymbol` with `kind = "class"`
//! - Methods (functions inside the `block` child of a class) → `kind = "method"`,
//!   `qualified_name = "ClassName::method_name"`
//!
//! ## Python visibility
//!
//! Python has no syntactic visibility modifier. Convention:
//! - Name starting with `_` (but not `__dunder__`) → `"priv"`
//! - Dunder names (`__init__`, `__str__`, etc.) → `"pub"` (public protocol API)
//! - Any other name → `"pub"`
//!
//! ## Not extracted (accuracy > coverage)
//!
//! - Module-level assignments (`CONSTANT = 42`): no suitable kind in `DerivedSymbol`
//!   (force-fitting as `"const"` would be misleading in Python — intentionally skipped)
//! - Functions/classes inside conditional blocks (`if __name__ == "__main__":`)
//! - Decorated definitions: decorators are read, but the inner entity is extracted normally
//! - Deps (call graph): `deps: vec![]` — call graph extraction not yet implemented
//!
//! ## Docstrings
//!
//! The first `expression_statement` containing a `string` inside the `block` of a
//! function or class is treated as the docstring (PEP 257).
//! The module-level docstring (first statement of the module) is ignored (not associated
//! with an extractable symbol).

use tree_sitter::Node;

use crate::DerivedSymbol;

/// Returns the UTF-8 text of a tree-sitter node.
///
/// ## Safety invariant
///
/// `source` must be the SAME buffer passed to `parser.parse(content, None)`.
/// The AST byte offsets are guaranteed to lie within this slice — `utf8_text` cannot
/// index out of bounds. `.unwrap_or("")` is defensive but should never trigger.
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Determines the visibility of a Python symbol according to the `_`-prefix convention.
///
/// Rules:
/// - `__dunder__` (double underscores on both sides) → `"pub"` (public Python protocol)
/// - `_single_prefix` → `"priv"` (private by convention)
/// - Any other name → `"pub"`
fn python_visibility(name: &str) -> &'static str {
    if name.starts_with("__") && name.ends_with("__") {
        // Dunder methods (__init__, __str__, etc.) = API publique du protocole Python.
        "pub"
    } else if name.starts_with('_') {
        "priv"
    } else {
        "pub"
    }
}

/// Computes the 1-based inclusive span `(start_line, end_line)` of a node.
///
/// Mirrors the same function in `rust_parser.rs`:
/// - Lines are 1-based: `row + 1` (tree-sitter is 0-based).
/// - If `end_position().column == 0` and `row > 0` → exclude the trailing blank line.
/// - Degenerate span (`start > end`) → `None`.
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

/// Extracts the docstring from a Python block (first `expression_statement` containing a `string`).
///
/// Per PEP 257, the docstring is the first statement of the body if it is a string literal.
/// Output is capped at 5 lines (consistent with rust_parser.rs).
fn extract_docstring(block_node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = block_node.walk();
    // La docstring doit être le PREMIER statement du bloc.
    // Si ce premier enfant n'est pas un expression_statement, pas de docstring.
    let first_child = block_node.children(&mut cursor).next()?;
    if first_child.kind() != "expression_statement" {
        return None;
    }
    // Chercher un nœud `string` direct dans l'expression_statement.
    let mut ec = first_child.walk();
    for expr_child in first_child.children(&mut ec) {
        if expr_child.kind() == "string" {
            // Extraire le contenu textuel de la docstring.
            // La structure : string_start + string_content(s) + string_end.
            let mut content_parts: Vec<String> = Vec::new();
            let mut sc = expr_child.walk();
            for string_child in expr_child.children(&mut sc) {
                if string_child.kind() == "string_content" {
                    content_parts.push(node_text(string_child, source).to_string());
                }
            }
            if !content_parts.is_empty() {
                // Joindre les parties et limiter à 5 lignes.
                let full = content_parts.join("");
                let truncated: String = full.lines().take(5).collect::<Vec<_>>().join("\n");
                return Some(truncated);
            }
            // Fallback : si pas de string_content, utiliser le texte brut sans les guillemets.
            let raw = node_text(expr_child, source);
            let stripped = raw
                .trim_start_matches("\"\"\"")
                .trim_start_matches("'''")
                .trim_start_matches('"')
                .trim_start_matches('\'')
                .trim_end_matches("\"\"\"")
                .trim_end_matches("'''")
                .trim_end_matches('"')
                .trim_end_matches('\'')
                .trim();
            if !stripped.is_empty() {
                return Some(stripped.lines().take(5).collect::<Vec<_>>().join("\n"));
            }
        }
    }
    None
}

/// Extracts the textual signature of a Python function (parameters + return type).
///
/// Extracts the raw text of the `parameters` node with a char-safe truncation at 120 bytes.
/// If a return type annotation is present (after `->`) it is appended.
fn extract_fn_signature(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut params_text: Option<String> = None;
    let mut return_type: Option<String> = None;
    let mut after_arrow = false;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "parameters" => {
                let raw = node_text(child, source);
                let truncated = if raw.len() > 120 {
                    // Troncature char-safe (même pattern que rust_parser.rs).
                    let boundary = raw
                        .char_indices()
                        .find(|(i, _)| *i >= 120)
                        .map(|(i, _)| i)
                        .unwrap_or(raw.len());
                    format!("{}…", &raw[..boundary])
                } else {
                    raw.to_string()
                };
                params_text = Some(truncated);
            }
            "->" => {
                after_arrow = true;
            }
            "type" if after_arrow => {
                return_type = Some(node_text(child, source).to_string());
                after_arrow = false;
            }
            _ => {}
        }
    }

    match (params_text, return_type) {
        (Some(p), Some(r)) => Some(format!("{p} -> {r}")),
        (Some(p), None) => Some(p),
        _ => None,
    }
}

/// Extracts a top-level function or a method.
///
/// # Parameters
///
/// - `class_name`: `Some("ClassName")` if this function is a method of a class.
/// - `include_private`: when `false`, items with a `_`-prefix are excluded.
fn extract_function(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    class_name: Option<&str>,
    include_private: bool,
) -> Option<DerivedSymbol> {
    // Trouver l'identifier (nom de la fonction).
    let name = {
        let mut cursor = node.walk();
        let mut found: Option<&str> = None;
        for child in node.children(&mut cursor) {
            if child.kind() == "identifier" {
                found = Some(node_text(child, source));
                break;
            }
        }
        found?
    };

    let visibility = python_visibility(name);

    // Appliquer le filtre de visibilité.
    if !include_private && visibility == "priv" {
        return None;
    }

    let qualified_name = match class_name {
        Some(cls) => format!("{cls}::{name}"),
        None => name.to_string(),
    };

    let kind = if class_name.is_some() { "method" } else { "fn" };
    let signature = extract_fn_signature(node, source);

    // Extraire la docstring depuis le block.
    let doc_comment = {
        let mut cursor = node.walk();
        let mut doc: Option<String> = None;
        for child in node.children(&mut cursor) {
            if child.kind() == "block" {
                doc = extract_docstring(child, source);
                break;
            }
        }
        doc
    };

    let span = extract_node_span(node);

    Some(DerivedSymbol {
        qualified_name,
        kind: kind.to_string(),
        signature,
        doc_comment,
        // TODO F-61 inc2 : extraction call_expression Python (call-graph deps différée).
        deps: Vec::new(),
        source_path: source_path.to_string(),
        visibility: visibility.to_string(),
        span,
        ambiguous: false,
    })
}

/// Extracts the methods of a class (functions inside the body block).
fn extract_class_methods(
    class_name: &str,
    block_node: Node<'_>,
    source: &[u8],
    source_path: &str,
    include_private: bool,
    symbols: &mut Vec<DerivedSymbol>,
) {
    let mut cursor = block_node.walk();
    for child in block_node.children(&mut cursor) {
        if child.kind() == "function_definition"
            && let Some(sym) = extract_function(
                child,
                source,
                source_path,
                Some(class_name),
                include_private,
            )
        {
            symbols.push(sym);
        }
        // decorated_definition : `@decorator\ndef ...` — unwrap le inner.
        if child.kind() == "decorated_definition" {
            let mut dc = child.walk();
            for inner in child.children(&mut dc) {
                if inner.kind() == "function_definition"
                    && let Some(sym) = extract_function(
                        inner,
                        source,
                        source_path,
                        Some(class_name),
                        include_private,
                    )
                {
                    symbols.push(sym);
                }
            }
        }
    }
}

/// Extracts a top-level class and its methods.
fn extract_class(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    include_private: bool,
    symbols: &mut Vec<DerivedSymbol>,
) {
    // Trouver le nom de la classe (premier identifier).
    let name = {
        let mut cursor = node.walk();
        let mut found: Option<&str> = None;
        for child in node.children(&mut cursor) {
            if child.kind() == "identifier" {
                found = Some(node_text(child, source));
                break;
            }
        }
        match found {
            Some(n) => n,
            None => return, // classe sans nom → ignorer
        }
    };

    let visibility = python_visibility(name);

    // Appliquer le filtre de visibilité.
    if !include_private && visibility == "priv" {
        return;
    }

    // Extraire la docstring et le bloc.
    let mut doc_comment: Option<String> = None;
    let mut block_node: Option<Node<'_>> = None;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "block" {
            doc_comment = extract_docstring(child, source);
            block_node = Some(child);
            break;
        }
    }

    let span = extract_node_span(node);

    symbols.push(DerivedSymbol {
        qualified_name: name.to_string(),
        kind: "class".to_string(),
        signature: None,
        doc_comment,
        deps: Vec::new(),
        source_path: source_path.to_string(),
        visibility: visibility.to_string(),
        span,
        ambiguous: false,
    });

    // Extraire les méthodes si le bloc existe.
    if let Some(block) = block_node {
        extract_class_methods(name, block, source, source_path, include_private, symbols);
    }
}

/// Extracts top-level items from a Python module.
///
/// Iterates over the direct children of the `module` node and dispatches by kind.
fn extract_module_items(
    module_node: Node<'_>,
    source: &[u8],
    source_path: &str,
    include_private: bool,
    symbols: &mut Vec<DerivedSymbol>,
) {
    let mut cursor = module_node.walk();
    for child in module_node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                if let Some(sym) =
                    extract_function(child, source, source_path, None, include_private)
                {
                    symbols.push(sym);
                }
            }
            "class_definition" => {
                extract_class(child, source, source_path, include_private, symbols);
            }
            "decorated_definition" => {
                // `@decorator\ndef ...` ou `@decorator\nclass ...` — unwrap le inner.
                let mut dc = child.walk();
                for inner in child.children(&mut dc) {
                    match inner.kind() {
                        "function_definition" => {
                            if let Some(sym) =
                                extract_function(inner, source, source_path, None, include_private)
                            {
                                symbols.push(sym);
                            }
                        }
                        "class_definition" => {
                            extract_class(inner, source, source_path, include_private, symbols);
                        }
                        _ => {}
                    }
                }
            }
            // expression_statement (assignments, docstrings module-level),
            // import_statement, if_statement, etc. → ignorés (accuracy > coverage).
            _ => {}
        }
    }
}

/// [`crate::language_parser::LanguageParser`] implementation for Python (tree-sitter-python).
///
/// Encapsulates Python grammar knowledge: node kinds, symbol extraction,
/// and the `_`-prefix visibility convention.
pub(crate) struct PythonParser {
    /// When `true`, private items (names starting with `_`) are included in the output.
    pub(crate) include_private: bool,
}

impl crate::language_parser::LanguageParser for PythonParser {
    fn ts_language(&self) -> tree_sitter::Language {
        tree_sitter_python::LANGUAGE.into()
    }

    fn extract_symbols(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        source_path: &str,
    ) -> Vec<DerivedSymbol> {
        let root = tree.root_node();
        let mut symbols = Vec::new();
        extract_module_items(
            root,
            source,
            source_path,
            self.include_private,
            &mut symbols,
        );
        symbols
    }
}
