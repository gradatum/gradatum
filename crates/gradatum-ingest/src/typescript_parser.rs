//! Tree-sitter parser for TypeScript files (feature `code-typescript`).
//!
//! ## Extracted entities
//!
//! - Functions (`function_declaration`) → `DerivedSymbol` with `kind = "fn"`
//! - Classes (`class_declaration`) → `DerivedSymbol` with `kind = "class"`
//! - Interfaces (`interface_declaration`) → `DerivedSymbol` with `kind = "type"`
//! - Class methods (`method_definition`) → `kind = "method"`,
//!   `qualified_name = "ClassName::methodName"`
//! - Top-level arrow functions (`const f = () => {}`) → `kind = "fn"`
//!
//! ## Visibility
//!
//! - Presence of a parent `export_statement` → `"pub"`
//! - Absence → `"priv"` (module-local, not exported)
//! - Inside a class, `public`/`private`/`protected` modifiers are respected.
//!   Missing modifier → `"pub"` (TypeScript default visibility = public).
//!
//! ## JSDoc docstrings
//!
//! `/** ... */` blocks (comment nodes preceding the item, text starting with `/**`)
//! are extracted as the doc-comment. Capped at 5 lines.
//!
//! ## Signature
//!
//! Raw text of the `formal_parameters` node, truncated to 120 bytes (char-safe).
//! `None` for classes and interfaces.
//!
//! ## TSX variant
//!
//! Not covered: `LANGUAGE_TYPESCRIPT` does not parse JSX.
//! Use `LANGUAGE_TSX` (see [`crate::parse_tsx_file`]) for `.tsx` files.
//! This is intentional behavior, not a bug.
//!
//! ## Deps
//!
//! `deps = vec![]` for this implementation (accuracy > coverage).

use tree_sitter::Node;

use crate::DerivedSymbol;

/// Returns the UTF-8 text of a tree-sitter node.
///
/// ## Safety invariant
///
/// `source` must be the SAME buffer passed to `parser.parse(content, None)`.
/// `.unwrap_or("")` is defensive but should never trigger.
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Computes the 1-based inclusive span `(start_line, end_line)` of a node.
///
/// Mirrors the same function in `python_parser.rs` and `bash_parser.rs`.
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

/// Truncates text to 120 bytes in a char-safe manner.
///
/// Mirrors the pattern used in `rust_parser.rs` and `python_parser.rs`.
fn truncate_120(s: &str) -> String {
    if s.len() <= 120 {
        s.to_string()
    } else {
        let boundary = s
            .char_indices()
            .find(|(i, _)| *i >= 120)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}…", &s[..boundary])
    }
}

