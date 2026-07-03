-- Migration 0025 — Backfill colonne `title` pour notes section=identity (v0.7.3 Slice A).
--
-- Contexte (Slice A Task 3) : avant cette migration, les notes soul `identity` créées
-- par vault_write peuvent avoir `title IS NULL` dans la colonne `notes.title` si
-- `upsert_note_title` n'a pas été appelé explicitement.
--
-- Fix v0.7.3 (Task 1) : `title_lookup` résout maintenant par colonne-first.
-- Cette migration garantit que les âmes existantes sont résolvables sans H1 dans le body.
--
-- LOGIQUE D'EXTRACTION :
--   - `body_text LIKE '# identity/%'` : cible uniquement les notes soul
--     avec H1 au format attendu.
--   - `substr(body_text, 3, ...)` : supprime le préfixe `# ` (2 caractères).
--   - `instr(body_text || char(10), char(10))` : position du 1er LF (défensive :
--     le `|| char(10)` garantit qu'on trouve toujours un LF même si le body est
--     une ligne unique sans newline).
--   - `rtrim(..., char(13))` : neutralise les fins CRLF (\r\n) — cas Windows/edge.
--
-- IDEMPOTENCE :
--   - WHERE `title IS NULL OR title = ''` : ne touche jamais les titres existants.
--   - `INSERT OR IGNORE INTO _schema_migrations` : la contrainte PRIMARY KEY sur
--     `version` est déjà un guard ; l'IGNORE permet la ré-application défensive
--     (tests inline, recovery) sans erreur.
--
-- ROLLBACK : `UPDATE notes SET title = NULL WHERE section = 'identity'` — voir docs/ops/migration-rollback.md.

UPDATE notes
SET title = rtrim(
        substr(body_text, 3, instr(body_text || char(10), char(10)) - 3),
        char(13)
    )
WHERE section = 'identity'
  AND (title IS NULL OR title = '')
  AND body_text LIKE '# identity/%';

INSERT OR IGNORE INTO _schema_migrations (version, applied_at)
VALUES ('0025_identity_title_backfill', CAST(strftime('%s','now') AS INTEGER) * 1000);
