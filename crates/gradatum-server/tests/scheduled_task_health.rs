//! Tests TDD — santé des tâches récurrentes (T4, v0.7.5 F-85).
//!
//! Couvre :
//! - `ALL_SCHEDULED_TASKS` : 8 entrées, noms corrects.
//! - `task_interval_secs` : retourne les intervalles attendus pour chaque tâche.
//! - `seed_scheduled_task` via IndexStore : après seed, les 8 tâches apparaissent avec
//!   `last_run_ms=None` et `run_count=0`.

use gradatum_index::SqliteIndex;
use gradatum_server::config::ServerConfig;
use gradatum_server::scheduled_tasks::{
    ALL_SCHEDULED_TASKS, TASK_ACTIVE_RECALL_PURGE, TASK_METRIC_SAMPLE, TASK_PROACTIVE_REFRESH,
    TASK_PURGE_EVENT_LOG, TASK_PURGE_READ_USAGE, TASK_PURGE_SESSION_TRACE, TASK_REVIEW_PROMOTE,
    TASK_TELEMETRY_FLUSH, task_interval_secs,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_cfg() -> ServerConfig {
    ServerConfig::default()
}

// ---------------------------------------------------------------------------
// ALL_SCHEDULED_TASKS
// ---------------------------------------------------------------------------

/// `ALL_SCHEDULED_TASKS` contient exactement 8 tâches.
#[test]
fn all_scheduled_tasks_has_8_entries() {
    assert_eq!(
        ALL_SCHEDULED_TASKS.len(),
        8,
        "8 tâches récurrentes in-process attendues"
    );
}

/// `ALL_SCHEDULED_TASKS` contient les 8 noms canoniques attendus.
#[test]
fn all_scheduled_tasks_contains_correct_names() {
    assert!(ALL_SCHEDULED_TASKS.contains(&TASK_TELEMETRY_FLUSH));
    assert!(ALL_SCHEDULED_TASKS.contains(&TASK_PURGE_EVENT_LOG));
    assert!(ALL_SCHEDULED_TASKS.contains(&TASK_PURGE_SESSION_TRACE));
    assert!(ALL_SCHEDULED_TASKS.contains(&TASK_PURGE_READ_USAGE));
    assert!(ALL_SCHEDULED_TASKS.contains(&TASK_REVIEW_PROMOTE));
    assert!(ALL_SCHEDULED_TASKS.contains(&TASK_PROACTIVE_REFRESH));
    assert!(ALL_SCHEDULED_TASKS.contains(&TASK_ACTIVE_RECALL_PURGE));
    assert!(ALL_SCHEDULED_TASKS.contains(&TASK_METRIC_SAMPLE));
}

// ---------------------------------------------------------------------------
// task_interval_secs — SSOT
// ---------------------------------------------------------------------------

/// `telemetry-flush` est hardcodé à 60s (non configurable).
#[test]
fn task_interval_secs_telemetry_flush_is_60() {
    let cfg = default_cfg();
    assert_eq!(
        task_interval_secs(TASK_TELEMETRY_FLUSH, &cfg),
        60,
        "telemetry-flush : 60s hardcodé"
    );
}

/// `purge-event-log` retourne `event_log.purge_interval_secs.max(60)`.
#[test]
fn task_interval_secs_purge_event_log_matches_config() {
    let cfg = default_cfg();
    let expected = cfg.event_log.purge_interval_secs.max(60);
    assert_eq!(
        task_interval_secs(TASK_PURGE_EVENT_LOG, &cfg),
        expected,
        "purge-event-log : doit refléter event_log.purge_interval_secs.max(60)"
    );
}

/// `purge-session-trace`, `purge-read-usage`, `active-recall-purge` partagent
/// `session_trace.purge_interval_secs.max(60)`.
#[test]
fn task_interval_secs_session_trace_tasks_share_interval() {
    let cfg = default_cfg();
    let expected = cfg.session_trace.purge_interval_secs.max(60);
    assert_eq!(
        task_interval_secs(TASK_PURGE_SESSION_TRACE, &cfg),
        expected,
        "purge-session-trace : doit refléter session_trace.purge_interval_secs.max(60)"
    );
    assert_eq!(
        task_interval_secs(TASK_PURGE_READ_USAGE, &cfg),
        expected,
        "purge-read-usage : réutilise session_trace.purge_interval_secs.max(60)"
    );
    assert_eq!(
        task_interval_secs(TASK_ACTIVE_RECALL_PURGE, &cfg),
        expected,
        "active-recall-purge : réutilise session_trace.purge_interval_secs.max(60)"
    );
}

/// `review-promote` retourne `review_promote.interval_secs.max(60)`.
#[test]
fn task_interval_secs_review_promote_matches_config() {
    let cfg = default_cfg();
    let expected = cfg.review_promote.interval_secs.max(60);
    assert_eq!(
        task_interval_secs(TASK_REVIEW_PROMOTE, &cfg),
        expected,
        "review-promote : doit refléter review_promote.interval_secs.max(60)"
    );
}

/// `proactive-refresh` retourne `proactive_recall.refresh_interval_secs.max(60)`.
#[test]
fn task_interval_secs_proactive_refresh_matches_config() {
    let cfg = default_cfg();
    let expected = cfg.proactive_recall.refresh_interval_secs.max(60);
    assert_eq!(
        task_interval_secs(TASK_PROACTIVE_REFRESH, &cfg),
        expected,
        "proactive-refresh : doit refléter proactive_recall.refresh_interval_secs.max(60)"
    );
}

/// Nom inconnu → fallback 60.
#[test]
fn task_interval_secs_unknown_task_returns_60() {
    let cfg = default_cfg();
    assert_eq!(
        task_interval_secs("tâche-inconnue", &cfg),
        60,
        "nom inconnu : fallback 60s"
    );
}

/// Le plancher de 60s est garanti même si la config est inférieure.
#[test]
fn task_interval_secs_floor_60s() {
    let mut cfg = default_cfg();
    // Écraser les intervals à 1 pour vérifier que max(60) est bien appliqué.
    cfg.event_log.purge_interval_secs = 1;
    cfg.session_trace.purge_interval_secs = 0;
    cfg.review_promote.interval_secs = 30;
    cfg.proactive_recall.refresh_interval_secs = 10;

    assert_eq!(
        task_interval_secs(TASK_PURGE_EVENT_LOG, &cfg),
        60,
        "plancher 60s event_log"
    );
    assert_eq!(
        task_interval_secs(TASK_PURGE_SESSION_TRACE, &cfg),
        60,
        "plancher 60s session_trace"
    );
    assert_eq!(
        task_interval_secs(TASK_REVIEW_PROMOTE, &cfg),
        60,
        "plancher 60s review_promote"
    );
    assert_eq!(
        task_interval_secs(TASK_PROACTIVE_REFRESH, &cfg),
        60,
        "plancher 60s proactive_refresh"
    );
}

// ---------------------------------------------------------------------------
// seed_scheduled_task via IndexStore
// ---------------------------------------------------------------------------

/// Après seed de toutes les tâches, `list_scheduled_health` retourne 8 entrées
/// avec `last_run_ms=None` et `run_count=0`.
#[tokio::test]
async fn seed_all_tasks_creates_null_entries() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    for task in ALL_SCHEDULED_TASKS {
        idx.seed_scheduled_task(task)
            .await
            .expect("seed_scheduled_task");
    }

    let rows = idx
        .list_scheduled_health(0)
        .await
        .expect("list_scheduled_health");
    assert_eq!(rows.len(), 8, "8 entrées attendues après seed");

    for task_name in ALL_SCHEDULED_TASKS {
        let row = rows
            .iter()
            .find(|r| r.task_name == task_name)
            .unwrap_or_else(|| panic!("tâche {} absente après seed", task_name));
        assert!(
            row.last_run_ms.is_none(),
            "tâche {} : last_run_ms doit être None avant premier tick",
            task_name
        );
        assert_eq!(
            row.run_count, 0,
            "tâche {} : run_count doit être 0 avant premier tick",
            task_name
        );
    }
}