/// Extracts the JSDoc comment (`/** ... */`) that immediately precedes a node.
///
/// Walks the node's previous siblings in its parent. Collects only contiguous
/// `comment` nodes. Capped at 5 lines.
///
/// ## `export_statement` case
///
/// When an item is declared via `export_statement`, the JSDoc comment is a sibling
/// of the `export_statement`, NOT of the inner declaration. In that case the function
/// ascends to the `export_statement` level to search for siblings.
///
/// Example AST:
/// ```text
/// (program
///   (comment "/** Handler. */")   ← sibling of export_statement
///   (export_statement
///     declaration: (function_declaration ...)))
/// ```
fn extract_jsdoc(node: Node<'_>, source: &[u8]) -> Option<String> {
    // Si le parent immédiat est un export_statement, remonter au niveau du export_statement
    // pour chercher ses siblings (le comment JSDoc est au-dessus de l'export, pas de la décl).
    let (effective_node, parent) = if let Some(p) = node.parent() {
        if p.kind() == "export_statement" {
            // Remonter au grand-parent : chercher les siblings de l'export_statement.
            let grandparent = p.parent()?;
            (p, grandparent)
        } else {
            (node, p)
        }
    } else {
        return None;
    };

    let target_id = effective_node.id();

    let mut cursor = parent.walk();
    let children: Vec<Node<'_>> = parent.children(&mut cursor).collect();

    let pos = children.iter().position(|c| c.id() == target_id)?;

    // Remonter les siblings en cherchant un comment JSDoc contigu.
    let mut collected: Vec<&str> = Vec::new();
    let mut i = pos.saturating_sub(1);
    loop {
        let sibling = children[i];
        if sibling.kind() == "comment" {
            let text = node_text(sibling, source);
            collected.push(text);
        } else {
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }

    if collected.is_empty() {
        return None;
    }

    // Inverser pour ordre chronologique.
    collected.reverse();

    // Extraire le contenu des blocs JSDoc (`/** ... */`).
    let mut lines: Vec<String> = Vec::new();
    for comment_text in collected {
        let trimmed = comment_text.trim();
        if trimmed.starts_with("/**") {
            // Nettoyer le bloc JSDoc.
            let inner = trimmed
                .trim_start_matches("/**")
                .trim_end_matches("*/")
                .trim();
            for line in inner.lines().take(5) {
                let cleaned = line.trim().trim_start_matches('*').trim();
                if !cleaned.is_empty() {
                    lines.push(cleaned.to_string());
                }
            }
        } else if trimmed.starts_with("//") {
            // Commentaire ligne simple → inclure aussi.
            let cleaned = trimmed.trim_start_matches("//").trim();
            if !cleaned.is_empty() {
                lines.push(cleaned.to_string());
            }
        }
    }

    if lines.is_empty() {
        return None;
    }
    Some(lines.iter().take(5).cloned().collect::<Vec<_>>().join("\n"))
}

/// Extracts the signature from the `formal_parameters` node.
///
/// Raw text truncated to 120 bytes (char-safe). `None` if the node is not found.
fn extract_formal_parameters(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "formal_parameters" {
            let raw = node_text(child, source);
            return Some(truncate_120(raw));
        }
    }
    None
}

/// Determines the visibility of a method from its TypeScript accessibility modifiers.
///
/// Searches for `accessibility_modifier` child nodes of `method_definition`.
/// - `public` → `"pub"`
/// - `private` → `"priv"`
/// - `protected` → `"priv"` (inaccessible from outside)
/// - Absent → `"pub"` (TypeScript default = public)
fn method_visibility(node: Node<'_>, source: &[u8]) -> &'static str {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "accessibility_modifier" {
            let text = node_text(child, source);
            return match text {
                "public" => "pub",
                "private" | "protected" => "priv",
                _ => "pub",
            };
        }
    }
    // Défaut TypeScript : public.
    "pub"
}

/// Parsing context: export visibility of the current item.
struct ParseContext {
    /// `true` when the current item is under an `export_statement`.
    is_exported: bool,
}

/// Extracts items from a `class_body` (methods).
fn extract_class_methods(
    class_name: &str,
    class_body: Node<'_>,
    source: &[u8],
    source_path: &str,
    include_private: bool,
    symbols: &mut Vec<DerivedSymbol>,
) {
    let mut cursor = class_body.walk();
    for child in class_body.children(&mut cursor) {
        if child.kind() == "method_definition" {
            // Nom de la méthode : champ nommé `name` de type `property_identifier`.
            let method_name = match child.child_by_field_name("name") {
                Some(n) => node_text(n, source),
                None => continue,
            };

            let visibility = method_visibility(child, source);
            if !include_private && visibility == "priv" {
                continue;
            }

            let qualified_name = format!("{class_name}::{method_name}");
            let signature = extract_formal_parameters(child, source);
            let doc_comment = extract_jsdoc(child, source);
            let span = extract_node_span(child);

            symbols.push(DerivedSymbol {
                qualified_name,
                kind: "method".to_string(),
                signature,
                doc_comment,
                deps: Vec::new(),
                source_path: source_path.to_string(),
                visibility: visibility.to_string(),
                span,
                ambiguous: false,
            });
        }
    }
}

