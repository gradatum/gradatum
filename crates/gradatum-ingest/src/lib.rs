//! # gradatum-ingest
//!
//! Code-ingest pipeline for Gradatum: multi-language source parsing via tree-sitter
//! → derived code symbols → index-only notes.
//!
//! ## Overview
//!
//! Extracted symbols live **only in SQLite** (logical vault `code-<project>`,
//! `provenance = "derived:tree-sitter"`). No Markdown files are written.
//! The git repository is the source of truth; the index is fully derived and
//! can be rebuilt at any time.
//!
//! ## Supported languages
//!
//! Each language is enabled by a dedicated feature flag:
//!
//! | Feature          | Language   | Entry point              |
//! |------------------|------------|--------------------------|
//! | `code-rust`      | Rust       | [`parse_rust_file`]      |
//! | `code-python`    | Python     | [`parse_python_file`]    |
//! | `code-bash`      | Bash       | [`parse_bash_file`]      |
//! | `code-typescript`| TypeScript | [`parse_typescript_file`]|
//!
//! `code-rust` is enabled by default. The remaining parsers are opt-in.
//!
//! ## API stability
//!
//! **This crate is published but the API is not yet stable.** The `parse_<lang>_file`
//! functions, [`DerivedSymbol`], and [`build_derived_notes`] are internal pipeline APIs
//! consumed by `gradatum-worker` and `gradatum-admin`. Their signatures may change across
//! minor versions as new language parsers or symbol kinds are added.
//!
//! External consumers should not depend on this crate directly — it is an implementation
//! detail of the gradatum stack. No SemVer stability is guaranteed for these APIs until
//! a `v1.0` release is published.
//!
//! ## Pipeline
//!
//! 1. Call the language-specific `parse_<lang>_file` function → `Vec<DerivedSymbol>`.
//! 2. Pass the symbols to [`build_derived_notes`] → `Vec<DerivedNote>` (index-ready).
//!
//! Accuracy over coverage: ambiguous or unparseable symbols are silently omitted
//! rather than returned with potentially incorrect metadata.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

use gradatum_core::identity::NoteId;
use gradatum_index::{CodeSymbolMeta, DerivedNote};
use sha2::{Digest, Sha256};

/// Séparateur de composants de clé pour `NoteId::derived_from`.
/// ASCII Unit Separator — n'apparaît jamais dans un identifiant Rust ni un chemin de fichier.
const KEY_SEP: u8 = 0x1f;

/// Symbole de code extrait par tree-sitter (Rust, Python, Bash ou TypeScript selon la feature).
///
/// Représente une entité de code (fonction, type, trait, impl, const, module, méthode, etc.).
/// Produit par les parsers `parse_rust_file`, `parse_python_file`, `parse_bash_file` ou
/// `parse_typescript_file` selon la feature activée, et consommé par `build_derived_notes`.
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

/// Module pipeline générique multi-langage.
///
/// Activé si au moins une feature de parser de langage est activée.
#[cfg(any(
    feature = "code-rust",
    feature = "code-python",
    feature = "code-bash",
    feature = "code-typescript"
))]
mod language_parser;

/// Module tree-sitter Rust (feature `code-rust`).
#[cfg(feature = "code-rust")]
mod rust_parser;

/// Module tree-sitter Python (feature `code-python`).
#[cfg(feature = "code-python")]
mod python_parser;

/// Module tree-sitter Bash (feature `code-bash`).
#[cfg(feature = "code-bash")]
mod bash_parser;

/// Module tree-sitter TypeScript (feature `code-typescript`).
#[cfg(feature = "code-typescript")]
mod typescript_parser;

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
/// Sans la feature, construire des `DerivedSymbol` manuellement via la struct publique.
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
    let parser = rust_parser::RustParser { visibility };
    language_parser::parse_with_language_parser(&parser, source_path, content)
}

/// Parse un fichier Python et retourne la liste des symboles extraits.
///
/// ## Visibilité
///
/// Le paramètre `include_private` contrôle le filtre de visibilité :
/// - `false` (défaut) : seuls les items publics sont extraits (pas de `_`-prefix).
///   Les items dunder (`__init__`, `__str__`) sont toujours publics (API protocole Python).
/// - `true` : tous les items sont extraits, y compris les items `_`-préfixés.
///
/// ## Accuracy > coverage
///
/// En cas de doute (symbole non parsable, source corrompue) → le symbole est omis plutôt
/// que retourné avec une extraction potentiellement fausse. Un fichier entièrement
/// non-parsable retourne `Ok(Vec::new())` (fichier ignoré silencieusement).
///
/// ## Entités extraites
///
/// - Fonctions top-level → `kind = "fn"`
/// - Classes top-level → `kind = "class"`
/// - Méthodes dans les classes → `kind = "method"`, `qualified_name = "Classe::méthode"`
///
/// ## Non-extraits (par design)
///
/// - Assignments module-level (`CONSTANT = 42`) : aucun kind adapté
/// - Deps (call graph) : `deps = vec![]` pour l'incrément 1
///
/// ## Feature `code-python`
///
/// Cette fonction n'est disponible que si la feature `code-python` est activée.
///
/// ## Effets de bord
///
/// Aucun. Fonction pure (lecture seule, pas d'I/O réseau, pas de DB).
#[cfg(feature = "code-python")]
pub fn parse_python_file(
    source_path: &str,
    content: &str,
    include_private: bool,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let parser = python_parser::PythonParser { include_private };
    language_parser::parse_with_language_parser(&parser, source_path, content)
}

