-- 0028_archive_index.sql — registre des archives (F-100 incrément 1.6).
--
-- Le delete (archivage) déplace le `.md` + `.history/` vers `<vault>/.archive/`
-- et inscrit ici une ligne. Ce registre PILOTE le GC de rétention (jamais un scan
-- filesystem) et sert de trace historique : une ligne SURVIT après GC physique
-- (gc_at marqué) ou après restauration (restored_at marqué). Additive, jamais modifiée.
--
-- La note archivée est TOTALEMENT absente des index de recherche (notes/FTS/ANN/
-- temporal) — ce registre est distinct et n'est jamais joint aux recherches.
CREATE TABLE IF NOT EXISTS archive_index (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  note_id        TEXT    NOT NULL,
  vault_id       TEXT    NOT NULL,               -- vault propriétaire (miroir notes.vault_id, migration 0001)
  section        TEXT    NOT NULL,
  title          TEXT,
  original_locus TEXT,
  archive_path   TEXT    NOT NULL,
  archived_at    INTEGER NOT NULL,
  archived_by    TEXT,
  gc_due         INTEGER NOT NULL,
  gc_at          INTEGER,
  restored_at    INTEGER
);

-- GC piloté par le registre : SELECT ... WHERE gc_due < now AND gc_at IS NULL.
CREATE INDEX IF NOT EXISTS idx_archive_gc_due ON archive_index(gc_due) WHERE gc_at IS NULL;
-- Résolution par note (restore/purge par ULID).
CREATE INDEX IF NOT EXISTS idx_archive_note ON archive_index(note_id);
-- Dimension vault (anticipation multi-vault v1.0) : listing/filtrage par vault propriétaire.
CREATE INDEX IF NOT EXISTS idx_archive_vault ON archive_index(vault_id);
-- Au plus UNE archive active (ni détruite, ni restaurée) par note : une note ne peut
-- être ré-archivée qu'après restauration ou GC de l'archive précédente.
CREATE UNIQUE INDEX IF NOT EXISTS uidx_archive_active
  ON archive_index(note_id) WHERE gc_at IS NULL AND restored_at IS NULL;
