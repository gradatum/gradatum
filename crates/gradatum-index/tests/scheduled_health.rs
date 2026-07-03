//! Tests TDD pour la santé des tâches récurrentes (T2, v0.7.5 F-85).
//!
//! Ordre TDD : test RED → impl → GREEN.
//! Couvre : upsert insert→update, outcome Ok/Error, fenêtre errors_24h,
//! purge paresseuse 7j, seed idempotent.

use gradatum_core::scheduled_health::TaskOutcome;
use gradatum_index::SqliteIndex;

/// Upsert : insert (run_count=1) puis update (run_count=2).
#[tokio::test]
async fn record_task_run_upsert_increments_run_count() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    let now_ms = 1_000_000_i64;

    // Premier appel : INSERT.
    idx.record_task_run("test-task", TaskOutcome::Ok, 10, None, now_ms)
        .await
        .expect("premier record_task_run");

    let rows = idx.list_scheduled_health(now_ms).await.expect("list 1");
    assert_eq!(rows.len(), 1, "1 ligne après premier run");
    let row = &rows[0];
    assert_eq!(row.task_name, "test-task");
    assert_eq!(row.run_count, 1, "run_count=1 après premier tick");
    assert_eq!(row.last_outcome.as_deref(), Some("ok"));
    assert_eq!(row.last_run_ms, Some(now_ms));
    assert_eq!(row.last_duration_ms, Some(10));
    assert!(row.last_error.is_none(), "last_error doit être None sur Ok");

    // Deuxième appel : UPDATE.
    let now2 = now_ms + 60_000;
    idx.record_task_run("test-task", TaskOutcome::Ok, 20, None, now2)
        .await
        .expect("deuxième record_task_run");

    let rows2 = idx.list_scheduled_health(now2).await.expect("list 2");
    assert_eq!(rows2.len(), 1, "toujours 1 ligne après deuxième run");
    assert_eq!(rows2[0].run_count, 2, "run_count=2 après deuxième tick");
    assert_eq!(rows2[0].last_run_ms, Some(now2));
    assert_eq!(rows2[0].last_duration_ms, Some(20));
}

/// Outcome Error : last_error renseigné + 1 ligne dans scheduled_task_error.
#[tokio::test]
async fn record_task_run_error_sets_last_error() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    let now_ms = 2_000_000_i64;

    idx.record_task_run(
        "err-task",
        TaskOutcome::Error,
        5,
        Some("connexion refusée"),
        now_ms,
    )
    .await
    .expect("record error");

    let rows = idx.list_scheduled_health(now_ms).await.expect("list");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.last_outcome.as_deref(), Some("error"));
    assert_eq!(row.last_error.as_deref(), Some("connexion refusée"));
    assert_eq!(row.errors_24h, 1, "errors_24h=1 car error dans la fenêtre");
}

/// errors_24h : erreur à -23h comptée, erreur à -25h exclue.
#[tokio::test]
async fn list_scheduled_health_errors_24h_window() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // "Maintenant" = 30h d'epoch en ms.
    let now_ms = 30 * 3_600_000_i64;
    // -23h (dans la fenêtre).
    let t_23h = now_ms - 23 * 3_600_000_i64;
    // -25h (hors fenêtre).
    let t_25h = now_ms - 25 * 3_600_000_i64;

    // Erreur à -25h.
    idx.record_task_run("win-task", TaskOutcome::Error, 1, Some("err -25h"), t_25h)
        .await
        .expect("record -25h");

    // Erreur à -23h.
    idx.record_task_run("win-task", TaskOutcome::Error, 1, Some("err -23h"), t_23h)
        .await
        .expect("record -23h");

    let rows = idx.list_scheduled_health(now_ms).await.expect("list");
    let row = rows
        .iter()
        .find(|r| r.task_name == "win-task")
        .expect("win-task");
    assert_eq!(
        row.errors_24h, 1,
        "errors_24h=1 : seule l'erreur à -23h est dans la fenêtre"
    );
}

