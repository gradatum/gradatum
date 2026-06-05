//! Sérialisation d'une `ParsedNote` vers la représentation Markdown on-disk.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §5.1.
//!
//! ## Format produit
//!
//! ```text
//! ---
//! <yaml frontmatter>
//! ---
//!
//! <body markdown>
//! ```
//!
//! ## Garantie de round-trip
//!
//! `parse(write_parsed(parse(x)?)) == parse(x)` (idempotence à 1 cycle).
//! L'égalité string stricte n'est pas garantie — `serde_yml` peut réordonner
//! les champs par rapport à l'original. La garantie porte sur les **valeurs**
//! après re-parse, pas sur la représentation textuelle exacte.

use crate::error::MarkdownError;
use crate::parser::ParsedNote;
use gradatum_core::note::Note;

/// Sérialise une `ParsedNote` en Markdown on-disk.
///
/// Produit le format spec §5.1 :
/// ```text
/// ---\n<yaml>\n---\n\n<body>
/// ```
///
/// `serde_yml::to_string` produit le YAML sans délimiteur `---` initial,
/// donc on l'ajoute manuellement avant et après.
///
/// ## Erreurs
///
/// Retourne `MarkdownError::Yaml` si le frontmatter n'est pas sérialisable.
/// En pratique impossible pour `Frontmatter` (pas de f32::NAN ni de types non-YAML).
pub fn write_parsed(note: &ParsedNote) -> Result<String, MarkdownError> {
    let yaml = serde_yml::to_string(&note.frontmatter).map_err(MarkdownError::Yaml)?;
    // serde_yml 0.9 produit "<yaml content>\n" sans délimiteur ---
    // On enveloppe manuellement.
    Ok(format!("---\n{}---\n\n{}", yaml, note.body.markdown))
}

/// Sérialise une `Note` complète en Markdown on-disk.
///
/// Identique à `write_parsed` mais accepte une `Note` complète.
/// Les champs `id`, `version`, `integrity_signature` sont ignorés —
/// ils ne font PAS partie de la représentation on-disk (le nom de fichier
/// porte le `NoteId`, et `version` est géré par `gradatum-vault`).
///
/// ## Erreurs
///
/// Retourne `MarkdownError::Yaml` si la sérialisation échoue.
pub fn write(note: &Note) -> Result<String, MarkdownError> {
    let parsed = ParsedNote {
        frontmatter: note.frontmatter.clone(),
        body: note.body.clone(),
        content_hash: note.content_hash,
    };
    write_parsed(&parsed)
}