/// Parse un fichier Bash et retourne la liste des symboles extraits.
///
/// ## Entités extraites
///
/// - Fonctions top-level (`function_definition`) → `kind = "fn"`
/// - Assignments top-level (`variable_assignment`) → `kind = "const"` (best-effort)
///
/// ## Visibilité
///
/// Bash n'a aucun modificateur de visibilité syntaxique — tous les symboles sont `"pub"`.
/// Ce parser ne prend pas de paramètre `include_private` (pas de concept applicable).
///
/// ## Signature
///
/// `signature = None` par design : Bash ne déclare pas de paramètres typés.
/// Les paramètres positionnels (`$1`, `$2`) ne sont pas déclarés syntaxiquement.
///
/// ## Deps
///
/// `deps = vec![]` — extraction des callees différée (accuracy > coverage).
///
/// ## Feature `code-bash`
///
/// Cette fonction n'est disponible que si la feature `code-bash` est activée.
///
/// ## Effets de bord
///
/// Aucun. Fonction pure (lecture seule, pas d'I/O réseau, pas de DB).
#[cfg(feature = "code-bash")]
pub fn parse_bash_file(
    source_path: &str,
    content: &str,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let parser = bash_parser::BashParser;
    language_parser::parse_with_language_parser(&parser, source_path, content)
}

/// Parse un fichier TypeScript et retourne la liste des symboles extraits.
///
/// ## Entités extraites
///
/// - Fonctions (`function_declaration`) → `kind = "fn"`
/// - Classes (`class_declaration`) → `kind = "class"`
/// - Interfaces (`interface_declaration`) → `kind = "type"`
/// - Méthodes dans les classes (`method_definition`) → `kind = "method"`,
///   `qualified_name = "ClassName::methodName"`
/// - Arrow functions top-level (`const f = () => {}`) → `kind = "fn"`
///
/// ## Visibilité
///
/// - Présence d'un `export_statement` parent → `"pub"`
/// - Absence → `"priv"` (module-local)
/// - Dans une classe, modificateurs `public`/`private`/`protected` respectés.
///   Défaut absent → `"pub"` (TypeScript default visibility = public).
///
/// ## Variante `.tsx`
///
/// Utiliser [`parse_tsx_file`] pour les fichiers `.tsx` (grammaire JSX `LANGUAGE_TSX`).
/// Cette fonction utilise `LANGUAGE_TYPESCRIPT` — les fragments JSX ne sont pas parsés.
///
/// ## Feature `code-typescript`
///
/// Cette fonction n'est disponible que si la feature `code-typescript` est activée.
///
/// ## Effets de bord
///
/// Aucun. Fonction pure (lecture seule, pas d'I/O réseau, pas de DB).
#[cfg(feature = "code-typescript")]
pub fn parse_typescript_file(
    source_path: &str,
    content: &str,
    include_private: bool,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let parser = typescript_parser::TypeScriptParser::ts(include_private);
    language_parser::parse_with_language_parser(&parser, source_path, content)
}

