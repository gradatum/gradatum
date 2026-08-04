-- Migration 0035 — `redirect_table` PK composite `(vault_id, title_slug)` (Groupe B, M4).
--
-- ## Root-cause / objectif
--
-- `redirect_table` (créée en migration 0010, F-39 Stable Wikilinks) est la SEULE table
-- filles restée à PRIMARY KEY globale (`title_slug` seul) — toutes les autres ont été
-- recomposées en PK incluant `vault_id` (0032 `notes`, 0033/0034 tables filles).
--
-- Deux vaults distincts renommant une note vers un même titre produisent le même
-- `title_slug`. Avec la PK globale :
--   * write-path : l'`INSERT OR REPLACE` du second vault CLOBBE la ligne du premier
--     (même clé `title_slug`) ;
--   * read-path : `resolve_redirect(slug)` (`WHERE title_slug = ?`) résout le slug d'un
--     vault vers l'ULID enregistré par l'autre — fuite d'isolation cross-vault ;
--   * delete-path : `delete_redirect_by_ulid(ulid)` (`WHERE ulid = ?`) supprime la ligne
--     homonyme (même ULID) d'un autre vault.
--
-- Cette migration recompose la PK en `(vault_id, title_slug)`, fermant la classe à la
-- racine (miroir de 0032/0033/0034). Les write/read/delete-paths sont scopés par
-- `vault_id` dans le même lot (`links.rs`, `sqlite.rs`, `admin/vault_rename.rs`).
--
-- ## Data-safety / backfill
--
-- La table gagne une colonne `vault_id NOT NULL` (absente de 0010). Les lignes legacy
-- appartiennent toutes au vault mono-tenant `'main'` (flag `multi_tenant` OFF) → backfill
-- explicite `vault_id = 'main'` sur toutes les lignes existantes. En mono-vault chaque
-- `title_slug` est unique, donc `(main, title_slug)` ≡ ancienne clé `title_slug` : zéro
-- perte, zéro collision au copy, zéro changement observable (byte-identical flag OFF).
--
-- ## Choréographie SQLite (recreate) — identique à 0033/0034
--
-- SQLite ne modifie pas une PRIMARY KEY in-place : recreate obligatoire (create new +
-- copy + drop + rename). `PRAGMA foreign_keys=OFF` + `PRAGMA legacy_alter_table=ON`
-- (pattern 12-steps : empêche `ALTER TABLE RENAME` de réécrire les références des autres
-- objets). L'index secondaire `idx_redirect_ulid` (perdu au DROP) est recréé. PRAGMAs
-- restaurés à la fin. Aucun rebuild FTS (redirect_table n'est pas external-content FTS).
--
-- ## Rollback
--
-- Le runner est forward-only. Rollback manuel documenté : `0035_redirect_table_vault_id.down.sql`
-- (non auto-exécuté ; restaure la PK `title_slug` seule et retire la colonne `vault_id`).

PRAGMA foreign_keys=OFF;
PRAGMA legacy_alter_table=ON;

-- redirect_table → PK (vault_id, title_slug) ──────────────────────────────────────
ALTER TABLE redirect_table RENAME TO redirect_table_old_0035;
CREATE TABLE redirect_table (
    vault_id   TEXT NOT NULL,
    title_slug TEXT NOT NULL,
    ulid       TEXT NOT NULL,
    renamed_at INTEGER NOT NULL,
    PRIMARY KEY (vault_id, title_slug)
);
-- Backfill : toutes les lignes legacy appartiennent au vault mono-tenant 'main'.
INSERT INTO redirect_table (vault_id, title_slug, ulid, renamed_at)
    SELECT 'main', title_slug, ulid, renamed_at FROM redirect_table_old_0035;
DROP TABLE redirect_table_old_0035;
CREATE INDEX IF NOT EXISTS idx_redirect_ulid ON redirect_table(ulid);

PRAGMA legacy_alter_table=OFF;
PRAGMA foreign_keys=ON;

INSERT INTO _schema_migrations (version, applied_at)
  VALUES ('0035_redirect_table_vault_id', strftime('%s', 'now') * 1000);
