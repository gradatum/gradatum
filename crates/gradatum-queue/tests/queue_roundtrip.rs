//! Tests d'intégration TDD pour SqliteQueue (sqlx-based, P2.0b).
//!
//! Ces tests vérifient :
//! - Le cycle complet enqueue → lease → complete (test 1)
//! - La récupération automatique d'une lease expirée (test 2)

use gradatum_queue::{NewJob, Queue, SqliteQueue};
use std::time::Duration;

/// Cycle complet : enqueue → lease → complete → plus de re-lease.
#[tokio::test]
async fn enqueue_lease_complete_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let q = SqliteQueue::new(tmp.path()).await.unwrap();

    let job_id = q
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "curate".into(),
            payload: b"hello".to_vec(),
            max_attempts: 3,
        })
        .await
        .unwrap();

    let leased = q.lease(&["curate"], Duration::from_secs(30)).await.unwrap();
    assert!(leased.is_some());
    let lj = leased.unwrap();
    assert_eq!(lj.id, job_id);
    assert_eq!(lj.payload, b"hello");

    q.complete(lj.id).await.unwrap();

    let leased2 = q.lease(&["curate"], Duration::from_secs(30)).await.unwrap();
    assert!(leased2.is_none(), "completed job should not re-lease");
}

/// Une lease expirée rend le job re-claimable.
#[tokio::test]
async fn lease_expires_returns_to_pending() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let q = SqliteQueue::new(tmp.path()).await.unwrap();
    let job_id = q
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "curate".into(),
            payload: vec![],
            max_attempts: 3,
        })
        .await
        .unwrap();

    let _lj = q
        .lease(&["curate"], Duration::from_millis(100))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let leased2 = q.lease(&["curate"], Duration::from_secs(30)).await.unwrap();
    assert!(leased2.is_some(), "expired lease must re-lease");
    assert_eq!(leased2.unwrap().id, job_id);
}