/// Parse un fichier TypeScript JSX (`.tsx`) et retourne la liste des symboles extraits.
///
/// Utilise la grammaire `LANGUAGE_TSX` de `tree-sitter-typescript 0.23.2`, qui comprend
/// les nœuds JSX/React (`jsx_element`, `jsx_opening_element`, etc.) en plus du TS.
///
/// ## Entités extraites
///
/// Identique à [`parse_typescript_file`] (même extraction de symboles) :
/// - Fonctions / composants React (`function_declaration`) → `kind = "fn"`
/// - Classes (`class_declaration`) → `kind = "class"`
/// - Interfaces (`interface_declaration`) → `kind = "type"`
/// - Méthodes dans les classes (`method_definition`) → `kind = "method"`
/// - Arrow functions top-level (`const f = () => {}`) → `kind = "fn"`
///
/// Les nœuds JSX (éléments `<div>`, `<Button>`, etc.) sont parsés correctement
/// mais **ne génèrent pas de symboles** — seules les déclarations TS sont extraites.
///
/// ## Visibilité
///
/// - `export` → `"pub"` ; absence → `"priv"`.
///
/// ## Feature `code-typescript`
///
/// Cette fonction n'est disponible que si la feature `code-typescript` est activée.
///
/// ## Effets de bord
///
/// Aucun. Fonction pure (lecture seule, pas d'I/O réseau, pas de DB).
///
/// # Errors
///
/// Retourne [`IngestError::ParseError`] si la grammaire `LANGUAGE_TSX` est ABI-incompatible
/// avec le core tree-sitter (détecté au premier appel, non silencieux).
#[cfg(feature = "code-typescript")]
pub fn parse_tsx_file(
    source_path: &str,
    content: &str,
    include_private: bool,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let parser = typescript_parser::TypeScriptParser::tsx(include_private);
    language_parser::parse_with_language_parser(&parser, source_path, content)
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
/// `"code <lang> <kind> <module_name>"` où `lang` est déduit de l'extension du fichier source
/// (`rs`→`rust`, `py`→`python`, `sh`/`bash`→`bash`, `ts`/`tsx`→`typescript`, sinon `unknown`).
/// `module_name` = composant avant le `::`, ou `"root"` si absent.
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

        // Tags : "code <lang> <kind> <module>"
        let module_name = if let Some(pos) = sym.qualified_name.find("::") {
            sym.qualified_name[..pos].to_string()
        } else {
            "root".to_string()
        };
        let lang = lang_from_path(&sym.source_path);
        let tags = format!("code {} {} {}", lang, sym.kind, module_name);

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

/// Déduit le langage de programmation depuis l'extension d'un chemin de fichier source.
///
/// Utilisé par `build_derived_notes` pour tagger les notes code avec le bon langage.
fn lang_from_path(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "rs" => "rust",
        "py" => "python",
        "sh" | "bash" => "bash",
        "ts" | "tsx" => "typescript",
        _ => "unknown",
    }
}

/// Calcule le hash SHA-256 hex d'un contenu de fichier.
///
/// Utilisé comme `content_hash_source` pour l'idempotence (skip si inchangé).
pub fn content_hash_source(bytes: &[u8]) -> String {
    let hash: [u8; 32] = Sha256::digest(bytes).into();
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod lang_from_path_tests {
    use super::lang_from_path;

    #[test]
    fn lang_from_path_rust() {
        assert_eq!(lang_from_path("src/foo/bar.rs"), "rust");
    }

    #[test]
    fn lang_from_path_bash() {
        assert_eq!(lang_from_path("scripts/setup.sh"), "bash");
    }

    #[test]
    fn lang_from_path_bash_extension() {
        assert_eq!(lang_from_path("scripts/run.bash"), "bash");
    }

    #[test]
    fn lang_from_path_typescript() {
        assert_eq!(lang_from_path("src/app.ts"), "typescript");
    }

    #[test]
    fn lang_from_path_tsx() {
        assert_eq!(lang_from_path("src/components/App.tsx"), "typescript");
    }

    #[test]
    fn lang_from_path_python() {
        assert_eq!(lang_from_path("scripts/migrate.py"), "python");
    }

    #[test]
    fn lang_from_path_unknown() {
        assert_eq!(lang_from_path("Makefile"), "unknown");
    }

    #[test]
    #[cfg(feature = "code-bash")]
    fn build_derived_notes_bash_tag() {
        use crate::{DerivedSymbol, build_derived_notes};

        let sym = DerivedSymbol {
            qualified_name: "setup_env".to_string(),
            kind: "fn".to_string(),
            signature: None,
            doc_comment: None,
            deps: vec![],
            source_path: "scripts/setup.sh".to_string(),
            visibility: "pub".to_string(),
            ambiguous: false,
            span: None,
        };
        let notes = build_derived_notes("code-test", vec![sym]);
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].tags.contains("bash"),
            "tag doit contenir 'bash', obtenu: {}",
            notes[0].tags
        );
        assert!(
            !notes[0].tags.contains("rust"),
            "tag ne doit pas contenir 'rust' pour un .sh, obtenu: {}",
            notes[0].tags
        );
    }

    #[test]
    #[cfg(feature = "code-typescript")]
    fn build_derived_notes_typescript_tag() {
        use crate::{DerivedSymbol, build_derived_notes};

        let sym = DerivedSymbol {
            qualified_name: "fetchData".to_string(),
            kind: "fn".to_string(),
            signature: None,
            doc_comment: None,
            deps: vec![],
            source_path: "src/api/client.ts".to_string(),
            visibility: "pub".to_string(),
            ambiguous: false,
            span: None,
        };
        let notes = build_derived_notes("code-test", vec![sym]);
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].tags.contains("typescript"),
            "tag doit contenir 'typescript', obtenu: {}",
            notes[0].tags
        );
        assert!(
            !notes[0].tags.contains("rust"),
            "tag ne doit pas contenir 'rust' pour un .ts, obtenu: {}",
            notes[0].tags
        );
    }
}
