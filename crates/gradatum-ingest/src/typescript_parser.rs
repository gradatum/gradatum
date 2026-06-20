//! Parser tree-sitter pour les fichiers TypeScript (feature `code-typescript`).
//!
//! ## Entités extraites
//!
//! - Fonctions (`function_declaration`) → `DerivedSymbol` avec `kind = "fn"`
//! - Classes (`class_declaration`) → `DerivedSymbol` avec `kind = "class"`
//! - Interfaces (`interface_declaration`) → `DerivedSymbol` avec `kind = "type"`
//! - Méthodes dans les classes (`method_definition`) → `kind = "method"`,
//!   `qualified_name = "ClassName::methodName"`
//! - Arrow functions top-level (`const f = () => {}`) → `kind = "fn"`
//!
//! ## Visibilité
//!
//! - Présence d'un `export_statement` parent → `"pub"`
//! - Absence → `"priv"` (module-local, non exporté)
//! - Dans une classe, modificateurs `public`/`private`/`protected` respectés.
//!   Défaut absent → `"pub"` (TypeScript default visibility = public).
//!
//! ## Docstrings JSDoc
//!
//! Les blocs `/** ... */` (nœuds `comment` précédant l'item, texte commençant par `/**`)
//! sont extraits comme doc-comment. Limité à 5 lignes.
//!
//! ## Signature
//!
//! Texte brut du nœud `formal_parameters`, tronqué à 120 bytes (char-safe).
//! `None` pour les classes et interfaces.
//!
//! ## Variante TSX
//!
//! Non couverte : `LANGUAGE_TYPESCRIPT` ne parse pas JSX.
//! Utiliser `LANGUAGE_TSX` (non exposé par cette fonction) pour `.tsx`.
//! Comportement documenté, pas un bug.
//!
//! ## Deps
//!
//! `deps = vec![]` pour cette implémentation (accuracy > coverage).

use tree_sitter::Node;

use crate::DerivedSymbol;

/// Retourne le texte UTF-8 d'un nœud tree-sitter.
///
/// ## Invariant de sécurité
///
/// `source` doit être le MÊME buffer passé à `parser.parse(content, None)`.
/// `.unwrap_or("")` est défensif mais ne se déclenche normalement jamais.
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Calcule le span 1-based inclusif `(start_line, end_line)` d'un nœud.
///
/// Mirror de la même fonction dans `python_parser.rs` et `bash_parser.rs`.
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

/// Tronque un texte à 120 bytes de manière char-safe.
///
/// Mirror du pattern utilisé dans `rust_parser.rs` et `python_parser.rs`.
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

/// Extrait le commentaire JSDoc (`/** ... */`) précédant immédiatement un nœud.
///
/// Parcourt les siblings précédents du nœud dans son parent. Ne collecte que
/// les `comment` contigus. Limité à 5 lignes.
///
/// ## Cas export_statement
///
/// Quand un item est déclaré via `export_statement`, le comment JSDoc est un sibling
/// de l'`export_statement`, PAS de la déclaration inner. Dans ce cas, on remonte
/// au niveau de l'`export_statement` pour chercher les siblings.
///
/// Exemple d'AST :
/// ```text
/// (program
///   (comment "/** Handler. */")   ← sibling de export_statement
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

/// Extrait la signature depuis le nœud `formal_parameters`.
///
/// Texte brut tronqué à 120 bytes (char-safe). `None` si le nœud n'est pas trouvé.
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

/// Détermine la visibilité d'une méthode depuis ses modificateurs TypeScript.
///
/// Cherche les nœuds `accessibility_modifier` enfants du `method_definition`.
/// - `public` → `"pub"`
/// - `private` → `"priv"`
/// - `protected` → `"priv"` (inaccessible depuis l'extérieur)
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

/// Contexte de parsing : visibilité export de l'item courant.
struct ParseContext {
    /// Si `true`, l'item courant est sous un `export_statement`.
    is_exported: bool,
}

/// Extrait les items d'un `class_body` (méthodes).
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

/// Extrait un `class_declaration` ou `interface_declaration`.
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

/// Extrait une `function_declaration`.
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

/// Extrait une `lexical_declaration` top-level contenant une `arrow_function`.
///
/// Patterns reconnus :
/// - `const f = () => {}` : `lexical_declaration` → `variable_declarator` → `arrow_function`
///
/// Le nom est extrait du `variable_declarator` (champ `name` de type `identifier`).
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

/// Dispatch d'un item top-level ou sous un `export_statement`.
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

/// Extrait les items top-level d'un programme TypeScript.
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

/// Implémentation [`crate::language_parser::LanguageParser`] pour TypeScript.
///
/// Encapsule la connaissance de la grammaire TypeScript : node-kinds, extraction
/// de symboles, convention de visibilité `export`.
///
/// ## Variante TSX
///
/// Quand `jsx = true`, utilise `LANGUAGE_TSX` (grammaire JSX/React fournie par la même
/// crate `tree-sitter-typescript 0.23.2`) à la place de `LANGUAGE_TYPESCRIPT`.
///
/// Les node-kinds `function_declaration`, `class_declaration`, `interface_declaration`,
/// `method_definition`, `lexical_declaration` / `arrow_function` sont identiques entre
/// les deux grammaires — seul l'ajout de nœuds JSX diffère. L'extraction de symboles
/// (`extract_symbols`) est donc identique quel que soit `jsx`.
///
/// Constructeurs :
/// - `TypeScriptParser::ts(include_private)` — grammaire `.ts` (LANGUAGE_TYPESCRIPT)
/// - `TypeScriptParser::tsx(include_private)` — grammaire `.tsx` (LANGUAGE_TSX / JSX)
pub(crate) struct TypeScriptParser {
    /// Si `true`, inclure les items non-exportés (`priv`) dans les symboles extraits.
    pub(crate) include_private: bool,
    /// Si `true`, utiliser `LANGUAGE_TSX` (JSX/React) au lieu de `LANGUAGE_TYPESCRIPT`.
    pub(crate) jsx: bool,
}

impl TypeScriptParser {
    /// Construit un parser pour fichiers `.ts` (grammaire `LANGUAGE_TYPESCRIPT`).
    pub(crate) fn ts(include_private: bool) -> Self {
        Self {
            include_private,
            jsx: false,
        }
    }

    /// Construit un parser pour fichiers `.tsx` (grammaire `LANGUAGE_TSX`, comprend JSX).
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
