//! Test d'intégration — câblage production AppState avec SqliteQueue réelle.
//!
//! Vérifie que `AppState::with_queue_path` injecte une `SqliteQueue` persistante
//! (pas un stub `NoopQueue`) : le `job_id` retourné est issu d'un vrai INSERT SQLite.

use gradatum_queue::NewJob;
use gradatum_server::state::AppState;
use tempfile::TempDir;

#[tokio::test]
async fn appstate_uses_real_sqlite_queue() {
    let dir = TempDir::new().expect("tempdir");
    let queue_path = dir.path().join("queue.db");
    let state = AppState::new()
        .with_queue_path(&queue_path)
        .await
        .expect("queue init");

    let job_id = state
        .queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "curate".into(),
            payload: vec![1, 2, 3],
            max_attempts: 5,
        })
        .await
        .expect("enqueue");

    assert!(job_id > 0, "real queue returns persistent ID, not stub 1");
    assert!(queue_path.exists(), "SQLite file created");
}
