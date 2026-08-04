-- Migration 011 — Isolation des jobs par tenant (L1+L2 flip-blocker)
--
-- Ajoute la colonne `tenant_id` sur `gradatum_jobs` — la table LIVE du
-- QueueStore (SqliteQueueStore, sqlx). `tenant_id` = tenant SERVI par le job,
-- dérivé du spec à l'enqueue via `gradatum_core::spec_tenant` (source = le spec,
-- pas l'appelant).
--
-- Backfill : les jobs existants (créés avant cette migration) prennent le
-- DEFAULT 'main' — correct, car à `multi_tenant` OFF tout le trafic est `main`.
--
-- Filtrage : appliqué conditionnellement par SqliteQueueStore.
--   * `None`    = aucune clause = byte-identical OFF (SQL inchangé).
--   * `Some(t)` = `AND tenant_id = ?` (isolation ON, 404 anti-disclosure).
--
-- La table legacy `jobs` (queue_v1, drainée post-v0.2.0 par migration 009)
-- n'est PAS concernée : le worker Apalis ne lit que `gradatum_jobs`.

ALTER TABLE gradatum_jobs ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'main';

-- Index pour le filtrage par tenant (get/cancel/list/count/latest à ON).
CREATE INDEX IF NOT EXISTS idx_gradatum_jobs_tenant ON gradatum_jobs (tenant_id);