/// Extracts a `class_declaration` or `interface_declaration`.
fn extract_class_or_interface(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    ctx: &ParseContext,
    include_private: bool,
    symbols: &mut Vec<DerivedSymbol>,
) {
    let is_interface = node.kind() == "interface_declaration";
    let kind = if is_interface { "type" } else { "class" };

    // Nom : champ `name` de type `type_identifier`.
    let name = match node.child_by_field_name("name") {
        Some(n) => node_text(n, source).to_string(),
        None => return,
    };

    let visibility = if ctx.is_exported { "pub" } else { "priv" };
    if !include_private && visibility == "priv" {
        return;
    }

    let doc_comment = extract_jsdoc(node, source);
    let span = extract_node_span(node);

    symbols.push(DerivedSymbol {
        qualified_name: name.clone(),
        kind: kind.to_string(),
        signature: None,
        doc_comment,
        deps: Vec::new(),
        source_path: source_path.to_string(),
        visibility: visibility.to_string(),
        span,
        ambiguous: false,
    });

    // Extraire les méthodes pour les classes (pas les interfaces).
    if !is_interface {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "class_body" {
                extract_class_methods(&name, child, source, source_path, include_private, symbols);
                break;
            }
        }
    }
}

/// Extracts a `function_declaration`.
fn extract_function_declaration(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    ctx: &ParseContext,
    include_private: bool,
    symbols: &mut Vec<DerivedSymbol>,
) {
    let name = match node.child_by_field_name("name") {
        Some(n) => node_text(n, source),
        None => return,
    };

    let visibility = if ctx.is_exported { "pub" } else { "priv" };
    if !include_private && visibility == "priv" {
        return;
    }

    let signature = extract_formal_parameters(node, source);
    let doc_comment = extract_jsdoc(node, source);
    let span = extract_node_span(node);

    symbols.push(DerivedSymbol {
        qualified_name: name.to_string(),
        kind: "fn".to_string(),
        signature,
        doc_comment,
        deps: Vec::new(),
        source_path: source_path.to_string(),
        visibility: visibility.to_string(),
        span,
        ambiguous: false,
    });
}

/// Extracts a top-level `lexical_declaration` containing an `arrow_function`.
///
/// Recognized patterns:
/// - `const f = () => {}`: `lexical_declaration` → `variable_declarator` → `arrow_function`
///
/// The name is extracted from the `variable_declarator` (field `name` of type `identifier`).
fn extract_arrow_function(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    ctx: &ParseContext,
    include_private: bool,
    symbols: &mut Vec<DerivedSymbol>,
) {
    let visibility = if ctx.is_exported { "pub" } else { "priv" };
    if !include_private && visibility == "priv" {
        return;
    }

    // Parcourir les variable_declarator dans la lexical_declaration.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }

        // Vérifier qu'il y a une arrow_function dans ce declarator.
        let has_arrow = {
            let mut dc = child.walk();
            child
                .children(&mut dc)
                .any(|c| c.kind() == "arrow_function")
        };
        if !has_arrow {
            continue;
        }

        // Nom de la variable (identifier).
        let name = match child.child_by_field_name("name") {
            Some(n) => node_text(n, source),
            None => continue,
        };

        // Extraire la signature depuis l'arrow_function.
        let signature = {
            let mut dc2 = child.walk();
            let mut sig: Option<String> = None;
            for inner in child.children(&mut dc2) {
                if inner.kind() == "arrow_function" {
                    sig = extract_formal_parameters(inner, source);
                    break;
                }
            }
            sig
        };

        let doc_comment = extract_jsdoc(node, source);
        let span = extract_node_span(node);

        symbols.push(DerivedSymbol {
            qualified_name: name.to_string(),
            kind: "fn".to_string(),
            signature,
            doc_comment,
            deps: Vec::new(),
            source_path: source_path.to_string(),
            visibility: visibility.to_string(),
            span,
            ambiguous: false,
        });
    }
}

