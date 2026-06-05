-- Migration 009 — Drain jobs_v2 vers DLQ (Phase 1.2 bridge incident LIVE)
--
-- Contexte :
--   Post-deploy v0.2.0, le worker Apalis lit uniquement `gradatum_jobs`.
--   Les jobs enqueués avant le bridge Phase 1.2 dans `jobs_v2` (status='pending')
--   ne seront jamais traités — aucun worker ne consomme cette table.
--
-- Action :
--   Marquer tous les jobs `pending` dans `jobs_v2` comme `failed` avec un
--   message explicite. Ils passent ainsi en DLQ visible pour le monitoring
--   et ne sont plus retentés silencieusement.
--
-- Replay :
--   Les notes correspondantes doivent être re-soumises via `vault_write` MCP
--   qui enqueuje désormais dans `gradatum_jobs` (chemin Phase 1.2).
--
--
-- Idempotent : WHERE status = 'pending' évite de toucher les jobs déjà done/leased.

-- Guard : jobs_v2 peut être absente sur une instance fraîche (première install v0.2.0+).
-- Crée la table si absente (idempotent via IF NOT EXISTS) avant l'UPDATE.
-- Les instances de mise à jour depuis alpha.15 ont déjà jobs_v2 via SCHEMA_V1 gradatum-queue.
CREATE TABLE IF NOT EXISTS jobs_v2 (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant_id    TEXT    NOT NULL DEFAULT 'main',
    kind         TEXT    NOT NULL DEFAULT '',
    payload      BLOB    NOT NULL DEFAULT X'',
    status       TEXT    NOT NULL DEFAULT 'pending',
    attempts     INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 5,
    lease_until  INTEGER,
    leased_by    TEXT,
    created_at   INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT
);

UPDATE jobs_v2
   SET status     = 'failed',
       last_error = 'drain Phase 1.2 : job_store bridge actif depuis v0.2.0 — '
                    || 'ce job ne sera pas traité (jobs_v2 sans worker). '
                    || 'Re-soumettre via vault_write MCP pour re-enqueue dans gradatum_jobs.',
       updated_at = CAST(strftime('%s', 'now') AS INTEGER)
 WHERE status = 'pending';
