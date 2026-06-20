-- Migration 0007 — colonne agent_id sur event_log (council caveat, v0.3.0 tranche B2)
--
-- Discriminateur agent pour l'event-log : prépare l'apprentissage transverse
-- vs propre-au-rôle (ACL/filtrage différé v0.4.0 — on capture juste la donnée).
--
-- Design : ADD COLUMN nullable — additif, aucune réécriture de table (SQLite safe).
-- Forward-compat : les callers qui ne fournissent pas X-Agent-Id obtiennent NULL.
-- Source : header X-Agent-Id extrait par le gateway (borné 256 chars).

ALTER TABLE event_log ADD COLUMN agent_id TEXT;

-- Index discriminateur : requêtes par agent (B2+ agent-aware queries, v0.4.0 ACL).
CREATE INDEX IF NOT EXISTS idx_event_log_agent ON event_log(agent_id);

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0007_event_log_agent_id', CAST(strftime('%s','now') AS INTEGER) * 1000);
