-- Rollback manuel de 0035 (Groupe B, M4) — **NON auto-exécuté** par le runner.
--
-- Le runner de migrations est forward-only (`_schema_migrations` ne trace que les
-- versions appliquées, pas de mécanisme `down`). Ce script est le rollback documenté à
-- appliquer MANUELLEMENT (via `sqlite3 <db> < 0035_redirect_table_vault_id.down.sql`) si
-- un rollback LIVE de 0035 est nécessaire — par exemple avant un redéploiement d'un
-- binaire pré-M4.
--
-- Il restaure la PRIMARY KEY d'origine (`title_slug` seule) et RETIRE la colonne
-- `vault_id` (schéma 0010). En mono-vault `'main'`, `title_slug` est unique, donc la
-- projection `(vault_id, title_slug) → title_slug` ne perd aucune ligne. Si des lignes
-- de plusieurs vaults partagent un même `title_slug` (régime multi-vault, hors flag OFF),
-- ce rollback lèverait une violation de PK au copy — c'est intentionnel : downgrader vers
-- un schéma mono-tenant alors que des données multi-vault existent est une perte de
-- données à refuser explicitement (ne jamais forcer le rollback dans ce cas).
--
-- L'index secondaire `idx_redirect_ulid` est recréé, et la ligne de registre
-- `_schema_migrations` retirée pour que le runner ré-applique 0035 au prochain démarrage
-- d'un binaire post-M4.
--
-- Choréographie identique à 0035 (foreign_keys=OFF + legacy_alter_table=ON).

PRAGMA foreign_keys=OFF;
PRAGMA legacy_alter_table=ON;

-- redirect_table → PK title_slug (schéma 0010, sans colonne vault_id) ──────────────
ALTER TABLE redirect_table RENAME TO redirect_table_down_0035;
CREATE TABLE redirect_table (
    title_slug TEXT NOT NULL PRIMARY KEY,
    ulid       TEXT NOT NULL,
    renamed_at INTEGER NOT NULL
);
INSERT INTO redirect_table (title_slug, ulid, renamed_at)
    SELECT title_slug, ulid, renamed_at FROM redirect_table_down_0035;
DROP TABLE redirect_table_down_0035;
CREATE INDEX IF NOT EXISTS idx_redirect_ulid ON redirect_table(ulid);

PRAGMA legacy_alter_table=OFF;
PRAGMA foreign_keys=ON;

DELETE FROM _schema_migrations WHERE version = '0035_redirect_table_vault_id';
