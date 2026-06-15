//! # gradatum-ingest
//!
//! Pipeline code-ingest pour Gradatum (v0.5.2 Phase A).
//!
//! ## Principe index-only
//!
//! Les notes code vivent **uniquement dans SQLite** (vault_id logique `code-<projet>`,
//! table `notes`, `provenance="derived:tree-sitter"`). Aucun fichier Markdown.
//! La source de vérité est le repo git — l'index est entièrement dérivé.
//!
//! ## Fonctionnalités
//!
//! - `parse_rust_file` (feature `code-rust`) : tree-sitter Rust → `DerivedSymbol`
//!   (fn/struct/enum/trait/impl/const/mod + méthodes publiques).
//! - `build_derived_notes` : `DerivedSymbol` → `DerivedNote` (body_text borné ≤ 60 lignes,
//!   note_id déterministe via `NoteId::derived_from`).
//! - Accuracy > coverage : duplicates ambigus omis, symboles non-parsables ignorés.
//!
//! ## Feature `code-rust`
//!
//! Activée par défaut. Dépend de `tree-sitter` + `tree-sitter-rust` (lien statique).
//! La dépendance C reste isolée dans ce crate (ne pollue pas gradatum-admin directement).
//!
//! ## Stabilité
//!
//! `0.x` — aucune garantie API. `publish = false`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

use gradatum_core::identity::NoteId;
use gradatum_index::{CodeSymbolMeta, DerivedNote};
use sha2::{Digest, Sha256};

/// Séparateur de composants de clé pour `NoteId::derived_from`.
/// ASCII Unit Separator — n'apparaît jamais dans un identifiant Rust ni un chemin de fichier.
const KEY_SEP: u8 = 0x1f;

/// Symbole extrait d'un fichier source Rust par tree-sitter.
///
/// Représente une entité de code (fonction, type, trait, impl, const, module ou méthode).
/// Produit par `parse_rust_file` et consommé par `build_derived_notes`.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedSymbol {
    /// Nom qualifié (ex. `"MyStruct::my_method"`, `"parse_file"`, `"MyEnum"`).
    pub qualified_name: String,
    /// Kind de l'entité (ex. `"fn"`, `"struct"`, `"enum"`, `"trait"`, `"impl"`,
    /// `"const"`, `"mod"`, `"method"`).
    pub kind: String,
    /// Signature textuelle (params + retour, ≤ 1 ligne). Absente si non extractible.
    pub signature: Option<String>,
    /// Doc-comment extrait (premières lignes). Absent si non présent dans la source.
    pub doc_comment: Option<String>,
    /// Dépendances intra-repo sortantes (best-effort, flaggées incertaines si besoin).
    /// Accuracy > coverage : en cas de doute, la dep est omise plutôt que hallucinée.
    pub deps: Vec<String>,
    /// Chemin du fichier source (relatif au repo).
    pub source_path: String,
    /// Visibilité de l'item : `"pub"` (item public) ou `"priv"` (item privé).
    /// Pour les blocs `impl` : toujours `"pub"` (les impl n'ont pas de visibilité propre).
    pub visibility: String,
    /// Span du nœud tree-sitter (1-based inclusif) : `(start_line, end_line)`.
    ///
    /// Capture le nœud de l'item seul (pas les attributs `#[...]` ni les doc-comments —
    /// ceux-ci sont déjà dans `doc_comment`). `None` si non extractible (accuracy>coverage).
    ///
    /// Utilisé par `code_scope include_body` pour servir le corps exact sans re-parse.
    /// Invariant B2 (caveats council) : exclure la ligne vide terminale si `end_position().row`
    /// pointe une ligne vide (newline final) — seul le corps réel est servi.
    pub span: Option<(u32, u32)>,
    /// Indicateur que ce symbole est ambigu (overload / macro opaque à tree-sitter).
    /// Un symbole ambigu est omis lors de la construction des notes.
    pub ambiguous: bool,
}

/// Erreur du pipeline ingest.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// Erreur de lecture ou de parse du fichier source.
    #[error("parse error pour {path}: {reason}")]
    ParseError {
        /// Chemin du fichier source.
        path: String,
        /// Raison de l'échec.
        reason: String,
    },
    /// Erreur interne inattendue.
    #[error("ingest internal error: {0}")]
    Internal(String),
}

/// Module tree-sitter Rust (feature `code-rust`).
#[cfg(feature = "code-rust")]
mod rust_parser;

/// Parse un fichier Rust et retourne la liste des symboles extraits.
///
/// ## Visibilité
///
/// Le paramètre `include_private` contrôle le filtre de visibilité :
/// - `false` (défaut) : seuls les items `pub` ou `pub(crate)` sont extraits.
///   Les modules sont toujours extraits quel que soit ce paramètre (ils structurent l'espace
///   de noms). Comportement historique préservé.
/// - `true` : tous les items sont extraits, y compris les items privés.
///
/// ## Accuracy > coverage
///
/// En cas de doute (symbole non parsable, source corrompue) → le symbole est omis plutôt
/// que retourné avec une signature potentiellement fausse. Un fichier entièrement non-parsable
/// retourne `Ok(Vec::new())` (pas d'erreur — le fichier est ignoré silencieusement).
///
/// ## Macros procédurales
///
/// `#[derive]` et macros procédurales sont invisibles à tree-sitter → omises.
/// Comportement documenté, pas un bug.
///
/// ## Duplicates ambigus
///
/// Même `(path, kind, qualified_name)` : overloads ou macros opaques → le symbole est
/// marqué `ambiguous=true` et filtré par `build_derived_notes`.
///
/// ## Feature `code-rust`
///
/// Cette fonction n'est disponible que si la feature `code-rust` est activée.
/// Sans la feature, utiliser `parse_rust_file_stub` pour les tests.
///
/// ## Effets de bord
///
/// Aucun. Fonction pure (lecture seule, pas d'I/O réseau, pas de DB).
#[cfg(feature = "code-rust")]
pub fn parse_rust_file(
    source_path: &str,
    content: &str,
    include_private: bool,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let visibility = if include_private {
        rust_parser::Visibility::All
    } else {
        rust_parser::Visibility::Pub
    };
    rust_parser::parse(source_path, content, visibility)
}

