-- 0042_agent_vault_grants.sql — substrat agent↔vault (lot B6, plan v1.0.0).
--
-- Duplique le pattern `tenant_vault_grants` (migration 0030) un cran plus bas :
-- l'agent, pas le tenant. `access` ∈ {'read','write'} — 'write' couvre la
-- lecture. L'absence de ligne est un REFUS (fail-closed, invariant 5 du modèle
-- de droits). Pas de FK CASCADE (convention du schéma).
--
-- Additif pur : `CREATE TABLE`, aucun ALTER sur `notes` ni sur `tenant_vault_grants`.
-- Table inerte tant qu'aucune consultation n'a lieu — la consultation est câblée
-- dans les lots B7 (identité par agent) et B8 (portée par section).
--
-- Idempotence : garantie par le registre `_schema_migrations` du runner (forward-only).
CREATE TABLE IF NOT EXISTS agent_vault_grants (
    agent_id TEXT NOT NULL,
    vault_id TEXT NOT NULL,
    access   TEXT NOT NULL CHECK (access IN ('read', 'write')),
    section  TEXT,
    PRIMARY KEY (agent_id, vault_id)
);

-- Seed idempotent : l'agent racine "main-agent" conserve l'accès write au vault
-- "main" (parité avec le seed 0030 pour les tenants). INSERT OR IGNORE →
-- re-jouable sans effet (idempotence exigée).
INSERT OR IGNORE INTO agent_vault_grants (agent_id, vault_id, access)
  VALUES ('main-agent', 'main', 'write');

-- Enregistrement dans le registre de migrations (guard idempotence du runner).
INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0042_agent_vault_grants', CAST(strftime('%s','now') AS INTEGER) * 1000);
