//! Représentation de référence canonique (`Stub`) pour les notes déjà inlinées.
//!
//! Un [`Stub`] est la forme compacte et byte-stable d'une note dont le corps complet
//! a déjà été envoyé au client dans un tour précédent. Il permet de référencer la note
//! sans reconsommer son budget tokens, tout en restant déréférençable via `vault_read`.
//!
//! ## Cache stability constraints (Global Constraints 1-2)
//!
//! - Aucun champ volatil (score, timestamp, `retrieved_at`) — tout champ volatil = cache bust.
//! - Ordre des champs fixe dans [`render_stub`] — byte-identique pour le même [`Stub`].
//! - Snippet figé à la construction (jamais ré-extrait depuis le corps).

use crate::context::select::Selected;

/// Représentation compacte et byte-stable d'une note déjà envoyée au client.
///
/// Contient uniquement les champs stables : ULID, titre, section, extrait figé.
/// Le score et le timestamp sont **exclus** (volatils → cache bust potentiel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    /// ULID de la note (identifiant stable pour déréférencement via `vault_read`).
    pub ulid: String,
    /// Titre de la note.
    pub title: String,
    /// Section thématique (ex. `"decisions"`, `"reference"`).
    pub section: String,
    /// Extrait figé du corps (tronqué char-safe, sans newline).
    pub snippet: String,
}

/// Rend un [`Stub`] en format compact déterministe, byte-identique pour le même stub.
///
/// Format : `@ref <ulid> · <title> · §<section> — <snippet>`
///
/// Les champs sont en ordre fixe. Aucun champ volatil (score, date) n'est inclus.
/// La fonction est pure et déterministe : mêmes entrées → sortie identique au bit près.
#[must_use]
pub fn render_stub(s: &Stub) -> String {
    format!(
        "@ref {} · {} · §{} — {}",
        s.ulid, s.title, s.section, s.snippet
    )
}

