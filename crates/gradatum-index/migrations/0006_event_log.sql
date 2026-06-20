-- Migration 0006 — table event_log (B1 tranche v0.3.0)
--
-- Table append-only pour la persistance des QaEvents (télémétrie gateway).
-- Design : séparée des tables notes/notes_fts → zéro pollution FTS5.
-- Immuable par construction (aucun chemin UPDATE/DELETE par record).
-- La tâche de rétention supprime des lignes EN MASSE par âge (maintenance interne).
--
-- Alignement v81 D-06 ACTÉ : append-only = write-no-delete ACL ;
-- rétention = chemin admin/full/Job::Purge (tokio interval en v0.3.0).
-- Forward-compat F-19 : flag `processed` (0=pending) consommé par Job::Distill v0.5.0.
-- Backup : exclure event_log (v81 l.4995 — télémétrie disposable, reconstructible).

CREATE TABLE IF NOT EXISTS event_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           INTEGER NOT NULL,              -- epoch ms (parsé depuis QaEvent.timestamp RFC3339)
    tenant_id    TEXT    NOT NULL,              -- depuis JWT (TrustContext::BearerToken)
    route        TEXT    NOT NULL,
    model_alias  TEXT    NOT NULL,
    model_used   TEXT,                          -- modèle réel résolu (pricing key) — nullable v0.3.0
    provider     TEXT    NOT NULL,
    feature_id   TEXT,                          -- nullable (header X-Feature-Id absent autorisé)
    status_code  INTEGER NOT NULL,
    latency_ms   INTEGER NOT NULL,
    tokens_input  INTEGER,                      -- nullable (streaming → None) — v81 naming
    tokens_output INTEGER,                      -- nullable (streaming → None) — v81 naming
    cost_usd     REAL,                          -- toujours NULL en v0.3.0 (pas de pricing table)
    processed    INTEGER NOT NULL DEFAULT 0,   -- 0=pending, 1=consommé Job::Distill F-19 v0.5.0
    created_at   INTEGER NOT NULL              -- epoch ms insertion serveur
);

-- Index primaire : purge par âge (WHERE created_at < cutoff) — critique perf rétention.
CREATE INDEX IF NOT EXISTS idx_event_log_created   ON event_log(created_at);
-- Index tenant : requêtes coût/usage par tenant (B2+ cost-attribution queries).
CREATE INDEX IF NOT EXISTS idx_event_log_tenant    ON event_log(tenant_id);
-- Index feature : attribution par feature (B2+ QaHistory routing).
CREATE INDEX IF NOT EXISTS idx_event_log_feature   ON event_log(feature_id);
-- Index processed : consommation Job::Distill (F-19 v0.5.0 — forward-compat).
CREATE INDEX IF NOT EXISTS idx_event_log_processed ON event_log(processed);

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0006_event_log', CAST(strftime('%s','now') AS INTEGER) * 1000);
