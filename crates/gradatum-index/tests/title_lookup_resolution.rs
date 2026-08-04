//! v0.7.3 : `title_lookup` résolution colonne-first.
//!
//! Prouve que `title_lookup` résout d'abord la colonne `title` (exact-match)
//! avant le fallback LIKE H1. Déclencheur du fix : âme sans H1 introuvable (smoke 6′).
//!
//! # Cas
//!
//! 1. `title_lookup_resolves_by_title_column_when_populated` — colonne peuplée, pas de H1 → résout.
//! 2. `title_lookup_falls_back_to_h1_when_title_column_empty` — colonne vide, H1 présent → résout.
//! 3. `title_lookup_excludes_non_live_with_title_column` — note non-live avec colonne peuplée → None.
//! 4. `title_lookup_collision_policy_column_wins` — colonne vs H1 simultanés → colonne gagne (P1-2 council).
//! 5. `title_lookup_parity_column_equals_h1` — colonne == H1 (cas corpus dominant) → résolution identique.

mod common;
use common::make_note;

use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// Cas 1 — Colonne `title` peuplée, body SANS H1 → résolution par colonne.
///
/// Preuve de l'indépendance au H1 — c'est le fix du smoke bug F-34 (injection âme
/// échouait silencieusement quand l'âme n'avait pas de H1 en tête de body).
#[tokio::test]
async fn title_lookup_resolves_by_title_column_when_populated() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Note avec body SANS H1 — `title_lookup` H1 LIKE ne matcherait pas.
    let note = make_note(
        "main",
        Section::Identity,
        NoteStatus::Live,
        "## INVARIANTS\nINV-CANARY | REQUIRED | x\n\n## NARRATIVE\nTu es Backend.",
    );
    let note_id = note.id;
    idx.upsert_note(&note).await.expect("upsert_note");

    // Peuple la colonne `title` séparément (comme le fait vault_write via persist_curated).
    idx.upsert_note_title("main", &note_id, "identity/x")
        .await
        .expect("upsert_note_title");

    // Sans l'exact-match colonne, `title_lookup("identity/x")` retournerait None
    // (le body ne commence pas par `# identity/x`).
    let result = idx
        .title_lookup("main", "identity/x")
        .await
        .expect("title_lookup");

    assert_eq!(
        result,
        Some(note_id.to_string()),
        "title_lookup doit résoudre via la colonne `title` même sans H1 dans le body"
    );
}

/// Cas 2 — Colonne `title` vide, body avec H1 → fallback H1 (backward-compat).
///
/// Régression : la résolution H1 existante ne doit PAS être brisée.
/// Les notes corpus-wide sans `upsert_note_title` explicite sont toujours résolvables.
#[tokio::test]
async fn title_lookup_falls_back_to_h1_when_title_column_empty() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    let note = make_note(
        "main",
        Section::Decisions,
        NoteStatus::Live,
        "# foo\nCorps de la note foo.",
    );
    let note_id = note.id;
    idx.upsert_note(&note).await.expect("upsert_note");
    // Ne PAS appeler upsert_note_title — colonne reste vide.

    let result = idx.title_lookup("main", "foo").await.expect("title_lookup");

    assert_eq!(
        result,
        Some(note_id.to_string()),
        "title_lookup doit fallback sur le H1 quand la colonne `title` est vide"
    );
}

/// Cas 3 — Note non-live (Deprecated) avec colonne `title` peuplée → None.
///
/// La garde `status = 'live'` doit s'appliquer aussi sur l'exact-match colonne.
/// Une âme archivée ne doit pas être résolvable par path.
#[tokio::test]
async fn title_lookup_excludes_non_live_with_title_column() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Note avec statut non-live (Deprecated).
    let note = make_note(
        "main",
        Section::Identity,
        NoteStatus::Deprecated,
        "## INVARIANTS\nINV-CANARY | REQUIRED | x\n",
    );
    let note_id = note.id;
    idx.upsert_note(&note).await.expect("upsert_note");
    idx.upsert_note_title("main", &note_id, "identity/x")
        .await
        .expect("upsert_note_title");

    let result = idx
        .title_lookup("main", "identity/x")
        .await
        .expect("title_lookup");

    assert!(
        result.is_none(),
        "note non-live (Deprecated) avec colonne title peuplée doit retourner None"
    );
}

/// Cas 4 — Collision : colonne `title` vs H1 simultanément → colonne gagne (P1-2 council).
///
/// Note A : colonne `title='dup'`, body SANS H1.
/// Note B : colonne vide, body `# dup`.
/// Résultat attendu : note A (colonne-first).
///
/// Politique documentée : exact-match colonne prioritaire ; si absent, H1.
/// ORDER BY created DESC dans chaque requête — mais la colonne est testée en premier.
#[tokio::test]
async fn title_lookup_collision_policy_column_wins() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Note A — colonne peuplée, pas de H1.
    let note_a = make_note(
        "main",
        Section::Identity,
        NoteStatus::Live,
        "Texte sans H1 — note A.",
    );
    let id_a = note_a.id;
    idx.upsert_note(&note_a).await.expect("upsert_note note_a");
    idx.upsert_note_title("main", &id_a, "dup")
        .await
        .expect("upsert_note_title note_a");

    // Note B — pas de colonne, H1 présent.
    let note_b = make_note(
        "main",
        Section::Reference,
        NoteStatus::Live,
        "# dup\nCorps note B.",
    );
    idx.upsert_note(&note_b).await.expect("upsert_note note_b");
    // colonne reste vide pour note_b

    let result = idx.title_lookup("main", "dup").await.expect("title_lookup");

    assert_eq!(
        result,
        Some(id_a.to_string()),
        "politique collision : colonne-first doit gagner sur H1 (note A colonne vs note B H1)"
    );
}

/// Cas 5 — Parity corpus-wide : colonne `title` == H1 → résolution identique (P1-2 council).
///
/// Cas dominant du corpus : les notes créées via vault_write ont colonne == H1.
/// Aucune régression de résolution wikilink/path ne doit se produire.
#[tokio::test]
async fn title_lookup_parity_column_equals_h1() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    let note = make_note(
        "main",
        Section::Architecture,
        NoteStatus::Live,
        "# Gradatum Architecture\nCorps.",
    );
    let note_id = note.id;
    idx.upsert_note(&note).await.expect("upsert_note");

    // Peuple la colonne avec la même valeur que le H1 (cas corpus dominant).
    idx.upsert_note_title("main", &note_id, "Gradatum Architecture")
        .await
        .expect("upsert_note_title");

    let result = idx
        .title_lookup("main", "Gradatum Architecture")
        .await
        .expect("title_lookup");

    assert_eq!(
        result,
        Some(note_id.to_string()),
        "colonne == H1 : résolution identique (parity corpus-wide, P1-2)"
    );
}
