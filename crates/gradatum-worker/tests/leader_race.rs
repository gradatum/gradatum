//! Tests d'élection leader — race condition 3 workers + expiry/takeover.
//!
//! Ces tests vérifient :
//! - Exactement un leader élu parmi N workers concurrents.
//! - Un nouveau worker peut prendre le leadership après expiry du précédent.

use std::time::Duration;

use gradatum_db_sqlite::open_queue_db;
use gradatum_worker::leader::{LeaderConfig, LeaderElection};

/// Ouvre (ou crée) une base SQLite en mode WAL et applique le schéma P2.0b.
async fn make_db(path: &std::path::Path) -> gradatum_db_sqlite::QueueDb {
    let db = open_queue_db(path).await.unwrap();
    db.with_conn(|conn| conn.execute_batch(gradatum_queue::schema::SCHEMA_V1))
        .await
        .unwrap();
    db
}

/// 3 workers concurrents : exactement 1 doit gagner le leadership.
#[tokio::test]
async fn three_workers_one_leader() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = make_db(tmp.path()).await;

    let cfg = LeaderConfig {
        renew_every: Duration::from_millis(200),
        expires_after: Duration::from_millis(600),
    };

    let mut els = vec![];
    for _ in 0..3 {
        let d = db.clone();
        let c = cfg.clone();
        els.push(LeaderElection::new(d, c).await.unwrap());
    }

    // Lancement concurrent de 3 try_acquire
    let mut handles = vec![];
    for el in els.iter() {
        let el = el.clone();
        handles.push(tokio::spawn(async move { el.try_acquire().await }));
    }

    let acquired: Vec<bool> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap().unwrap())
        .collect();

    let count_leaders = acquired.iter().filter(|&&b| b).count();
    assert_eq!(count_leaders, 1, "exactement un leader doit être élu");
}

/// Après expiry du leader sans renewal, un nouveau worker doit pouvoir acquérir.
#[tokio::test]
async fn leader_expires_new_acquires() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = make_db(tmp.path()).await;
    let cfg = LeaderConfig {
        renew_every: Duration::from_millis(200),
        expires_after: Duration::from_millis(300),
    };

    let el1 = LeaderElection::new(db.clone(), cfg.clone()).await.unwrap();
    assert!(
        el1.try_acquire().await.unwrap(),
        "premier leader doit acquérir"
    );

    // Attente au-delà de l'expiry sans renouvellement
    tokio::time::sleep(Duration::from_millis(400)).await;

    let el2 = LeaderElection::new(db.clone(), cfg).await.unwrap();
    assert!(
        el2.try_acquire().await.unwrap(),
        "le lease expiré doit permettre la prise de leadership"
    );
}
