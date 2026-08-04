//! Construction de la requête de salience depuis l'activité récente.
//!
//! La `salience query` est une chaîne de texte concaténant les titres et tags des notes
//! most recently active. It is passed to the search engine to find notes conceptually
//! related to what the user has recently viewed or edited.

use gradatum_core::index::NoteRecord;

/// Borne maximale de la longueur de la requête de salience (en caractères).
///
/// Évite de construire une requête géante qui dégraderait la qualité BM25/embedding.
const MAX_SALIENCE_QUERY_CHARS: usize = 512;

/// Construit une requête de salience et la liste des ULIDs sources à exclure.
///
/// Concatène les titres et les tags des notes récentes (`recent`) en une seule chaîne
/// de texte utilisable directement comme requête de recherche. La longueur est bornée à
/// `MAX_SALIENCE_QUERY_CHARS` pour éviter les requêtes géantes qui nuisent à la qualité
/// BM25 et à la latence embedding.
///
/// # Retour
///
/// - `String` : requête de salience (peut être vide si `recent` est vide ou si tous les
///   titres/tags sont vides).
/// - `Vec<String>`: list of source note ULIDs to exclude downstream, so that a note
///   the user just edited is not re-surfaced.
///
/// # Cas limites
///
/// - Corpus vide → `("".to_string(), vec![])`.
/// - Note sans titre ni tags → ignorée dans la concaténation (son ULID est quand même exclu).
pub fn derive_salience_query(recent: &[NoteRecord]) -> (String, Vec<String>) {
    if recent.is_empty() {
        return (String::new(), vec![]);
    }

    let source_ulids: Vec<String> = recent.iter().map(|n| n.id.clone()).collect();

    let mut parts: Vec<String> = Vec::with_capacity(recent.len() * 2);
    let mut total_len = 0usize;

    'outer: for note in recent {
        // Titre de la note (si présent et non vide)
        if let Some(title) = &note.title
            && !title.is_empty()
        {
            let candidate = title.trim();
            if total_len + candidate.len() > MAX_SALIENCE_QUERY_CHARS {
                break 'outer;
            }
            total_len += candidate.len() + 1; // +1 pour l'espace séparateur
            parts.push(candidate.to_string());
        }

        // Tags de la note (espace-séparés, si présents)
        if let Some(tags_raw) = &note.tags_raw {
            for tag in tags_raw.split_whitespace() {
                if total_len + tag.len() > MAX_SALIENCE_QUERY_CHARS {
                    break 'outer;
                }
                total_len += tag.len() + 1;
                parts.push(tag.to_string());
            }
        }
    }

    let query = parts.join(" ");
    (query, source_ulids)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construit un NoteRecord minimal pour les tests.
    fn make_note(id: &str, title: Option<&str>, tags_raw: Option<&str>) -> NoteRecord {
        NoteRecord {
            id: id.to_string(),
            vault_id: "main".to_string(),
            section: "decisions".to_string(),
            locus: None,
            status: "live".to_string(),
            body_text: String::new(),
            author: None,
            tags_raw: tags_raw.map(|s| s.to_string()),
            content_hash: vec![0u8; 32],
            created: 1_000_000,
            updated: None,
            title: title.map(|s| s.to_string()),
        }
    }

    /// Corpus vide → requête vide et liste de sources vide.
    #[test]
    fn empty_corpus_returns_empty_query_and_empty_ulids() {
        let (query, ulids) = derive_salience_query(&[]);
        assert_eq!(query, "");
        assert!(ulids.is_empty());
    }

    /// Concatène titres et tags ; retourne les ULIDs sources.
    #[test]
    fn concatenates_titles_and_tags_returns_source_ulids() {
        let notes = vec![
            make_note(
                "01ULID1AAAAAAAAAAAAAAAAAA0",
                Some("Rust async patterns"),
                Some("rust async"),
            ),
            make_note(
                "01ULID2AAAAAAAAAAAAAAAAAA0",
                Some("SQLite indexes"),
                Some("sqlite perf"),
            ),
        ];

        let (query, ulids) = derive_salience_query(&notes);

        assert!(
            query.contains("Rust async patterns"),
            "le titre de la première note doit être inclus"
        );
        assert!(
            query.contains("rust"),
            "les tags de la première note doivent être inclus"
        );
        assert!(
            query.contains("SQLite indexes"),
            "le titre de la deuxième note doit être inclus"
        );
        assert_eq!(ulids.len(), 2, "les 2 ULIDs sources doivent être retournés");
        assert_eq!(ulids[0], "01ULID1AAAAAAAAAAAAAAAAAA0");
        assert_eq!(ulids[1], "01ULID2AAAAAAAAAAAAAAAAAA0");
    }

    /// Note sans titre ni tags : ULID exclu quand même, mais ne contribue pas à la query.
    #[test]
    fn note_without_title_and_tags_still_excluded_from_candidates() {
        let notes = vec![
            make_note("01NOTITLEAAAAAAAAAAAAAAAAAA", None, None),
            make_note(
                "01WITHTITLEAAAAAAAAAAAAAAA0",
                Some("Active recall"),
                Some("memory"),
            ),
        ];

        let (query, ulids) = derive_salience_query(&notes);

        assert_eq!(
            ulids.len(),
            2,
            "les 2 ULIDs doivent figurer dans l'exclusion"
        );
        assert!(
            query.contains("Active recall"),
            "le titre de la note avec titre doit être inclus"
        );
        assert!(
            !query.is_empty(),
            "la query ne doit pas être vide (une note a un titre)"
        );
    }

    /// La longueur de la query est bornée à MAX_SALIENCE_QUERY_CHARS.
    #[test]
    fn query_length_bounded_by_max_chars() {
        // 20 notes avec des titres longs
        let notes: Vec<NoteRecord> = (0..20)
            .map(|i| {
                make_note(
                    &format!("01LONGULID{:017}", i),
                    Some("A very long title that keeps growing and growing to fill up the query buffer"),
                    Some("tag1 tag2 tag3 tag4 tag5"),
                )
            })
            .collect();

        let (query, ulids) = derive_salience_query(&notes);

        assert!(
            query.len() <= MAX_SALIENCE_QUERY_CHARS,
            "la query ne doit pas dépasser {} chars, got {}",
            MAX_SALIENCE_QUERY_CHARS,
            query.len()
        );
        assert_eq!(
            ulids.len(),
            20,
            "tous les ULIDs sources doivent être retournés quel que soit le cap"
        );
    }
}
