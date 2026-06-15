//! Tests d'extraction de wikilinks.
//!
//! Vérifie le comportement de [`gradatum_markdown::wikilinks`] sur les cas nominaux
//! et les cas limites.

use gradatum_markdown::wikilinks;

/// Wikilink simple sans alias.
#[test]
fn single_wikilink_no_alias() {
    let links = wikilinks("[[abc]]");
    assert_eq!(links.len(), 1, "un seul lien attendu");
    assert_eq!(links[0].target, "abc");
    assert_eq!(links[0].alias, None);
}

/// Wikilink avec alias d'affichage.
#[test]
fn wikilink_with_alias() {
    let links = wikilinks("See [[01HQK|MyNote]] for details.");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].target, "01HQK");
    assert_eq!(
        links[0].alias.as_deref(),
        Some("MyNote"),
        "l'alias doit être extrait"
    );
}

/// Plusieurs wikilinks dans le même body.
#[test]
fn multiple_wikilinks() {
    let links = wikilinks("[[a]] and [[b|B]] and [[c]]");
    assert_eq!(links.len(), 3, "trois liens attendus");
    assert_eq!(links[0].target, "a");
    assert_eq!(links[0].alias, None);
    assert_eq!(links[1].target, "b");
    assert_eq!(links[1].alias.as_deref(), Some("B"));
    assert_eq!(links[2].target, "c");
    assert_eq!(links[2].alias, None);
}

/// Corps sans wikilinks → vec vide.
#[test]
fn no_wikilinks_returns_empty() {
    assert!(
        wikilinks("plain markdown without links").is_empty(),
        "aucun lien attendu dans un texte sans wikilinks"
    );
}

/// Les wikilinks dans la fixture note-with-wikilinks.md sont extraits correctement.
#[test]
fn fixture_wikilinks_extracted() {
    let raw = include_str!("fixtures/note-with-wikilinks.md");
    // La fixture contient [[abc]], [[def|alias]], [[ghi]].
    // Le body est tout ce qui suit le second ---\n\n.
    let parsed = gradatum_markdown::parse(raw).expect("parse fixture");
    let links = wikilinks(&parsed.body.markdown);

    assert_eq!(links.len(), 3, "trois wikilinks attendus dans la fixture");
    assert_eq!(links[0].target, "abc");
    assert_eq!(links[0].alias, None);
    assert_eq!(links[1].target, "def");
    assert_eq!(links[1].alias.as_deref(), Some("alias"));
    assert_eq!(links[2].target, "ghi");
    assert_eq!(links[2].alias, None);
}

/// Wikilink avec cible ULID (26 chars base32).
#[test]
fn wikilink_ulid_target() {
    let links = wikilinks("[[01JQBK3V8EPWSMG0X6G5Z2K9A0|My Important Note]]");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].target, "01JQBK3V8EPWSMG0X6G5Z2K9A0");
    assert_eq!(links[0].alias.as_deref(), Some("My Important Note"));
}
