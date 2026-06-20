//! Tests de la sémantique de lease : expiration et blocage.
//!
//! Vérifie que :
//! - Une lease expirée rend le job re-claimable avec `attempts++`.
//! - Une lease active bloque tout nouveau claim.

// Rétrocompatibilité P2.0b : LegacyQueue préserve l'API rusqlite Phase 1.
use gradatum_queue::LegacyQueue as Queue;

/// Un job dont la lease a expiré peut être reclaim.
///
/// `attempts` est incrémenté à chaque claim (y compris les re-claims).
#[tokio::test]
async fn expired_lease_can_be_reclaimed() {
    let q = Queue::open_in_memory().await.unwrap();
    let _ = q.enqueue("test", "{}").await.unwrap();

    // Premier claim avec une lease de 100ms.
    let job1 = q
        .claim_one(100)
        .await
        .unwrap()
        .expect("claim 1 doit réussir");
    assert_eq!(job1.attempts, 1);

    // Attente de l'expiration de la lease.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Deuxième claim — la lease est expirée, le job est de nouveau disponible.
    let job2 = q
        .claim_one(60_000)
        .await
        .unwrap()
        .expect("claim 2 doit réussir après expiration de la lease");
    assert_eq!(job2.id, job1.id, "doit s'agir du même job");
    assert_eq!(
        job2.attempts, 2,
        "attempts doit être incrémenté au re-claim"
    );
}

/// Une lease active bloque tout nouveau claim sur le même job.
#[tokio::test]
async fn active_lease_blocks_reclaim() {
    let q = Queue::open_in_memory().await.unwrap();
    let _ = q.enqueue("test", "{}").await.unwrap();

    // Claim avec une lease longue (60s).
    let _job1 = q
        .claim_one(60_000)
        .await
        .unwrap()
        .expect("claim 1 doit réussir");

    // Tentative de re-claim immédiat — doit retourner None.
    assert!(
        q.claim_one(60_000).await.unwrap().is_none(),
        "lease active doit bloquer tout re-claim immédiat",
    );
}
