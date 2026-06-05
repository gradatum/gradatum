-- Migration 0003 : ajout de la colonne tags dans notes (T3 P2.0c).
--
-- Raison : FTS5 avec content=notes lit les colonnes depuis la table source (notes).
-- La colonne tags était insérée dans notes_fts shadow tables mais pas dans notes,
-- rendant impossible un JOIN notes/notes_fts pour récupérer les tags.
-- Solution : stocker les tags (espace-séparés) directement dans notes pour les
-- queries non-FTS (distinct_tags, get_note).
--
-- NULL pour les notes existantes (sentinelles et notes pré-migration).
ALTER TABLE notes ADD COLUMN tags TEXT;

INSERT INTO _schema_migrations (version, applied_at) VALUES ('0003_add_tags_to_notes', strftime('%s', 'now') * 1000);
