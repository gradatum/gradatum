-- Migration 0026 : santé des tâches récurrentes (v0.7.5 F-85).
--
-- 2 tables :
--  - scheduled_task_health : snapshot statut courant, 1 ligne/tâche (upsert PK).
--  - scheduled_task_error  : journal append-only des erreurs (fenêtre 7j, purge paresseuse).
--
-- Migration additive (CREATE TABLE IF NOT EXISTS) — revert binaire sûr.
-- Les tables restent inertes si le binaire est revert.

CREATE TABLE IF NOT EXISTS scheduled_task_health (
    task_name        TEXT NOT NULL PRIMARY KEY,
    last_run_ms      INTEGER,
    last_outcome     TEXT,
    last_duration_ms INTEGER,
    last_error       TEXT,
    run_count        INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS scheduled_task_error (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task_name   TEXT NOT NULL,
    occurred_ms INTEGER NOT NULL,
    error_msg   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sched_err_task_time ON scheduled_task_error(task_name, occurred_ms);

INSERT INTO _schema_migrations (version, applied_at) VALUES ('0026_scheduled_task_health', unixepoch() * 1000);
