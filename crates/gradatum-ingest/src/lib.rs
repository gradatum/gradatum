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
//! | `code-python`    | Python     | `parse_python_file`      |
//! | `code-bash`      | Bash       | `parse_bash_file`        |
//! | `code-typescript`| TypeScript | `parse_typescript_file`  |
//!
//! `code-rust` is enabled by default. The remaining parsers are opt-in.
//!
//! ## Stability
//!
//! `2.0.0` — public API under [SemVer 2.0.0](https://semver.org): backward-compatible
//! additions only within `2.x`, breaking changes deferred to the next major. See
//! [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! That said, the `parse_<lang>_file` functions, [`DerivedSymbol`] and
//! [`build_derived_notes`] are pipeline APIs consumed by `gradatum-worker` and
//! `gradatum-admin`. They are an implementation detail of the gradatum stack, and
//! external consumers are advised not to depend on this crate directly.
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

/// Key component separator for `NoteId::derived_from`.
/// ASCII Unit Separator — never appears in a Rust identifier or a file path.
const KEY_SEP: u8 = 0x1f;

/// A code symbol extracted by tree-sitter (Rust, Python, Bash, or TypeScript depending on the active feature).
///
/// Represents a code entity (function, type, trait, impl, const, module, method, etc.).
/// Produced by `parse_rust_file`, `parse_python_file`, `parse_bash_file`, or
/// `parse_typescript_file` (depending on the active feature) and consumed by `build_derived_notes`.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedSymbol {
    /// Qualified name (e.g. `"MyStruct::my_method"`, `"parse_file"`, `"MyEnum"`).
    pub qualified_name: String,
    /// Entity kind (e.g. `"fn"`, `"struct"`, `"enum"`, `"trait"`, `"impl"`,
    /// `"const"`, `"mod"`, `"method"`).
    pub kind: String,
    /// Textual signature (params + return type, ≤ 1 line). `None` if not extractable.
    pub signature: Option<String>,
    /// Extracted doc-comment (first lines). `None` if not present in the source.
    pub doc_comment: Option<String>,
    /// Outgoing intra-repo dependencies (best-effort; omitted rather than fabricated when uncertain).
    pub deps: Vec<String>,
    /// Source file path (relative to the repository root).
    pub source_path: String,
    /// Item visibility: `"pub"` (public item) or `"priv"` (private item).
    /// For `impl` blocks: always `"pub"` (impl blocks have no visibility of their own).
    pub visibility: String,
    /// Tree-sitter node span (1-based inclusive): `(start_line, end_line)`.
    ///
    /// Covers the item node only (not preceding `#[...]` attributes or doc-comments —
    /// those are already in `doc_comment`). `None` if not extractable (accuracy > coverage).
    ///
    /// Used by `code_scope include_body` to serve the exact body without re-parsing.
    /// The trailing blank line is excluded when `end_position().row` points at a blank line
    /// (final newline) — only the real body is served.
    pub span: Option<(u32, u32)>,
    /// Whether this symbol is ambiguous (overload or macro opaque to tree-sitter).
    /// Ambiguous symbols are omitted when building notes.
    pub ambiguous: bool,
}

/// Error type for the ingest pipeline.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// A source file could not be read or parsed.
    #[error("parse error for {path}: {reason}")]
    ParseError {
        /// Source file path.
        path: String,
        /// Reason for the failure.
        reason: String,
    },
    /// Unexpected internal error.
    #[error("ingest internal error: {0}")]
    Internal(String),
}

/// Generic multi-language parser pipeline.
///
/// Enabled when at least one language-parser feature is active.
#[cfg(any(
    feature = "code-rust",
    feature = "code-python",
    feature = "code-bash",
    feature = "code-typescript"
))]
mod language_parser;

/// Tree-sitter Rust parser module (feature `code-rust`).
#[cfg(feature = "code-rust")]
mod rust_parser;

/// Tree-sitter Python parser module (feature `code-python`).
#[cfg(feature = "code-python")]
mod python_parser;

/// Tree-sitter Bash parser module (feature `code-bash`).
#[cfg(feature = "code-bash")]
mod bash_parser;

/// Tree-sitter TypeScript parser module (feature `code-typescript`).
#[cfg(feature = "code-typescript")]
mod typescript_parser;

