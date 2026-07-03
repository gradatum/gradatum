//! Rendu du contexte LLM — jointure des parties de notes.
//!
//! [`render_raw`] is the entry point for Raw mode, reproducing
//! exactly the `context_parts.join("\n\n---\n\n")` from the legacy handler (`logic.rs:1086`).
//! [`render_assembled`] is the entry point for Assembled mode:
//! structured Markdown rendering with header, score, and source per note.

use super::reference::{Stub, render_stub};
use super::select::Selected;

/// Séparateur canonique entre les blocs de notes dans le contexte assemblé.
///
/// Valeur : `"\n\n---\n\n"` — identique au legacy `logic.rs:1086`.
pub const PART_SEPARATOR: &str = "\n\n---\n\n";

/// Assemble les parties de texte en contexte LLM par jointure avec [`PART_SEPARATOR`].
///
/// # Parité legacy (mode Raw)
///
/// Reproduit fidèlement `context_parts.join("\n\n---\n\n")` (`logic.rs:1086`) :
/// - `parts` vide → chaîne vide.
/// - `parts` avec un seul élément → la partie seule (pas de séparateur).
/// - Plusieurs parties → jointes par `"\n\n---\n\n"` sans transformation supplémentaire.
///
/// # Panics
///
/// Ne panique pas.
pub fn render_raw(parts: Vec<String>) -> String {
    parts.join(PART_SEPARATOR)
}

/// Assemble les notes sélectionnées en un bloc Markdown structuré pour injection LLM,
/// followed by an optional `## References` block listing the stubs.
///
/// ## Inline block format
///
/// ```text
/// ### <titre> · <section> · <date ISO> · score=<X.XX>
/// <corps>
///
/// — source: [[<ULID>]]
/// ```
///
/// ## References block (conditional)
///
/// Si `stubs` est non vide, un bloc `## References` est ajouté après le contenu inline.
/// Chaque stub est rendu via [`render_stub`] sur une ligne dédiée.
/// L'ordre des stubs est préservé tel que reçu (ULID-stable depuis `select_budget_aware`).
/// Les stubs n'ont pas de score — ils ne sont **pas** re-triés par score ici.
///
/// ## Tiebreaker ULID (P1-3 BLOQUANT cache)
///
/// Le tri interne sur les scores `f64` utilise `.then_with(|| a.note_id.cmp(&b.note_id))`
/// comme tiebreaker secondaire. Sans ce tiebreaker, les scores ex-aequo (ex. tous à
/// `rrf=1.0` sur ULID-direct) produisent un ordre non-déterministe via `sort_unstable_by`,
/// causant un cache bust systématique. Le tiebreaker garantit un ordre byte-stable.
///
/// # Paramètres
///
/// - `query` : la requête originale — affichée dans l'en-tête pour la traçabilité.
/// - `notes` : les notes retenues par [`super::select::select_budget_aware`].
///   L'ordre d'entrée est ignoré : la fonction garantit le tri score↓ + ULID↑.
/// - `stubs` : les notes hors-budget inline, sous forme compacte (F-29).
///   Order preserved (ULID-stable from `select`). `&[]` → backward-compatible (no References block).
///
/// # Panics
///
/// Ne panique pas.
pub fn render_assembled(query: &str, notes: &[Selected], stubs: &[Stub]) -> String {
    if notes.is_empty() {
        return String::new();
    }

    // Tri décroissant par score + tiebreaker ULID (P1-3 BLOQUANT cache).
    // `partial_cmp` est sûr ici : les scores issus de `composite_score_weighted` sont
    // des f64 finies (NaN impossible avec les inputs valides).
    // Tiebreaker `.then_with(|| a.note_id.cmp(&b.note_id))` : pour les scores ex-aequo,
    // l'ULID lexicalement plus petit passe en premier → ordre byte-stable reproductible.
    let mut sorted: Vec<&Selected> = notes.iter().collect();
    sorted.sort_unstable_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.note_id.cmp(&b.note_id))
    });

    let n = sorted.len();
    let suffix = if n == 1 { "" } else { "s" };
    let header = format!("Contexte assemblé pour : «{query}» · {n} note{suffix}");

    // Construction des blocs par note — format spec §2.3.
    let blocks: Vec<String> = sorted
        .iter()
        .map(|note| {
            format!(
                "### {} · {} · {} · score={:.2}\n{}\n\n— source: [[{}]]",
                note.title, note.section, note.date, note.score, note.body, note.note_id,
            )
        })
        .collect();

    let inline_block = format!("{header}\n\n{}", blocks.join(PART_SEPARATOR));

    // Bloc References conditionnel (F-29, Task 3) :
    // - seulement si stubs non vide
    // - ordre ULID-stable préservé de `select_budget_aware` (NE PAS re-trier par score)
    if stubs.is_empty() {
        inline_block
    } else {
        format!("{inline_block}{}", render_references_block(stubs))
    }
}

