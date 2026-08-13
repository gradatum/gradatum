//! Tests F-100 1.1 — cascade `delete_note_from_index` (transaction + ANN dégradé)
//! et GC one-shot `gc_orphan_ann`.
//!
//! Ces tests s'exécutent sur un `SqliteIndex::open_in_memory` : l'extension
//! sqlite-vec (`vec0`) n'est PAS enregistrée dans ce contexte, donc la table
//! `note_embeddings_ann` est absente → **mode dégradé**. C'est exactement le cas
//! LIVE 2026-07-12 (0 orphelin, table absente). Les tests prouvent que la cascade
//! et le GC tolèrent l'absence de la table sans erreur.

use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;
use ulid::Ulid;

/// GC en mode dégradé (table ANN absente) → `Ok(0)`, jamais d'erreur.
#[tokio::test]
async fn gc_orphan_ann_degraded_mode_returns_zero() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    let removed = idx
        .gc_orphan_ann("main")
        .await
        .expect("gc_orphan_ann dégradé");
    assert_eq!(
        removed, 0,
        "table ANN absente → 0 orphelin supprimé (dégradé)"
    );
}

/// GC idempotent : deux exécutions consécutives → `Ok(0)` les deux fois.
#[tokio::test]
async fn gc_orphan_ann_is_idempotent() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Seed une note vivante puis une note « orpheline » côté notes seulement.
    idx.seed_note_with_fts(&Ulid::generate().to_string(), "feedback", "note vivante")
        .await
        .expect("seed note");

    let first = idx.gc_orphan_ann("main").await.expect("gc 1");
    let second = idx.gc_orphan_ann("main").await.expect("gc 2");
    assert_eq!(first, 0, "1er run dégradé → 0");
    assert_eq!(second, 0, "2e run idempotent → 0");
}

/// `delete_note_from_index` supprime la note (notes + FTS) dans une transaction,
/// tolère l'absence de la table ANN, et est idempotent.
#[tokio::test]
async fn delete_note_from_index_removes_and_is_idempotent() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    let id = Ulid::generate().to_string();
    idx.seed_note_with_fts(&id, "decisions", "corps cherchable alpha")
        .await
        .expect("seed note");

    // La note existe avant suppression.
    assert!(
        idx.get_note("main", &id).await.expect("get_note").is_some(),
        "note doit exister avant delete"
    );

    // 1er delete → true (trouvée + supprimée), malgré la table ANN absente (dégradé).
    let deleted = idx
        .delete_note_from_index("main", &id)
        .await
        .expect("delete_note_from_index (mode dégradé ANN toléré)");
    assert!(deleted, "1er delete doit retourner true");

    // La note a disparu de l'index (cascade).
    assert!(
        idx.get_note("main", &id).await.expect("get_note").is_none(),
        "note doit avoir disparu après delete"
    );

    // 2e delete → false (idempotent, note déjà absente).
    let deleted_again = idx
        .delete_note_from_index("main", &id)
        .await
        .expect("delete idempotent");
    assert!(
        !deleted_again,
        "2e delete doit retourner false (idempotent)"
    );
}

/// `delete_note_from_index` sur une note inexistante → `Ok(false)`.
#[tokio::test]
async fn delete_note_from_index_absent_returns_false() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    let deleted = idx
        .delete_note_from_index("main", &Ulid::generate().to_string())
        .await
        .expect("delete absent");
    assert!(!deleted, "note absente → false");
}

/// F-100 P1-1 (défense en profondeur) : `list_garbage_older_than` exclut les
/// sections `PROTECTED_DELETE` des candidats Purge, tout en listant les autres.
#[tokio::test]
async fn list_garbage_older_than_excludes_protected_sections() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Une note garbage par section protégée (jamais candidate au Purge).
    let mut protected_ids = Vec::new();
    for section in Section::PROTECTED_DELETE {
        let id = Ulid::generate().to_string();
        idx.seed_note_with_status(&id, *section, "note gouvernance", NoteStatus::Garbage, None)
            .await
            .expect("seed protected garbage");
        protected_ids.push(id);
    }

    // Deux notes garbage NON protégées (candidates légitimes, non-régression GC).
    let feedback_id = Ulid::generate().to_string();
    idx.seed_note_with_status(
        &feedback_id,
        Section::Feedback,
        "note feedback jetable",
        NoteStatus::Garbage,
        None,
    )
    .await
    .expect("seed feedback garbage");
    let debug_id = Ulid::generate().to_string();
    idx.seed_note_with_status(
        &debug_id,
        Section::Debug,
        "note debug jetable",
        NoteStatus::Garbage,
        None,
    )
    .await
    .expect("seed debug garbage");

    // cutoff = i64::MAX → toutes les notes garbage qualifient par l'âge.
    let candidates = idx
        .list_garbage_older_than("main", i64::MAX)
        .await
        .expect("list_garbage_older_than");
    let candidate_ids: Vec<String> = candidates.iter().map(|n| n.to_string()).collect();

    // Aucune section protégée n'est candidate.
    for pid in &protected_ids {
        assert!(
            !candidate_ids.contains(pid),
            "une note en section protégée ne doit jamais être candidate au Purge : {pid}"
        );
    }
    // Les deux notes non protégées SONT candidates (non-régression).
    assert!(
        candidate_ids.contains(&feedback_id),
        "une note feedback garbage doit rester purgeable"
    );
    assert!(
        candidate_ids.contains(&debug_id),
        "une note debug garbage doit rester purgeable"
    );
    assert_eq!(
        candidate_ids.len(),
        2,
        "exactement 2 candidats non protégés attendus, obtenu : {candidate_ids:?}"
    );
}