/// Parses a Rust file and returns the list of extracted symbols.
///
/// ## Visibility
///
/// The `include_private` parameter controls the visibility filter:
/// - `false` (default): only `pub` or `pub(crate)` items are extracted.
///   Modules are always extracted regardless of this parameter (they structure the namespace).
///   Historical behavior preserved.
/// - `true`: all items are extracted, including private ones.
///
/// ## Accuracy > coverage
///
/// When in doubt (symbol not parseable, corrupted source) → the symbol is omitted rather
/// than returned with a potentially incorrect signature. A file that cannot be parsed at all
/// returns `Ok(Vec::new())` (no error — the file is silently ignored).
///
/// ## Procedural macros
///
/// `#[derive]` and procedural macros are invisible to tree-sitter → omitted.
/// Documented behavior, not a bug.
///
/// ## Ambiguous duplicates
///
/// Same `(path, kind, qualified_name)`: overloads or opaque macros → the symbol is
/// marked `ambiguous=true` and filtered out by `build_derived_notes`.
///
/// ## Feature `code-rust`
///
/// This function is only available when the `code-rust` feature is enabled.
/// Without the feature, construct `DerivedSymbol` instances manually via the public struct.
///
/// ## Side effects
///
/// None. Pure function (read-only, no network I/O, no DB).
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

/// Parses a Python file and returns the list of extracted symbols.
///
/// ## Visibility
///
/// The `include_private` parameter controls the visibility filter:
/// - `false` (default): only public items are extracted (no `_`-prefix).
///   Dunder items (`__init__`, `__str__`) are always public (Python protocol API).
/// - `true`: all items are extracted, including `_`-prefixed ones.
///
/// ## Accuracy > coverage
///
/// When in doubt (symbol not parseable, corrupted source) → the symbol is omitted rather
/// than returned with a potentially incorrect extraction. A file that cannot be parsed at all
/// returns `Ok(Vec::new())` (file silently ignored).
///
/// ## Extracted entities
///
/// - Top-level functions → `kind = "fn"`
/// - Top-level classes → `kind = "class"`
/// - Methods inside classes → `kind = "method"`, `qualified_name = "ClassName::method_name"`
///
/// ## Not extracted (by design)
///
/// - Module-level assignments (`CONSTANT = 42`): no suitable kind
/// - Deps (call graph): `deps = vec![]` — call graph extraction not yet implemented
///
/// ## Feature `code-python`
///
/// This function is only available when the `code-python` feature is enabled.
///
/// ## Side effects
///
/// None. Pure function (read-only, no network I/O, no DB).
#[cfg(feature = "code-python")]
pub fn parse_python_file(
    source_path: &str,
    content: &str,
    include_private: bool,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let parser = python_parser::PythonParser { include_private };
    language_parser::parse_with_language_parser(&parser, source_path, content)
}

/// Parses a Bash file and returns the list of extracted symbols.
///
/// ## Extracted entities
///
/// - Top-level functions (`function_definition`) → `kind = "fn"`
/// - Top-level assignments (`variable_assignment`) → `kind = "const"` (best-effort)
///
/// ## Visibility
///
/// Bash has no syntactic visibility modifier — all symbols are `"pub"`.
/// This parser takes no `include_private` parameter (the concept does not apply).
///
/// ## Signature
///
/// `signature = None` by design: Bash does not declare typed parameters.
/// Positional parameters (`$1`, `$2`) are not syntactically declared.
///
/// ## Deps
///
/// `deps = vec![]` — callee extraction is deferred (accuracy > coverage).
///
/// ## Feature `code-bash`
///
/// This function is only available when the `code-bash` feature is enabled.
///
/// ## Side effects
///
/// None. Pure function (read-only, no network I/O, no DB).
#[cfg(feature = "code-bash")]
pub fn parse_bash_file(
    source_path: &str,
    content: &str,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let parser = bash_parser::BashParser;
    language_parser::parse_with_language_parser(&parser, source_path, content)
}