/// Rend le bloc `## References` pour une liste de stubs (F-29).
///
/// Retourne une chaîne vide si `stubs` est vide.
/// Sinon retourne `"\n\n## References\n\n<stub1>\n<stub2>..."` —
/// prêt à être appendé à un bloc inline via `String::push_str`.
///
/// Séparer ce rendu du rendu inline permet à l'appelant (ex. `assemble_assembled`)
/// de calculer `budget_used` sur la seule portion inline + skills **avant** d'ajouter
/// les références, qui sont des pointeurs compacts non imputés au budget inline.
///
/// L'ordre des stubs est préservé tel que reçu (ULID-stable depuis `select_budget_aware`).
///
/// # Panics
///
/// Ne panique pas.
#[must_use]
pub fn render_references_block(stubs: &[Stub]) -> String {
    if stubs.is_empty() {
        return String::new();
    }
    let refs_lines: Vec<String> = stubs.iter().map(render_stub).collect();
    format!("\n\n## References\n\n{}", refs_lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_raw_empty_returns_empty_string() {
        assert_eq!(render_raw(vec![]), "");
    }

    #[test]
    fn render_raw_single_part_no_separator_added() {
        assert_eq!(render_raw(vec!["bonjour".to_string()]), "bonjour");
    }

    #[test]
    fn render_raw_two_parts_joined_with_canonical_separator() {
        let result = render_raw(vec!["partA".to_string(), "partB".to_string()]);
        assert_eq!(result, "partA\n\n---\n\npartB");
    }

    #[test]
    fn render_raw_three_parts_two_separators() {
        let result = render_raw(vec!["A".to_string(), "B".to_string(), "C".to_string()]);
        assert_eq!(result, "A\n\n---\n\nB\n\n---\n\nC");
    }

    #[test]
    fn separator_constant_matches_legacy_value() {
        // Contrainte de parité : le séparateur NE doit PAS changer sans mise à jour
        // des tests de parité (context_raw_parity.rs).
        assert_eq!(PART_SEPARATOR, "\n\n---\n\n");
    }

    // ── Tests render_assembled ───────────────────────────────────────────────

    /// Snapshot of the structured Markdown format — freezes the output.
    ///
    /// Tout changement de format → snapshot failure → révision intentionnelle obligatoire.
    #[test]
    fn render_snapshot_structure() {
        let notes = vec![Selected {
            note_id: "01ABC".into(),
            title: "Titre A".into(),
            section: "decisions".into(),
            date: "2026-06-26".into(),
            score: 1.234,
            body: "corps A".into(),
        }];
        let out = render_assembled("ma query", &notes, &[]);
        insta::assert_snapshot!(out);
    }

    /// Notes vides → chaîne vide (early return propre).
    #[test]
    fn render_assembled_empty_notes_returns_empty() {
        assert_eq!(render_assembled("query vide", &[], &[]), "");
    }

    /// Deux notes → séparateur `---` présent + tri score décroissant respecté.
    #[test]
    fn render_assembled_two_notes_ordered_by_score_descending() {
        let notes = vec![
            Selected {
                note_id: "01LOW".into(),
                title: "Basse priorité".into(),
                section: "reference".into(),
                date: "2026-01-01".into(),
                score: 0.5,
                body: "corps basse prio".into(),
            },
            Selected {
                note_id: "01HIGH".into(),
                title: "Haute priorité".into(),
                section: "decisions".into(),
                date: "2026-06-01".into(),
                score: 0.9,
                body: "corps haute prio".into(),
            },
        ];
        let out = render_assembled("test tri", &notes, &[]);
        // Séparateur entre les deux notes.
        assert!(
            out.contains("\n\n---\n\n"),
            "séparateur attendu entre les deux notes"
        );
        // Haute priorité apparaît avant basse priorité.
        let pos_high = out.find("Haute priorité").expect("titre high manquant");
        let pos_low = out.find("Basse priorité").expect("titre low manquant");
        assert!(
            pos_high < pos_low,
            "high (score=0.9) doit précéder low (score=0.5)"
        );
        // Les marqueurs score= sont présents.
        assert!(out.contains("score=0.90"), "score=0.90 attendu pour high");
        assert!(out.contains("score=0.50"), "score=0.50 attendu pour low");
    }

    /// En-tête contient la requête et le compte de notes (singulier/pluriel).
    #[test]
    fn render_assembled_header_contains_query_and_count() {
        let note = Selected {
            note_id: "01XYZ".into(),
            title: "Note unique".into(),
            section: "reasoning".into(),
            date: "2026-06-26T10:00:00+00:00".into(),
            score: 0.75,
            body: "corps note".into(),
        };
        let out_one = render_assembled("recherche singleton", &[note], &[]);
        assert!(
            out_one.contains("recherche singleton"),
            "en-tête doit contenir la requête"
        );
        // Singulier pour 1 note.
        assert!(out_one.contains("1 note"), "singulier attendu pour 1 note");
        assert!(!out_one.contains("1 notes"), "pluriel erroné pour 1 note");

        let notes_two = vec![
            Selected {
                note_id: "01A".into(),
                title: "A".into(),
                section: "s".into(),
                date: "2026-06-26".into(),
                score: 1.0,
                body: "b".into(),
            },
            Selected {
                note_id: "01B".into(),
                title: "B".into(),
                section: "s".into(),
                date: "2026-06-26".into(),
                score: 0.5,
                body: "b".into(),
            },
        ];
        let out_two = render_assembled("deux notes", &notes_two, &[]);
        assert!(out_two.contains("2 notes"), "pluriel attendu pour 2 notes");
    }

    /// Le marqueur source contient le format `[[<ULID>]]`.
    #[test]
    fn render_assembled_source_marker_format() {
        let note = Selected {
            note_id: "01JKZQ9TF5H7P3X2W8NM4RYDAB".into(),
            title: "T".into(),
            section: "s".into(),
            date: "2026-06-26".into(),
            score: 0.5,
            body: "b".into(),
        };
        let out = render_assembled("q", &[note], &[]);
        assert!(
            out.contains("— source: [[01JKZQ9TF5H7P3X2W8NM4RYDAB]]"),
            "marqueur source attendu — out={out}"
        );
    }

    // ── Tests Task 3 : bloc References + tiebreaker ULID (P1-3) ────────────────

    /// Bloc inline présent + bloc « ## References » ajouté avec les stubs
    /// dans l'ordre ULID-stable reçu de `select_budget_aware`.
    ///
    /// Verifies: (1) the inline block is unchanged, (2) a `## References` block
    /// est ajouté après, (3) chaque stub est rendu via `render_stub`, (4) l'ordre
    /// ULID-stable est préservé (le stub dont l'ULID est lexicalement inférieur
    /// apparaît en premier, conforme à l'ordre de `select`).
    #[test]
    fn render_with_stubs_appends_references_block() {
        use super::super::reference::{Stub, render_stub};

        let notes = vec![Selected {
            note_id: "01AAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            title: "Note inline".into(),
            section: "decisions".into(),
            date: "2026-06-27".into(),
            score: 0.9,
            body: "corps inline".into(),
        }];

        // Stubs déjà triés ULID-stable (01BBB... < 01CCC...).
        let stubs = vec![
            Stub {
                ulid: "01BBBBBBBBBBBBBBBBBBBBBBBBB".into(),
                title: "Note stub B".into(),
                section: "reference".into(),
                snippet: "extrait B".into(),
            },
            Stub {
                ulid: "01CCCCCCCCCCCCCCCCCCCCCCCCC".into(),
                title: "Note stub C".into(),
                section: "reasoning".into(),
                snippet: "extrait C".into(),
            },
        ];

        let out = render_assembled("ma query", &notes, &stubs);

        // Bloc inline présent (F-35 inchangé).
        assert!(out.contains("Note inline"), "bloc inline attendu");
        assert!(
            out.contains("corps inline"),
            "corps de la note inline attendu"
        );

        // Bloc References présent.
        assert!(out.contains("## References"), "bloc ## References attendu");

        // Les deux stubs sont rendus via render_stub.
        let rendered_b = render_stub(&stubs[0]);
        let rendered_c = render_stub(&stubs[1]);
        assert!(
            out.contains(&rendered_b),
            "stub B attendu dans le rendu : {out}"
        );
        assert!(
            out.contains(&rendered_c),
            "stub C attendu dans le rendu : {out}"
        );

        // Ordre ULID-stable préservé : 01BBB... < 01CCC... → B avant C.
        let pos_b = out.find(&rendered_b).expect("stub B introuvable");
        let pos_c = out.find(&rendered_c).expect("stub C introuvable");
        assert!(
            pos_b < pos_c,
            "ordre ULID-stable : stub B doit précéder stub C"
        );

        // Bloc inline précède le bloc References.
        let pos_inline = out.find("Note inline").expect("note inline introuvable");
        let pos_refs = out
            .find("## References")
            .expect("## References introuvable");
        assert!(
            pos_inline < pos_refs,
            "bloc inline doit précéder le bloc ## References"
        );
    }

    /// `stubs` empty → backward-compatible behavior (no References block).
    ///
    /// Snapshot that freezes the format with `stubs=&[]`:
    /// - même SET de notes, même format
    /// - différence admise uniquement pour les ex-aequo score (ordre ULID-stable)
    /// - aucune suppression de note
    #[test]
    fn render_no_stubs_is_f35_parity() {
        let notes = vec![Selected {
            note_id: "01ABC".into(),
            title: "Titre A".into(),
            section: "decisions".into(),
            date: "2026-06-26".into(),
            score: 1.234,
            body: "corps A".into(),
        }];
        let out = render_assembled("ma query", &notes, &[]);
        // Pas de bloc References quand stubs vide.
        assert!(
            !out.contains("## References"),
            "pas de bloc References si stubs=&[]"
        );
        insta::assert_snapshot!(out);
    }

    /// Deux notes avec le même score → tiebreaker ULID garantit un ordre byte-identique.
    ///
    /// P1-3 BLOQUANT (cache) : sans tiebreaker, `sort_unstable_by` est non-déterministe
    /// sur les ex-aequo → cache bust sur RRF direct (tous à `rrf=1.0`).
    /// Avec `.then_with(|| a.note_id.cmp(&b.note_id))`, l'ordre est ULID croissant,
    /// byte-stable et reproductible.
    #[test]
    fn render_ties_ulid_byte_stable() {
        // Deux notes avec score identique = 1.0.
        // ULIDs : 01AAA... < 01ZZZ... → Note A doit apparaître avant Note Z.
        let notes = vec![
            Selected {
                note_id: "01ZZZZZZZZZZZZZZZZZZZZZZZZZ".into(),
                title: "Note Z".into(),
                section: "decisions".into(),
                date: "2026-06-27".into(),
                score: 1.0,
                body: "corps Z".into(),
            },
            Selected {
                note_id: "01AAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                title: "Note A".into(),
                section: "decisions".into(),
                date: "2026-06-27".into(),
                score: 1.0,
                body: "corps A".into(),
            },
        ];

        // Deux appels indépendants — byte-identique (déterminisme tiebreaker ULID).
        let out1 = render_assembled("query tiebreaker", &notes, &[]);
        let out2 = render_assembled("query tiebreaker", &notes, &[]);
        assert_eq!(
            out1, out2,
            "render_assembled doit être byte-identique sur 2 runs (tiebreaker ULID)"
        );

        // Ordre ULID croissant : 01AAA... < 01ZZZ... → Note A avant Note Z.
        let pos_a = out1.find("Note A").expect("Note A attendu dans le rendu");
        let pos_z = out1.find("Note Z").expect("Note Z attendu dans le rendu");
        assert!(
            pos_a < pos_z,
            "tiebreaker ULID : 01AAA... < 01ZZZ... → Note A doit précéder Note Z"
        );
    }
}
