-- 0031_tenants_status_deleted.sql — C2 (F-18, EX-C2-4) : soft-delete de vault.
--
-- Élargit la contrainte CHECK de `tenants.status` pour admettre 'deleted'
-- (suppression logique : le tenant disparaît de `list_active_vaults` et le JOIN
-- status='active' de `tenant_grants` refuse immédiatement — la purge physique
-- des notes reste différée aux jobs existants, AUCUN ALTER destructif sur `notes`).
--
-- SQLite ne sait pas modifier un CHECK en place → rebuild non destructif de la
-- table (copie intégrale des lignes). `tenants` est seed-only à ce stade (0030
-- jamais déployée LIVE) : le rebuild est trivial et re-jouable via le guard du
-- runner (_schema_migrations).
CREATE TABLE tenants_new (
  id         TEXT    PRIMARY KEY,
  status     TEXT    NOT NULL DEFAULT 'active'
             CHECK (status IN ('active', 'suspended', 'deleted')),
  created_at INTEGER NOT NULL
);
INSERT INTO tenants_new (id, status, created_at)
  SELECT id, status, created_at FROM tenants;
DROP TABLE tenants;
ALTER TABLE tenants_new RENAME TO tenants;

-- Enregistrement dans le registre de migrations (guard idempotence du runner).
INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0031_tenants_status_deleted', CAST(strftime('%s','now') AS INTEGER) * 1000);
