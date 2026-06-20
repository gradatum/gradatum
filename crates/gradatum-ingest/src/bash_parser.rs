//! Parser tree-sitter pour les fichiers Bash (feature `code-bash`).
//!
//! ## Entités extraites
//!
//! - Fonctions (`function_definition`) → `DerivedSymbol` avec `kind = "fn"`.
//!   Les deux formes Bash sont identiques dans l'AST tree-sitter-bash 0.25.1 :
//!   `foo() { ... }` et `function bar { ... }` produisent toutes les deux un nœud
//!   `function_definition` avec un champ nommé `name` de type `word`.
//! - Assignments top-level (`variable_assignment`) → `kind = "const"` (best-effort :
//!   Bash n'a pas de notion de constante, mais c'est le kind le plus proche).
//!
//! ## Visibilité
//!
//! Bash n'a aucun modificateur syntaxique de visibilité. Tout est `"pub"`.
//!
//! ## Signature
//!
//! Bash ne déclare pas de paramètres typés — ils sont positionnels (`$1`, `$2`...).
//! `signature = None` par design (accuracy > coverage : une signature hallucinée serait trompeuse).
//!
//! ## Doc-comments
//!
//! Les nœuds `comment` (lignes `#`) qui précèdent immédiatement une `function_definition`
//! en tant que siblings du nœud parent sont extraits comme doc-comment.
//! Limité à 5 lignes, cohérent avec les autres parsers.
//!
//! ## Deps (call graph)
//!
//! `deps = vec![]` — extraction des callees (nœuds `command_name` sous `command`) différée.
//! La structure AST permettrait l'extraction, mais accuracy > coverage guide à ne pas
//! hallucer des dépendances incertaines (alias Bash, fonctions de shell builtin, etc.).
//!
//! ## Non-extraits (accuracy > coverage)
//!
//! - Assignments dans des sous-blocs (dans une fonction) : uniquement le niveau `program` racine.
//! - Fonctions définies dynamiquement (via eval, export -f) : invisibles à tree-sitter.
//! - Fonctions importées via `source` ou `.` : ignorées.

use tree_sitter::Node;

use crate::DerivedSymbol;

/// Retourne le texte UTF-8 d'un nœud tree-sitter.
///
/// ## Invariant de sécurité
///
/// `source` doit être le MÊME buffer passé à `parser.parse(content, None)`.
/// Les offsets byte de l'AST sont garantis dans ce slice.
/// `.unwrap_or("")` est défensif mais ne se déclenche normalement jamais.
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Calcule le span 1-based inclusif `(start_line, end_line)` d'un nœud.
///
/// Mirror de la même fonction dans `python_parser.rs` et `rust_parser.rs`.
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

/// Extrait les lignes de commentaire précédant immédiatement un nœud cible.
///
/// Parcourt les siblings précédents du nœud dans son parent immédiat.
/// Ne collecte que les `comment` contigus (s'arrête dès qu'un sibling non-comment
/// est rencontré en remontant). Limité à 5 lignes.
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

/// Extrait une `function_definition` Bash.
///
/// Dans l'AST tree-sitter-bash 0.25.1, `function_definition` a un champ nommé `name`
/// de type `word` (nom de la fonction) et un champ `body` de type `compound_statement`.
/// Les deux formes syntaxiques (`foo(){}` et `function foo {}`) produisent le même nœud
/// dans l'AST — pas besoin de les distinguer.
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

/// Extrait un `variable_assignment` top-level.
///
/// Dans l'AST tree-sitter-bash 0.25.1, `variable_assignment` a :
/// - Un champ nommé `name` de type `variable_name`.
/// - Un champ nommé `value` (optionnel).
///
/// Kind = "const" (best-effort — Bash n'a pas de constante syntaxique,
/// mais les assignments top-level correspondent à des variables de configuration).
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

/// Extrait les items top-level d'un programme Bash.
///
/// Parcourt les enfants directs du nœud `program` (racine de l'AST Bash).
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

/// Implémentation [`crate::language_parser::LanguageParser`] pour Bash (tree-sitter-bash).
///
/// Encapsule la connaissance de la grammaire Bash : node-kinds, extraction de symboles.
/// Pas de champ `include_private` : Bash n'a aucun modificateur de visibilité syntaxique.
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
