-- Rollback manuel de 0034 (C4-1e, Slice D3) — **NON auto-exécuté** par le runner.
--
-- Le runner de migrations est forward-only (`_schema_migrations` ne trace que les versions
-- appliquées, pas de mécanisme `down`). Ce script est le rollback documenté à appliquer
-- MANUELLEMENT (via `sqlite3 <db> < 0034_child_tables_composite_pk.down.sql`) si un rollback
-- LIVE de 0034 est nécessaire — par exemple avant un redéploiement du binaire pré-D3.
--
-- Il restaure les PRIMARY KEY d'origine (sans `vault_id` dans la clé) des trois tables
-- filles, recopie les données 1:1 (aucune colonne ne change — la dimension `vault_id` reste
-- présente comme colonne, seule la clé est reculée), recrée les index secondaires, et retire
-- la ligne de registre `_schema_migrations` pour que le runner ré-applique 0034 au prochain
-- démarrage d'un binaire post-D3.
--
-- Choréographie identique à 0034 (foreign_keys=OFF + legacy_alter_table=ON).

PRAGMA foreign_keys=OFF;
PRAGMA legacy_alter_table=ON;

-- ── 1. note_index → PK note_id ──────────────────────────────────────────────────
ALTER TABLE note_index RENAME TO note_index_down_0034;
CREATE TABLE note_index (
  note_id TEXT PRIMARY KEY,
  vault_id TEXT NOT NULL,
  locus TEXT,
  bm25_tokens INTEGER NOT NULL,
  last_indexed INTEGER NOT NULL
);
INSERT INTO note_index (note_id, vault_id, locus, bm25_tokens, last_indexed)
  SELECT note_id, vault_id, locus, bm25_tokens, last_indexed FROM note_index_down_0034;
DROP TABLE note_index_down_0034;

-- ── 2. temporal_index → PK note_id ──────────────────────────────────────────────
ALTER TABLE temporal_index RENAME TO temporal_index_down_0034;
CREATE TABLE temporal_index (
  note_id TEXT NOT NULL PRIMARY KEY,
  vault_id TEXT NOT NULL,
  anchor_ms INTEGER NOT NULL,
  anchor_src TEXT NOT NULL,
  doc_kind TEXT NOT NULL,
  valid_until_ms INTEGER
);
INSERT INTO temporal_index (note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms)
  SELECT note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms FROM temporal_index_down_0034;
DROP TABLE temporal_index_down_0034;
CREATE INDEX idx_temporal_anchor ON temporal_index(anchor_ms);
CREATE INDEX idx_temporal_vault_anchor ON temporal_index(vault_id, anchor_ms);

-- ── 3. note_overrides → PK (note_id, scope_kind, scope_id, override_type) ────────
ALTER TABLE note_overrides RENAME TO note_overrides_down_0034;
CREATE TABLE note_overrides (
  note_id TEXT NOT NULL,
  vault_id TEXT NOT NULL,
  scope_kind TEXT NOT NULL,
  scope_id TEXT NOT NULL,
  override_type TEXT NOT NULL,
  schema_version INTEGER NOT NULL,
  payload_toml TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 0,
  created_by_kind TEXT,
  created_by_id TEXT,
  created_at INTEGER NOT NULL,
  reason TEXT,
  file_relative_path TEXT NOT NULL,
  file_hash BLOB NOT NULL,
  PRIMARY KEY (note_id, scope_kind, scope_id, override_type)
);
INSERT INTO note_overrides (note_id, vault_id, scope_kind, scope_id, override_type, schema_version, payload_toml, priority, created_by_kind, created_by_id, created_at, reason, file_relative_path, file_hash)
  SELECT note_id, vault_id, scope_kind, scope_id, override_type, schema_version, payload_toml, priority, created_by_kind, created_by_id, created_at, reason, file_relative_path, file_hash FROM note_overrides_down_0034;
DROP TABLE note_overrides_down_0034;
CREATE INDEX idx_note_overrides_type ON note_overrides(override_type);
CREATE INDEX idx_note_overrides_vault ON note_overrides(vault_id, scope_kind, scope_id);
CREATE INDEX idx_note_overrides_priority ON note_overrides(note_id, override_type, priority DESC);

PRAGMA legacy_alter_table=OFF;
PRAGMA foreign_keys=ON;

DELETE FROM _schema_migrations WHERE version = '0034_child_tables_composite_pk';
