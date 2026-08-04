-- 0030_tenants_grants.sql — substrat multi-vault C1 (F-63, plan v1.0.0 A5).
--
-- Additif pur : AUCUN ALTER sur `notes` (règle transversale A5 jusqu'à C5).
-- Ces tables ne sont consultées QUE lorsque `multi_tenant.enabled = true`
-- (flag serveur, défaut false) — le chemin legacy mono-vault "main" ne les lit
-- jamais (byte-identical à flag OFF).
--
-- `tenant_vault_grants` est l'allow-list tenant↔vault (EX-C1-2) : l'absence de
-- ligne est un REFUS (fail-closed). `access` ∈ {'read','write'} — 'write' couvre
-- la lecture. Références logiques sans FK CASCADE (convention du schéma, cf. 0013).
CREATE TABLE IF NOT EXISTS tenants (
  id         TEXT    PRIMARY KEY,
  status     TEXT    NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended')),
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tenant_vault_grants (
  tenant_id TEXT NOT NULL,
  vault_id  TEXT NOT NULL,
  access    TEXT NOT NULL CHECK (access IN ('read', 'write')),
  PRIMARY KEY (tenant_id, vault_id)
);

-- Seed idempotent : le tenant racine "main" conserve l'accès write à son vault
-- (parité stricte avec le verrou legacy `tenant_is_authorized`). INSERT OR IGNORE
-- → re-jouable sans effet (idempotence exigée par A5).
INSERT OR IGNORE INTO tenants (id, status, created_at)
  VALUES ('main', 'active', CAST(strftime('%s', 'now') AS INTEGER) * 1000);
INSERT OR IGNORE INTO tenant_vault_grants (tenant_id, vault_id, access)
  VALUES ('main', 'main', 'write');

-- Enregistrement dans le registre de migrations (guard idempotence du runner).
INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0030_tenants_grants', CAST(strftime('%s','now') AS INTEGER) * 1000);