/// Construit un [`Stub`] depuis une [`Selected`], avec snippet tronqué char-safe.
///
/// ## Troncature
///
/// Le snippet est extrait du corps (`sel.body`) en deux étapes :
/// 1. Remplacement de tous les `\n` par des espaces (stub mono-ligne).
/// 2. Troncature à `snippet_max_chars` **codepoints** via `.char_indices().nth(n)` —
///    la frontière de coupe est toujours un byte-offset valide, jamais au milieu
///    d'un codepoint multibyte UTF-8.
///
/// Cas `snippet_max_chars = 0` : `.nth(0)` retourne le premier codepoint à byte-offset 0,
/// le slice `[..0]` produit `""` — identique à un body vide.
///
/// The score and date from [`Selected`] are **ignored** (cache stability constraint).
#[must_use]
pub fn stub_from_selected(sel: &Selected, snippet_max_chars: usize) -> Stub {
    // Remplacer les newlines par des espaces pour garder le snippet sur une seule ligne.
    let body_flat = sel.body.replace('\n', " ");

    // Troncature char-safe : `.char_indices().nth(n)` donne le byte-offset du (n+1)-ème
    // codepoint, garantissant une coupure sur une frontière UTF-8 valide.
    // Si le body est plus court que snippet_max_chars → None → on prend body_flat entier.
    let snippet = match body_flat.char_indices().nth(snippet_max_chars) {
        Some((byte_idx, _)) => body_flat[..byte_idx].to_string(),
        None => body_flat,
    };

    Stub {
        ulid: sel.note_id.clone(),
        title: sel.title.clone(),
        section: sel.section.clone(),
        snippet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::select::Selected;

    fn make_selected(body: &str) -> Selected {
        Selected {
            note_id: "01JXABCDE12345678901234567".to_string(),
            title: "Titre test".to_string(),
            section: "decisions".to_string(),
            date: "2026-06-27T12:00:00+00:00".to_string(),
            score: 0.042,
            body: body.to_string(),
        }
    }

    /// Même Stub rendu deux fois → bytes identiques (contrainte cache 1).
    #[test]
    fn stub_render_is_byte_stable() {
        let stub = Stub {
            ulid: "01JXABCDE12345678901234567".to_string(),
            title: "Ma note".to_string(),
            section: "decisions".to_string(),
            snippet: "Un extrait concis".to_string(),
        };
        let render1 = render_stub(&stub);
        let render2 = render_stub(&stub);
        assert_eq!(
            render1, render2,
            "render_stub doit être byte-identique pour le même Stub"
        );
        // Vérification du format exact (ordre de champs fixe).
        assert_eq!(
            render1,
            "@ref 01JXABCDE12345678901234567 · Ma note · §decisions — Un extrait concis"
        );
    }

    /// Le rendu ne contient ni le score ni la date de Selected (contrainte cache 1).
    #[test]
    fn stub_excludes_score_and_timestamp() {
        // Construit via stub_from_selected pour vérifier que le score et la date
        // de Selected (score=0.042, date="2026-06-27T12:00:00+00:00") sont exclus.
        let sel = make_selected("Texte extrait sans champ volatil");
        let stub = stub_from_selected(&sel, 100);
        let rendered = render_stub(&stub);

        // Score 0.042 de Selected ne doit pas apparaître dans le rendu.
        assert!(
            !rendered.contains("0.042"),
            "render_stub ne doit pas contenir le score de Selected : {rendered}"
        );
        // Date de Selected ne doit pas apparaître.
        assert!(
            !rendered.contains("2026-06-27"),
            "render_stub ne doit pas contenir la date de Selected : {rendered}"
        );
        assert!(
            !rendered.contains("T12:00"),
            "render_stub ne doit pas contenir le timestamp de Selected : {rendered}"
        );
    }

    /// Snippet borné + troncature char-safe + remplacement newline.
    #[test]
    fn stub_snippet_bounded() {
        // Body ASCII long → snippet tronqué à exactement snippet_max_chars codepoints.
        let stub = stub_from_selected(&make_selected(&"a".repeat(200)), 50);
        assert_eq!(
            stub.snippet.chars().count(),
            50,
            "snippet ASCII : doit être exactement 50 codepoints"
        );
        assert!(
            !stub.snippet.contains('\n'),
            "snippet ne doit pas contenir de newline"
        );

        // Body multibyte : "café 日本語" (9 codepoints).
        // Tronqué à 5 → "café " (5 codepoints, frontière UTF-8 valide, pas de coupe au milieu de '日').
        let stub_mb = stub_from_selected(&make_selected("café 日本語"), 5);
        assert_eq!(
            stub_mb.snippet.chars().count(),
            5,
            "snippet multibyte : doit être exactement 5 codepoints"
        );
        assert_eq!(
            stub_mb.snippet, "café ",
            "troncature multibyte attendue : 'café '"
        );

        // Body avec newline → remplacé par espace dans le snippet.
        let stub_nl = stub_from_selected(&make_selected("première ligne\ndeuxième ligne"), 100);
        assert!(
            !stub_nl.snippet.contains('\n'),
            "newline doit être remplacé par espace : {:?}",
            stub_nl.snippet
        );
        assert!(
            stub_nl.snippet.contains(" deuxième"),
            "espace attendu après remplacement newline : {:?}",
            stub_nl.snippet
        );

        // Body plus court que snippet_max_chars → pas de troncature, pas de panic.
        let stub_short = stub_from_selected(&make_selected("court"), 100);
        assert_eq!(
            stub_short.snippet, "court",
            "body court : pas de troncature"
        );

        // snippet_max_chars = 0 → snippet vide.
        let stub_zero = stub_from_selected(&make_selected("texte quelconque"), 0);
        assert!(
            stub_zero.snippet.is_empty(),
            "snippet_max_chars=0 → snippet vide"
        );
    }
}
