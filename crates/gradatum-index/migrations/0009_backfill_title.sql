-- Migration 0009 — backfill colonne `notes.title` pour le corpus existant.
--
-- Contexte (bug LIVE 2026-06-03) : la colonne `title` (ajoutée en 0005) n'était
-- jamais peuplée à l'écriture par le worker. Résultat : 910/911 notes NULL en prod.
-- Le fix write-path est dans gradatum-worker::apalis_handlers::handle_curate.
-- Cette migration récupère le corpus existant en extrayant le H1 Markdown.
--
-- Pattern : première ligne `# Titre` → SUBSTR(body_text, 3, ...).
-- Identique au backfill de 0005 mais étendu à `title = ''` (chaîne vide) en plus de NULL.
--
-- Idempotente : WHERE (title IS NULL OR title = '') exclut les notes déjà renseignées.
-- Exclut les sentinelles : id NOT LIKE '__sentinel__%'.
-- N'écrase PAS les titres déjà présents : guard sur title IS NULL OR title = ''.
--
-- Rollback : UPDATE notes SET title = NULL WHERE id NOT LIKE '__sentinel__%';
--            (réinitialise tout le backfill — à n'utiliser qu'en cas d'urgence)

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
WHERE (title IS NULL OR title = '')
  AND id NOT LIKE '__sentinel__%';

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0009_backfill_title', CAST(strftime('%s','now') AS INTEGER) * 1000);