/// Dispatches a top-level item or one nested under an `export_statement`.
fn dispatch_item(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    ctx: &ParseContext,
    include_private: bool,
    symbols: &mut Vec<DerivedSymbol>,
) {
    match node.kind() {
        "function_declaration" => {
            extract_function_declaration(node, source, source_path, ctx, include_private, symbols);
        }
        "class_declaration" | "interface_declaration" => {
            extract_class_or_interface(node, source, source_path, ctx, include_private, symbols);
        }
        "lexical_declaration" => {
            extract_arrow_function(node, source, source_path, ctx, include_private, symbols);
        }
        _ => {}
    }
}

/// Extracts top-level items from a TypeScript program.
fn extract_program_items(
    program_node: Node<'_>,
    source: &[u8],
    source_path: &str,
    include_private: bool,
    symbols: &mut Vec<DerivedSymbol>,
) {
    let mut cursor = program_node.walk();
    for child in program_node.children(&mut cursor) {
        if child.kind() == "export_statement" {
            // L'item exporté est dans le champ `declaration`.
            if let Some(decl) = child.child_by_field_name("declaration") {
                let ctx = ParseContext { is_exported: true };
                dispatch_item(decl, source, source_path, &ctx, include_private, symbols);
            }
        } else {
            // Item non exporté → priv.
            let ctx = ParseContext { is_exported: false };
            dispatch_item(child, source, source_path, &ctx, include_private, symbols);
        }
    }
}

/// [`crate::language_parser::LanguageParser`] implementation for TypeScript.
///
/// Encapsulates TypeScript grammar knowledge: node kinds, symbol extraction,
/// and the `export`-based visibility convention.
///
/// ## TSX variant
///
/// When `jsx = true`, uses `LANGUAGE_TSX` (the JSX/React grammar from the same
/// `tree-sitter-typescript 0.23.2` crate) instead of `LANGUAGE_TYPESCRIPT`.
///
/// The node kinds `function_declaration`, `class_declaration`, `interface_declaration`,
/// `method_definition`, `lexical_declaration` / `arrow_function` are identical in both
/// grammars — only the addition of JSX nodes differs. The symbol extraction
/// (`extract_symbols`) is therefore identical regardless of `jsx`.
///
/// Constructors:
/// - `TypeScriptParser::ts(include_private)` — `.ts` grammar (LANGUAGE_TYPESCRIPT)
/// - `TypeScriptParser::tsx(include_private)` — `.tsx` grammar (LANGUAGE_TSX / JSX)
pub(crate) struct TypeScriptParser {
    /// When `true`, non-exported (`priv`) items are included in the output.
    pub(crate) include_private: bool,
    /// When `true`, uses `LANGUAGE_TSX` (JSX/React) instead of `LANGUAGE_TYPESCRIPT`.
    pub(crate) jsx: bool,
}

impl TypeScriptParser {
    /// Builds a parser for `.ts` files (grammar `LANGUAGE_TYPESCRIPT`).
    pub(crate) fn ts(include_private: bool) -> Self {
        Self {
            include_private,
            jsx: false,
        }
    }

    /// Builds a parser for `.tsx` files (grammar `LANGUAGE_TSX`, includes JSX).
    pub(crate) fn tsx(include_private: bool) -> Self {
        Self {
            include_private,
            jsx: true,
        }
    }
}

impl crate::language_parser::LanguageParser for TypeScriptParser {
    fn ts_language(&self) -> tree_sitter::Language {
        if self.jsx {
            tree_sitter_typescript::LANGUAGE_TSX.into()
        } else {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
    }

    fn extract_symbols(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        source_path: &str,
    ) -> Vec<DerivedSymbol> {
        let root = tree.root_node();
        let mut symbols = Vec::new();
        extract_program_items(
            root,
            source,
            source_path,
            self.include_private,
            &mut symbols,
        );
        symbols
    }
}
