//! Wikilink extraction from Markdown body text.
//!
//! ## Supported formats
//!
//! - `[[target]]`: plain link with no alias.
//! - `[[target|alias]]`: link with a display alias.
//!
//! ## Performance
//!
//! The `Regex` is compiled once via `once_cell::sync::Lazy`, avoiding
//! recompilation on each call to [`wikilinks`].

use once_cell::sync::Lazy;
use regex::Regex;

/// A wikilink extracted from a note body.
///
/// Source format: `[[target]]` or `[[target|alias]]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Wikilink {
    /// Link target — a ULID note ID or a slug.
    ///
    /// Example: `"01HQK0000000000000"` or `"MyNote"`.
    pub target: String,

    /// Optional display alias.
    ///
    /// `Some("MyNote")` for `[[01HQK|MyNote]]`, `None` for `[[abc]]`.
    pub alias: Option<String>,
}

/// Regex compiled once — thread-safe via `Lazy`.
///
/// Pattern: `\[\[([^\]|]+)(?:\|([^\]]+))?\]\]`
/// - Group 1: target (any character except `]` and `|`)
/// - Group 2 (optional): alias after `|`
static WIKILINK_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[\[([^\]|]+)(?:\|([^\]]+))?\]\]")
        .expect("Pattern wikilink est valide et statiquement correct")
});

/// Extracts all wikilinks from the `body` text.
///
/// Returns a `Vec<Wikilink>` in order of appearance.
/// Returns `vec![]` if no wikilinks are found.
///
/// ## Example
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
