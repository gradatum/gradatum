-- Migration 008 — Table idempotency (F-16 Idempotency-Key)
--
-- Stocke les paires (Idempotency-Key → job_id) pour le dédup des POST /api/v1/jobs.
-- TTL 24h : les entrées expirées sont nettoyées par le job cron IdempotencyCleanup
-- (schedules.rs, toutes les heures).
--
-- Garanties :
-- - PRIMARY KEY sur `key` : INSERT OR IGNORE est atomique (pas de TOCTOU).
-- - `created_at` en timestamp Unix ms pour le TTL cleanup simple.
-- - `job_id` est le ULID string du job associé.
--
-- Compatibilité : SQLite WAL mode, INTEGER PRIMARY KEY autoincrement non requis.

CREATE TABLE IF NOT EXISTS gradatum_idempotency (
    -- Clé d'idempotence fournie par le client (header Idempotency-Key)
    -- Taille max recommandée : 128 chars. Pas de validation format (opaque string).
    key         TEXT NOT NULL PRIMARY KEY,

    -- ULID du job créé lors du premier appel avec cette clé
    job_id      TEXT NOT NULL,

    -- Timestamp de création (Unix ms) — utilisé pour le TTL cleanup
    created_at  INTEGER NOT NULL
);

-- Index pour le cleanup TTL (DELETE WHERE created_at < ?)
CREATE INDEX IF NOT EXISTS idx_idempotency_created_at
    ON gradatum_idempotency (created_at);