/// Purge paresseuse : erreur > 7j supprimée lors de l'append suivant.
///
/// Prouve via `COUNT(*)` direct sur `scheduled_task_error` qu'une ligne ancienne
/// est physiquement supprimée — pas seulement exclue du calcul `errors_24h`.
#[tokio::test]
async fn record_task_run_lazy_purge_removes_old_errors() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // "Maintenant" = 10j d'epoch.
    let now_ms = 10 * 86_400_000_i64;
    // Erreur à t=1j (> 7j avant now) → doit être purgée.
    let t_old = 86_400_000_i64;

    // Première erreur (ancienne — sera purgée lors de l'insertion suivante).
    idx.record_task_run(
        "purge-task",
        TaskOutcome::Error,
        1,
        Some("vieille erreur"),
        t_old,
    )
    .await
    .expect("record old error");

    // AVANT la 2ème erreur : scheduled_task_error doit avoir 1 ligne.
    let count_before = idx
        .count_task_errors_for("purge-task")
        .await
        .expect("count before");
    assert_eq!(
        count_before, 1,
        "1 ligne dans scheduled_task_error avant purge"
    );

    // Deuxième erreur (maintenant) → déclenche la purge paresseuse.
    idx.record_task_run(
        "purge-task",
        TaskOutcome::Error,
        1,
        Some("err récente"),
        now_ms,
    )
    .await
    .expect("record new error");

    // APRÈS la 2ème erreur : la vieille ligne est physiquement supprimée (purge lazy).
    // Il ne reste QUE l'erreur récente → COUNT = 1.
    let count_after = idx
        .count_task_errors_for("purge-task")
        .await
        .expect("count after");
    assert_eq!(
        count_after, 1,
        "la vieille erreur doit être physiquement supprimée de scheduled_task_error (purge lazy)"
    );

    // errors_24h doit ne compter QUE l'erreur récente.
    let rows = idx.list_scheduled_health(now_ms).await.expect("list");
    let row = rows
        .iter()
        .find(|r| r.task_name == "purge-task")
        .expect("purge-task");
    assert_eq!(
        row.errors_24h, 1,
        "errors_24h=1 : seule l'erreur récente compte"
    );

    // Cohérence : run_count = 2 (les 2 ticks ont bien été enregistrés dans scheduled_task_health).
    assert_eq!(
        row.run_count, 2,
        "run_count=2 : les 2 ticks sont enregistrés"
    );
}

/// seed_scheduled_task : idempotent (INSERT OR IGNORE).
#[tokio::test]
async fn seed_scheduled_task_idempotent() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    let now_ms = 5_000_000_i64;

    // Premier seed.
    idx.seed_scheduled_task("seeded-task")
        .await
        .expect("seed 1");

    // Deuxième seed — ne doit pas écraser le run existant.
    idx.record_task_run("seeded-task", TaskOutcome::Ok, 10, None, now_ms)
        .await
        .expect("record run");

    // Troisième seed — INSERT OR IGNORE ne doit pas écraser run_count.
    idx.seed_scheduled_task("seeded-task")
        .await
        .expect("seed 2");

    let rows = idx.list_scheduled_health(now_ms).await.expect("list");
    let row = rows
        .iter()
        .find(|r| r.task_name == "seeded-task")
        .expect("seeded-task");
    assert_eq!(row.run_count, 1, "seed ne doit pas écraser run_count=1");
    assert_eq!(
        row.last_run_ms,
        Some(now_ms),
        "seed ne doit pas écraser last_run_ms"
    );
}

/// seed crée la ligne avec last_run_ms=None avant tout tick.
#[tokio::test]
async fn seed_scheduled_task_creates_null_entry() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    idx.seed_scheduled_task("null-task").await.expect("seed");

    let rows = idx.list_scheduled_health(0).await.expect("list");
    let row = rows
        .iter()
        .find(|r| r.task_name == "null-task")
        .expect("null-task");
    assert!(
        row.last_run_ms.is_none(),
        "last_run_ms=None avant premier tick"
    );
    assert!(
        row.last_outcome.is_none(),
        "last_outcome=None avant premier tick"
    );
    assert_eq!(row.run_count, 0, "run_count=0 avant premier tick");
    assert_eq!(row.errors_24h, 0, "errors_24h=0 avant premier tick");
}
