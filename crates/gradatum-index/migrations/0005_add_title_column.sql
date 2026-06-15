-- 0005_add_title_column.sql
-- Ajoute la colonne `title` (H1 Markdown extrait) sur la table `notes`.
--
-- Rollback : ALTER TABLE notes DROP COLUMN title; (SQLite 3.35+)
--
-- Backfill : extrait la première ligne `# ...` pour les notes existantes.
-- Le UPDATE est idempotent (WHERE title IS NULL) et exclut les sentinelles.
ALTER TABLE notes ADD COLUMN title TEXT;

CREATE INDEX IF NOT EXISTS idx_notes_title ON notes(vault_id, title)
  WHERE title IS NOT NULL;

-- Backfill : extraire le titre H1 des notes existantes.
-- Pattern : `body_text LIKE '# %'` = commence par '# '.
-- SUBSTR(body_text, 3, ...) extrait à partir du 3ème caractère (après '# ').
-- INSTR(body_text, CHAR(10)) localise le premier '\n' pour tronquer le titre.
UPDATE notes
SET title = CASE
  WHEN body_text LIKE '# %' THEN
    TRIM(SUBSTR(body_text, 3,
      CASE
        WHEN INSTR(body_text, CHAR(10)) > 0
        THEN INSTR(body_text, CHAR(10)) - 3
        ELSE LENGTH(body_text) - 2
      END))
  ELSE NULL
END
WHERE title IS NULL
  AND id NOT LIKE '__sentinel__%';

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0005_add_title_column', CAST(strftime('%s','now') AS INTEGER) * 1000);