/// Construit les `DerivedNote` depuis les symboles extraits.
///
/// ## Clé déterministe
///
/// `note_id = NoteId::derived_from(vault_id ‖ '\x1f' ‖ source_path ‖ '\x1f' ‖ kind ‖ '\x1f' ‖ qualified_name)`
///
/// Garantit idempotence : rebuild identique quelle que soit l'ordre d'insertion.
///
/// ## Filtrage
///
/// - Symboles `ambiguous=true` → omis (accuracy > coverage, §3.2 spec).
/// - Duplicates `(kind, qualified_name)` après filtrage → le premier est conservé.
///
/// ## Body_text
///
/// Construit comme : `kind qualified_name\n[signature]\n[doc_comment]\ndeps: ...`
/// Cap strict ≤ 60 lignes.
///
/// ## Tags
///
/// `"code rust <kind> <module_name>"` (module_name = composant avant le `::`, ou "root").
///
/// ## Effets de bord
///
/// Aucun. Fonction pure.
pub fn build_derived_notes(vault_id: &str, symbols: Vec<DerivedSymbol>) -> Vec<DerivedNote> {
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut result = Vec::new();

    for sym in symbols {
        // Accuracy > coverage : sauter les symboles ambigus.
        if sym.ambiguous {
            tracing::debug!(
                path = %sym.source_path,
                name = %sym.qualified_name,
                "symbole ambigu omis"
            );
            continue;
        }

        // Déduplication (kind, qualified_name) — ne devrait pas arriver après parse
        // mais défense en profondeur.
        let dedup_key = (sym.kind.clone(), sym.qualified_name.clone());
        if seen.contains(&dedup_key) {
            tracing::debug!(
                path = %sym.source_path,
                name = %sym.qualified_name,
                "duplicate omis"
            );
            continue;
        }
        seen.insert(dedup_key);

        // Construire la clé déterministe.
        let mut key = Vec::new();
        key.extend_from_slice(vault_id.as_bytes());
        key.push(KEY_SEP);
        key.extend_from_slice(sym.source_path.as_bytes());
        key.push(KEY_SEP);
        key.extend_from_slice(sym.kind.as_bytes());
        key.push(KEY_SEP);
        key.extend_from_slice(sym.qualified_name.as_bytes());
        let id = NoteId::derived_from(&key);

        // Construire body_text (cap ≤ 60 lignes).
        let mut lines: Vec<String> = Vec::with_capacity(8);
        lines.push(format!("{} `{}`", sym.kind, sym.qualified_name));
        lines.push(format!("source: {}", sym.source_path));
        lines.push(format!("visibility: {}", sym.visibility));

        if let Some(sig) = &sym.signature {
            lines.push(String::new());
            lines.push(format!("signature: {sig}"));
        }

        if let Some(doc) = &sym.doc_comment {
            lines.push(String::new());
            // Limiter le doc-comment à 5 lignes.
            for (i, doc_line) in doc.lines().take(5).enumerate() {
                if i == 0 {
                    lines.push(format!("doc: {doc_line}"));
                } else {
                    lines.push(format!("     {doc_line}"));
                }
            }
        }

        if !sym.deps.is_empty() {
            lines.push(String::new());
            lines.push("deps:".to_string());
            for dep in sym.deps.iter().take(10) {
                lines.push(format!("  - {dep}"));
            }
        }

        // Cap strict ≤ 60 lignes.
        let body_text = lines
            .iter()
            .take(60)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");

        // Tags : "code rust <kind> <module>"
        let module_name = if let Some(pos) = sym.qualified_name.find("::") {
            sym.qualified_name[..pos].to_string()
        } else {
            "root".to_string()
        };
        let tags = format!("code rust {} {}", sym.kind, module_name);

        // Titre = qualified_name (court, utile pour l'index titre).
        let title = Some(sym.qualified_name.clone());

        // Métadonnées structurées pour code_scope (persistées dans extra_json["cs"]).
        // Évite au handler de re-parser le body_text (couplage fragile).
        // Le span est propagé pour permettre `include_body` (lecture corps au grain symbole).
        let code_meta = Some(CodeSymbolMeta {
            source_path: sym.source_path.clone(),
            kind: sym.kind.clone(),
            qualified_name: sym.qualified_name.clone(),
            signature: sym.signature.clone(),
            deps: sym.deps.clone(),
            visibility: Some(sym.visibility.clone()),
            span: sym.span,
        });

        result.push(DerivedNote {
            id,
            body_text,
            tags,
            title,
            code_meta,
        });
    }

    result
}

/// Calcule le hash SHA-256 hex d'un contenu de fichier.
///
/// Utilisé comme `content_hash_source` pour l'idempotence (skip si inchangé).
pub fn content_hash_source(bytes: &[u8]) -> String {
    let hash: [u8; 32] = Sha256::digest(bytes).into();
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
