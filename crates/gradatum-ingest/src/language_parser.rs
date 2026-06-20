//! Abstraction générique de parser de langage (feature `code-rust`).
//!
//! Ce module expose le trait [`LanguageParser`] et la fonction pipeline
//! [`parse_with_language_parser`] qui orchestre le pipeline tree-sitter commun :
//! création du Parser, `set_language`, `parse`, gestion d'erreurs.
//!
//! Les détails propres à chaque langage (grammaire, extraction de symboles) sont
//! délégués aux implémentations du trait.
//!
//! ## Extensibilité
//!
//! Pour ajouter le support d'un nouveau langage (TypeScript, Python, Bash…) :
//! 1. Créer un fichier `<lang>_parser.rs` dans ce crate.
//! 2. Y définir un struct `<Lang>Parser { … }` qui implémente `LanguageParser`.
//! 3. Exposer une fonction `parse_<lang>_file` dans `lib.rs` qui instancie le struct
//!    et appelle `parse_with_language_parser`.
//!
//! Le pipeline commun (ce module) n'a pas à changer.

use crate::{DerivedSymbol, IngestError};

/// Abstraction d'un parser pour un langage source vers des symboles dérivés.
///
/// Un impl de ce trait encapsule la connaissance spécifique au langage :
/// - la grammaire tree-sitter à utiliser,
/// - la logique d'extraction de symboles depuis l'AST.
///
/// Le pipeline commun (création du Parser, set_language, parse, gestion d'erreurs)
/// est externalisé dans [`parse_with_language_parser`].
///
/// # Errors
/// Voir [`IngestError`].
pub(crate) trait LanguageParser {
    /// Retourne la grammaire tree-sitter pour ce langage.
    fn ts_language(&self) -> tree_sitter::Language;

    /// Extrait les symboles depuis un AST tree-sitter parsé.
    ///
    /// # Paramètres
    /// - `tree` : l'AST produit par `tree_sitter::Parser::parse`.
    /// - `source` : les bytes sources (identiques à ceux passés au parser).
    /// - `source_path` : chemin relatif du fichier (pour les erreurs et les DerivedSymbol).
    fn extract_symbols(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        source_path: &str,
    ) -> Vec<DerivedSymbol>;
}

/// Orchestre le pipeline commun : crée un Parser tree-sitter, set_language,
/// parse les bytes sources, puis délègue l'extraction au [`LanguageParser`] fourni.
///
/// # Errors
/// - [`IngestError::ParseError`] si `set_language` échoue.
/// - Retourne `Ok(Vec::new())` si `parser.parse` retourne `None` (fichier ignoré silencieusement).
pub(crate) fn parse_with_language_parser(
    parser_impl: &dyn LanguageParser,
    source_path: &str,
    content: &str,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let mut parser = tree_sitter::Parser::new();
    let language = parser_impl.ts_language();
    parser
        .set_language(&language)
        .map_err(|e| IngestError::ParseError {
            path: source_path.to_string(),
            reason: format!("tree-sitter set_language: {e}"),
        })?;

    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => {
            tracing::warn!(path = %source_path, "tree-sitter parse returned None (fichier ignoré)");
            return Ok(Vec::new());
        }
    };

    let source_bytes = content.as_bytes();
    Ok(parser_impl.extract_symbols(&tree, source_bytes, source_path))
}
