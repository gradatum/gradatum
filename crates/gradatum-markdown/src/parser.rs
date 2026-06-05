//! Parser Markdown + frontmatter YAML pour les notes Gradatum.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §5.1.
//!
//! ## Format attendu
//!
//! ```text
//! ---
//! <yaml frontmatter>
//! ---
//!
//! <body markdown>
//! ```
//!
//! ## Design — `ParsedNote` vs `Note`
//!
//! Le parser produit un [`ParsedNote`] et non un [`gradatum_core::note::Note`] complet.
//! Raison : le fichier Markdown on-disk ne contient PAS :
//! - `NoteId` — porté par le nom de fichier (`<ulid>.md`), assigné par `gradatum-vault` (T11).
//! - `NoteVersion` — compteur monotone géré par `gradatum-vault` à chaque écriture.
//! - `IntegritySignature` — Phase 2+ uniquement.
//!
//! Le caller (typiquement `gradatum-vault`) assemble le `Note` complet en ajoutant
//! ces trois champs après l'appel à [`parse`].

use gradatum_core::{frontmatter::Frontmatter, identity::ContentHash, note::NoteBody};

use crate::error::MarkdownError;

/// Résultat du parsing d'un fichier Markdown Gradatum.
///
/// Contient les données extraites du fichier — sans les champs gérés par le vault
/// (`NoteId`, `NoteVersion`, `IntegritySignature`).
///
/// Le caller assemble le `Note` complet :
/// ```rust,ignore
/// let parsed = parse(raw)?;
/// let note = Note {
///     id: NoteId::from_filename(filename),
///     frontmatter: parsed.frontmatter,
///     body: parsed.body,
///     version: NoteVersion::initial(),
///     content_hash: parsed.content_hash,
///     integrity_signature: None,
/// };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedNote {
    /// Métadonnées canoniques désérialisées depuis le bloc YAML.
    pub frontmatter: Frontmatter,

    /// Corps Markdown — tout ce qui suit le second `---`.
    pub body: NoteBody,

    /// Hash SHA-256 JCS calculé depuis le frontmatter + body.
    ///
    /// Pré-calculé ici pour éviter une double-parse côté vault.
    /// Invariant : `content_hash == ContentHash::compute(&frontmatter, &body.markdown)`.
    pub content_hash: ContentHash,
}

/// Parse un fichier Markdown Gradatum depuis sa représentation string.
///
/// ## Algorithme
///
/// 1. Vérifie que le contenu commence par `---\n`.
/// 2. Cherche la ligne `---` fermante à partir de la position 4.
/// 3. Désérialise le bloc YAML entre les deux délimiteurs via `serde_yml`.
/// 4. Extrait le body : tout ce qui suit le `---\n` fermant.
///    Strip un leading `\n` si présent (ligne vide entre frontmatter et titre).
/// 5. Calcule le `ContentHash` via JCS RFC 8785.
///
/// ## Erreurs
///
/// - [`MarkdownError::MissingFrontmatter`] si le contenu ne commence pas par `---\n`.
/// - [`MarkdownError::UnterminatedFrontmatter`] si le délimiteur fermant est absent.
/// - [`MarkdownError::Yaml`] si le bloc YAML est invalide.
///
/// ## Exemple
///
/// ```
/// use gradatum_markdown::parse;
///
/// let raw = "---\nschema_version: 1\nvault_id: main\nsection: decisions\nstatus: live\ncreated: \"2026-05-04T11:00:00Z\"\n---\n\n# titre\n\nCorps.\n";
/// let parsed = parse(raw).unwrap();
/// assert_eq!(parsed.frontmatter.vault_id, "main");
/// ```
pub fn parse(raw: &str) -> Result<ParsedNote, MarkdownError> {
    // Étape 1 : vérifier le délimiteur ouvrant.
    if !raw.starts_with("---\n") {
        return Err(MarkdownError::MissingFrontmatter);
    }

    // Étape 2 : chercher le délimiteur fermant à partir de la position 4.
    // On cherche "\n---\n" après le premier "---\n" pour trouver la fin du bloc YAML.
    // La position de recherche commence après "---\n" (4 bytes).
    let search_start = 4;
    let close_marker = "\n---\n";

    let close_pos = raw[search_start..]
        .find(close_marker)
        .ok_or(MarkdownError::UnterminatedFrontmatter)?;

    // Position absolue du "\n" qui précède "---\n" fermant.
    let yaml_end = search_start + close_pos;

    // Étape 3 : extraire et parser le bloc YAML.
    let yaml_block = &raw[4..yaml_end];
    let frontmatter: Frontmatter = serde_yml::from_str(yaml_block).map_err(MarkdownError::Yaml)?;

    // Étape 4 : extraire le body (après le "\n---\n" fermant).
    let body_start = yaml_end + close_marker.len();
    let body_raw = raw.get(body_start..).unwrap_or("");

    // Strip un leading '\n' si présent (ligne vide conventionnelle entre frontmatter et body).
    let body_str = body_raw.strip_prefix('\n').unwrap_or(body_raw);

    let body = NoteBody {
        markdown: body_str.to_owned(),
    };

    // Étape 5 : calculer le ContentHash (JCS RFC 8785).
    let content_hash = ContentHash::compute(&frontmatter, &body.markdown);

    Ok(ParsedNote {
        frontmatter,
        body,
        content_hash,
    })
}
