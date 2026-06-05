//! Extraction de wikilinks depuis le corps Markdown.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §5.1.
//!
//! ## Format supporté
//!
//! - `[[target]]` : lien simple sans alias.
//! - `[[target|alias]]` : lien avec alias d'affichage.
//!
//! ## Performance
//!
//! Le `Regex` est compilé une seule fois via `once_cell::sync::Lazy` — pas de
//! recompilation à chaque appel à [`wikilinks`].

use once_cell::sync::Lazy;
use regex::Regex;

/// Représentation d'un wikilink extrait du corps d'une note.
///
/// Format source : `[[target]]` ou `[[target|alias]]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Wikilink {
    /// Cible du lien — NoteId ULID ou slug.
    ///
    /// Ex. `"01HQK0000000000000"` ou `"MyNote"`.
    pub target: String,

    /// Alias d'affichage optionnel.
    ///
    /// `Some("MyNote")` si `[[01HQK|MyNote]]`, `None` pour `[[abc]]`.
    pub alias: Option<String>,
}

/// Regex compilée une seule fois — thread-safe via `Lazy`.
///
/// Pattern : `\[\[([^\]|]+)(?:\|([^\]]+))?\]\]`
/// - Groupe 1 : target (tout sauf `]` et `|`)
/// - Groupe 2 (optionnel) : alias après `|`
static WIKILINK_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[\[([^\]|]+)(?:\|([^\]]+))?\]\]")
        .expect("Pattern wikilink est valide et statiquement correct")
});

/// Extrait tous les wikilinks présents dans le texte `body`.
///
/// Retourne un `Vec<Wikilink>` dans l'ordre d'apparition.
/// Retourne `vec![]` si aucun wikilink n'est trouvé.
///
/// ## Exemple
///
/// ```
/// use gradatum_markdown::wikilinks;
///
/// let links = wikilinks("[[abc]] and [[def|alias]]");
/// assert_eq!(links.len(), 2);
/// assert_eq!(links[0].target, "abc");
/// assert_eq!(links[1].alias.as_deref(), Some("alias"));
/// ```
pub fn wikilinks(body: &str) -> Vec<Wikilink> {
    WIKILINK_RE
        .captures_iter(body)
        .map(|cap| Wikilink {
            // Groupe 1 toujours présent par construction du pattern.
            target: cap[1].to_owned(),
            // Groupe 2 absent → None.
            alias: cap.get(2).map(|m| m.as_str().to_owned()),
        })
        .collect()
}
