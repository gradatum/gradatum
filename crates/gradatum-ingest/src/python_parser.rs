//! Parser tree-sitter pour les fichiers Python (feature `code-python`).
//!
//! ## Entités extraites
//!
//! - Fonctions top-level (`function_definition`) → `DerivedSymbol` avec `kind = "fn"`
//! - Classes top-level (`class_definition`) → `DerivedSymbol` avec `kind = "class"`
//! - Méthodes (fonctions dans un bloc `block` enfant d'une classe) → `kind = "method"`,
//!   `qualified_name = "ClassName::method_name"`
//!
//! ## Visibilité Python
//!
//! Python n'a pas de modificateur de visibilité syntaxique. Convention :
//! - Nom commençant par `_` (mais pas `__dunder__`) → `"priv"`
//! - Noms dunder (`__init__`, `__str__`, etc.) → `"pub"` (API publique du protocole)
//! - Tout autre nom → `"pub"`
//!
//! ## Non-extraits (accuracy > coverage)
//!
//! - Assignments module-level (`CONSTANT = 42`) : aucun kind adapté dans `DerivedSymbol`
//!   (force-fitter en "const" serait trompeur en Python — skip documenté)
//! - Fonctions/classes dans des blocs conditionnels (`if __name__ == "__main__":`)
//! - Decorated definitions : décorateurs lus mais l'entité inner est extraite normalement
//! - Deps (call graph) : `deps: vec![]` — TODO F-61 inc2 (extraction call_expression Python)
//!
//! ## Docstrings
//!
//! Le premier `expression_statement` contenant une `string` dans le `block` d'une
//! fonction ou classe est traité comme docstring (PEP 257).
//! La docstring module-level (premier statement du module) est ignorée (non associée
//! à un symbole extractible).

use tree_sitter::Node;

use crate::DerivedSymbol;

/// Retourne le texte UTF-8 d'un nœud tree-sitter.
///
/// ## Invariant de sécurité
///
/// `source` doit être le MÊME buffer passé à `parser.parse(content, None)`.
/// Les offsets byte de l'AST sont garantis dans ce slice — `utf8_text` ne peut
/// pas indexer hors-bornes. `.unwrap_or("")` est défensif mais ne se déclenche
/// normalement jamais.
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Détermine la visibilité d'un symbole Python selon la convention `_`-prefix.
///
/// Règles :
/// - `__dunder__` (double underscore des deux côtés) → `"pub"` (protocole Python public)
/// - `_single_prefix` → `"priv"` (convention privé)
/// - Tout autre nom → `"pub"`
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

/// Calcule le span 1-based inclusif `(start_line, end_line)` d'un nœud.
///
/// Mirror de la même fonction dans `rust_parser.rs` (caveats council B2/B3) :
/// - Lines 1-based : `row + 1` (tree-sitter = 0-based).
/// - Si `end_position().column == 0` et `row > 0` → exclure la ligne vide terminale.
/// - Span dégénéré (`start > end`) → `None`.
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

/// Extrait la docstring d'un bloc Python (première `expression_statement` contenant une `string`).
///
/// Selon PEP 257, la docstring est le premier statement du body si c'est un littéral string.
/// La fonction limite à 5 lignes (cohérent avec rust_parser.rs).
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

/// Extrait la signature textuelle d'une fonction Python (paramètres + retour).
///
/// Extrait le texte brut du nœud `parameters`, avec troncature char-safe à 120 bytes.
/// Si un type de retour est présent (après `->`) il est ajouté.
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

/// Extrait une fonction top-level ou une méthode.
///
/// # Paramètres
///
/// - `class_name` : `Some("ClassName")` si cette fonction est une méthode d'une classe.
/// - `visibility_filter` : si `true`, exclure les items privés (`_`-prefix).
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

/// Extrait les méthodes d'une classe (fonctions dans le bloc body).
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

/// Extrait une classe top-level et ses méthodes.
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

/// Extrait les items top-level d'un module Python.
///
/// Parcourt les enfants directs du nœud `module` et dispatche selon le kind.
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

/// Implémentation [`crate::language_parser::LanguageParser`] pour Python (tree-sitter-python).
///
/// Encapsule la connaissance de la grammaire Python : node-kinds, extraction de symboles,
/// convention de visibilité `_`-prefix.
pub(crate) struct PythonParser {
    /// Si `true`, inclure les items privés (`_`-prefix) dans les symboles extraits.
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
