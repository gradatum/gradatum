-- Rollback manuel de 0040 (L3, F-121) — **NON auto-exécuté** par le runner.
--
-- Le runner de migrations est forward-only (`_schema_migrations` ne trace que les
-- versions appliquées, pas de mécanisme `down`). Ce script est le rollback documenté
-- à appliquer MANUELLEMENT (`sqlite3 <db> < 0040_grants_section_scope.down.sql`) si un
-- rollback LIVE de 0040 est nécessaire — par exemple avant un redéploiement d'un
-- binaire pré-0040.
--
-- ⚠️ PERTE DE DONNÉES ASSUMÉE : les grants section-scopés (`section IS NOT NULL`)
-- deviendraient des grants VAULT-ENTIER en revenant au schéma C1 — élargissement
-- silencieux de privilège. Ce script les SUPPRIME donc explicitement (fail-closed :
-- un grant qu'on ne sait plus borner est retiré, jamais élargi).
--
-- Rebuild de table plutôt que `ALTER TABLE ... DROP COLUMN` : le schéma reconstruit est
-- alors byte-identical à celui laissé par 0030 (mêmes colonnes, même PK, même CHECK),
-- sans dépendre de la version de SQLite embarquée.

PRAGMA foreign_keys=OFF;
PRAGMA legacy_alter_table=ON;

-- 1. Retrait des grants qui ne survivent pas au schéma C1 (fail-closed).
DELETE FROM tenant_vault_grants WHERE section IS NOT NULL;

-- 2. Recomposition de la table au schéma 0030 exact.
ALTER TABLE tenant_vault_grants RENAME TO tenant_vault_grants_down_0040;
CREATE TABLE tenant_vault_grants (
  tenant_id TEXT NOT NULL,
  vault_id  TEXT NOT NULL,
  access    TEXT NOT NULL CHECK (access IN ('read', 'write')),
  PRIMARY KEY (tenant_id, vault_id)
);
INSERT INTO tenant_vault_grants (tenant_id, vault_id, access)
  SELECT tenant_id, vault_id, access FROM tenant_vault_grants_down_0040;
DROP TABLE tenant_vault_grants_down_0040;

-- 3. Retrait de la ligne de registre : le runner ré-appliquera 0040 au prochain
--    démarrage d'un binaire post-0040.
DELETE FROM _schema_migrations WHERE version = '0040_grants_section_scope';