/// Parses a TypeScript file and returns the list of extracted symbols.
///
/// ## Extracted entities
///
/// - Functions (`function_declaration`) → `kind = "fn"`
/// - Classes (`class_declaration`) → `kind = "class"`
/// - Interfaces (`interface_declaration`) → `kind = "type"`
/// - Class methods (`method_definition`) → `kind = "method"`,
///   `qualified_name = "ClassName::methodName"`
/// - Top-level arrow functions (`const f = () => {}`) → `kind = "fn"`
///
/// ## Visibility
///
/// - Presence of a parent `export_statement` → `"pub"`
/// - Absence → `"priv"` (module-local)
/// - Inside a class, `public`/`private`/`protected` modifiers are respected.
///   Missing modifier → `"pub"` (TypeScript default visibility = public).
///
/// ## `.tsx` variant
///
/// Use [`parse_tsx_file`] for `.tsx` files (JSX grammar `LANGUAGE_TSX`).
/// This function uses `LANGUAGE_TYPESCRIPT` — JSX fragments are not parsed.
///
/// ## Feature `code-typescript`
///
/// This function is only available when the `code-typescript` feature is enabled.
///
/// ## Side effects
///
/// None. Pure function (read-only, no network I/O, no DB).
#[cfg(feature = "code-typescript")]
pub fn parse_typescript_file(
    source_path: &str,
    content: &str,
    include_private: bool,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let parser = typescript_parser::TypeScriptParser::ts(include_private);
    language_parser::parse_with_language_parser(&parser, source_path, content)
}

/// Parses a TypeScript JSX (`.tsx`) file and returns the list of extracted symbols.
///
/// Uses the `LANGUAGE_TSX` grammar from `tree-sitter-typescript 0.23.2`, which includes
/// JSX/React nodes (`jsx_element`, `jsx_opening_element`, etc.) in addition to TypeScript.
///
/// ## Extracted entities
///
/// Identical to [`parse_typescript_file`] (same symbol extraction):
/// - Functions / React components (`function_declaration`) → `kind = "fn"`
/// - Classes (`class_declaration`) → `kind = "class"`
/// - Interfaces (`interface_declaration`) → `kind = "type"`
/// - Class methods (`method_definition`) → `kind = "method"`
/// - Top-level arrow functions (`const f = () => {}`) → `kind = "fn"`
///
/// JSX nodes (elements `<div>`, `<Button>`, etc.) are parsed correctly
/// but **do not produce symbols** — only TypeScript declarations are extracted.
///
/// ## Visibility
///
/// - `export` → `"pub"`; absent → `"priv"`.
///
/// ## Feature `code-typescript`
///
/// This function is only available when the `code-typescript` feature is enabled.
///
/// ## Side effects
///
/// None. Pure function (read-only, no network I/O, no DB).
///
/// # Errors
///
/// Returns [`IngestError::ParseError`] if the `LANGUAGE_TSX` grammar is ABI-incompatible
/// with the tree-sitter core (detected on the first call, not silenced).
#[cfg(feature = "code-typescript")]
pub fn parse_tsx_file(
    source_path: &str,
    content: &str,
    include_private: bool,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let parser = typescript_parser::TypeScriptParser::tsx(include_private);
    language_parser::parse_with_language_parser(&parser, source_path, content)
}

/// Builds `DerivedNote` instances from the extracted symbols.
///
/// ## Deterministic key
///
/// `note_id = NoteId::derived_from(vault_id ‖ '\x1f' ‖ source_path ‖ '\x1f' ‖ kind ‖ '\x1f' ‖ qualified_name)`
///
/// Guarantees idempotence: identical rebuild regardless of insertion order.
///
/// ## Filtering
///
/// - Symbols with `ambiguous=true` → omitted (accuracy > coverage).
/// - Duplicate `(kind, qualified_name)` pairs after filtering → the first one is kept.
///
/// ## Body text
///
/// Constructed as: `kind qualified_name\n[signature]\n[doc_comment]\ndeps: ...`
/// Hard cap at ≤ 60 lines.
///
/// ## Tags
///
/// `"code <lang> <kind> <module_name>"` where `lang` is inferred from the source file extension
/// (`rs`→`rust`, `py`→`python`, `sh`/`bash`→`bash`, `ts`/`tsx`→`typescript`, otherwise `unknown`).
/// `module_name` = the component before `::`, or `"root"` if absent.
///
/// ## Side effects
///
/// None. Pure function.
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

/// Infers the programming language from a source file path extension.
///
/// Used by `build_derived_notes` to tag code notes with the correct language.
fn lang_from_path(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "rs" => "rust",
        "py" => "python",
        "sh" | "bash" => "bash",
        "ts" | "tsx" => "typescript",
        _ => "unknown",
    }
}

/// Computes the hex-encoded SHA-256 hash of a file's byte content.
///
/// Used as `content_hash_source` for idempotency (skip if unchanged).
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
