-- Migration 0019 : table de compteurs d'usage des read-paths (télémétrie v0.5.3).
--
-- Design : accumulation en mémoire (AtomicU64 dans AppState), flush toutes les 60s.
-- Granularité : heure (window_h = epoch_ms / 3_600_000).
-- WITHOUT ROWID : PK (endpoint, window_h) = seul accès, rowid superflu.
-- UPSERT sur conflit → agrégation cumulative inter-flush.
-- Rétention : 90j (purgée par la tâche retention du server).
CREATE TABLE IF NOT EXISTS read_usage_counters (
    endpoint   TEXT    NOT NULL,
    window_h   INTEGER NOT NULL,   -- epoch_ms / 3_600_000
    hit_count  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (endpoint, window_h)
) WITHOUT ROWID;
