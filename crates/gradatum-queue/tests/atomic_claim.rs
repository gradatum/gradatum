//! Tests du claim atomique via `UPDATE…RETURNING`.
//!
//! Vérifie : enqueue→claim, queue vide, complete, fail, contention concurrente.

use std::sync::Arc;

// Rétrocompatibilité P2.0b : LegacyQueue préserve l'API rusqlite Phase 1.
use gradatum_queue::LegacyQueue as Queue;

/// Enqueue puis claim — le job retourné correspond à celui inséré.
#[tokio::test]
async fn enqueue_then_claim() {
    let q = Queue::open_in_memory().await.unwrap();
    let id = q
        .enqueue("embed_note", r#"{"note_id":"01HQK"}"#)
        .await
        .unwrap();

    let job = q
        .claim_one(60_000)
        .await
        .unwrap()
        .expect("doit retourner un job");
    assert_eq!(job.id, id);
    assert_eq!(job.kind, "embed_note");
    assert_eq!(
        job.attempts, 1,
        "attempts doit être 1 après le premier claim"
    );
}

/// Queue vide — `claim_one` retourne `None`.
#[tokio::test]
async fn empty_queue_returns_none() {
    let q = Queue::open_in_memory().await.unwrap();
    assert!(q.claim_one(60_000).await.unwrap().is_none());
}

/// `complete` marque le job `done` — il ne peut plus être reclaim.
#[tokio::test]
async fn complete_marks_done() {
    let q = Queue::open_in_memory().await.unwrap();
    let id = q.enqueue("test", "{}").await.unwrap();
    let _ = q.claim_one(60_000).await.unwrap();

    q.complete(id).await.unwrap();

    // Un second claim ne doit rien retourner (job done).
    assert!(
        q.claim_one(60_000).await.unwrap().is_none(),
        "job done ne doit pas être re-claim",
    );
}

/// `fail` marque le job `failed` avec la raison — il ne peut plus être reclaim.
#[tokio::test]
async fn fail_records_reason() {
    let q = Queue::open_in_memory().await.unwrap();
    let id = q.enqueue("test", "{}").await.unwrap();
    let _ = q.claim_one(60_000).await.unwrap();

    q.fail(id, "out of memory").await.unwrap();

    // Phase 1 : failed est terminal, pas de retry automatique.
    assert!(
        q.claim_one(60_000).await.unwrap().is_none(),
        "job failed ne doit pas être re-claim (Phase 1 — pas de retry auto)",
    );
}

/// Contention concurrente — un seul consommateur obtient le job.
///
/// Avec `tokio::sync::Mutex<Connection>`, les deux `claim_one` sont sérialisés.
/// Le premier claim vide la queue ; le second retourne `None`.
/// Le test vérifie que count == 1 quelle que soit l'ordonnance tokio.
#[tokio::test]
async fn concurrent_claim_atomic() {
    let q = Arc::new(Queue::open_in_memory().await.unwrap());
    let _ = q.enqueue("test", "{}").await.unwrap();

    let q1 = Arc::clone(&q);
    let q2 = Arc::clone(&q);

    let (a, b) = tokio::join!(q1.claim_one(60_000), q2.claim_one(60_000),);

    let count = [a.unwrap(), b.unwrap()]
        .iter()
        .filter(|j| j.is_some())
        .count();
    assert_eq!(count, 1, "exactement un consommateur doit obtenir le job");
}
