-- Migration 0034 — clés primaires composites `(vault_id, …)` sur les tables filles
-- `note_index`, `temporal_index`, `note_overrides` (C4-1e, Slice D3).
--
-- ## Root-cause / objectif
--
-- Ces trois tables portent DÉJÀ une colonne `vault_id NOT NULL` (ajoutée à leur création
-- ou en migration antérieure), mais leur PRIMARY KEY n'inclut PAS `vault_id` :
--   * `note_index`     : PK `note_id` seul ;
--   * `temporal_index` : PK `note_id` seul ;
--   * `note_overrides` : PK `(note_id, scope_kind, scope_id, override_type)`.
--
-- Depuis la PK composite `(vault_id, id)` de la table parente (migration 0032), deux vaults
-- distincts peuvent porter des notes de MÊME ULID. Avec une PK fille sans dimension de tenant,
-- un `note_id` colliding cross-vault provoque une write-collision : un `INSERT OR REPLACE`
-- (`temporal_index`) ou un `ON CONFLICT DO UPDATE` (`note_overrides`) déclenché par un vault
-- tiers clobbe la ligne homonyme d'un autre vault. Cette migration recompose les PK pour
-- inclure `vault_id`, fermant la classe write-collision à la racine (miroir de 0032/0033) :
--   * `note_index`     : PK → `(vault_id, note_id)` ;
--   * `temporal_index` : PK → `(vault_id, note_id)` ;
--   * `note_overrides` : PK → `(vault_id, note_id, scope_kind, scope_id, override_type)`.
--
-- ## Data-safety
--
-- SEULE la PK change : aucune colonne ajoutée/retirée, copie 1:1 par colonnes explicites.
-- Données actuelles = `vault_id = 'main'` uniquement (flag multi-tenant OFF) → en mono-vault
-- chaque `note_id` est unique, donc `(main, note_id)` ≡ ancienne clé `note_id` : zéro perte,
-- zéro collision au copy, zéro changement observable (byte-identical flag OFF).
--
-- ## Choréographie SQLite (recreate) — identique à 0032/0033
--
-- `PRAGMA foreign_keys=OFF` (le runner n'ouvre pas de transaction englobante → le PRAGMA est
-- effectif) + `PRAGMA legacy_alter_table=ON` (empêche `ALTER TABLE RENAME` de réécrire les
-- références des autres objets — pattern 12-steps SQLite). Pour chaque table : recreate avec
-- la nouvelle PK, `INSERT INTO … SELECT` explicite depuis la table `_old_0034`, DROP de l'old,
-- recréation des index secondaires (perdus au DROP). PAS de rebuild FTS : aucune de ces tables
-- n'est external-content FTS (leur rowid n'est pas dérivé de `notes`). PRAGMAs restaurés à la fin.
--
-- ## Rollback
--
-- Le runner est forward-only. Rollback manuel documenté : `0034_child_tables_composite_pk.down.sql`
-- (non auto-exécuté ; restaure les PK d'origine sans `vault_id` dans la clé).

PRAGMA foreign_keys=OFF;
PRAGMA legacy_alter_table=ON;

-- ── 1. note_index → PK (vault_id, note_id) ──────────────────────────────────────
ALTER TABLE note_index RENAME TO note_index_old_0034;
CREATE TABLE note_index (
  note_id TEXT NOT NULL,
  vault_id TEXT NOT NULL,
  locus TEXT,
  bm25_tokens INTEGER NOT NULL,
  last_indexed INTEGER NOT NULL,
  PRIMARY KEY (vault_id, note_id)
);
INSERT INTO note_index (note_id, vault_id, locus, bm25_tokens, last_indexed)
  SELECT note_id, vault_id, locus, bm25_tokens, last_indexed FROM note_index_old_0034;
DROP TABLE note_index_old_0034;

-- ── 2. temporal_index → PK (vault_id, note_id) ──────────────────────────────────
ALTER TABLE temporal_index RENAME TO temporal_index_old_0034;
CREATE TABLE temporal_index (
  note_id TEXT NOT NULL,
  vault_id TEXT NOT NULL,
  anchor_ms INTEGER NOT NULL,
  anchor_src TEXT NOT NULL,
  doc_kind TEXT NOT NULL,
  valid_until_ms INTEGER,
  PRIMARY KEY (vault_id, note_id)
);
INSERT INTO temporal_index (note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms)
  SELECT note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms FROM temporal_index_old_0034;
DROP TABLE temporal_index_old_0034;
CREATE INDEX idx_temporal_anchor ON temporal_index(anchor_ms);
CREATE INDEX idx_temporal_vault_anchor ON temporal_index(vault_id, anchor_ms);

-- ── 3. note_overrides → PK (vault_id, note_id, scope_kind, scope_id, override_type) ─
ALTER TABLE note_overrides RENAME TO note_overrides_old_0034;
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
  PRIMARY KEY (vault_id, note_id, scope_kind, scope_id, override_type)
);
INSERT INTO note_overrides (note_id, vault_id, scope_kind, scope_id, override_type, schema_version, payload_toml, priority, created_by_kind, created_by_id, created_at, reason, file_relative_path, file_hash)
  SELECT note_id, vault_id, scope_kind, scope_id, override_type, schema_version, payload_toml, priority, created_by_kind, created_by_id, created_at, reason, file_relative_path, file_hash FROM note_overrides_old_0034;
DROP TABLE note_overrides_old_0034;
CREATE INDEX idx_note_overrides_type ON note_overrides(override_type);
CREATE INDEX idx_note_overrides_vault ON note_overrides(vault_id, scope_kind, scope_id);
CREATE INDEX idx_note_overrides_priority ON note_overrides(note_id, override_type, priority DESC);

PRAGMA legacy_alter_table=OFF;
PRAGMA foreign_keys=ON;

INSERT INTO _schema_migrations (version, applied_at)
  VALUES ('0034_child_tables_composite_pk', strftime('%s', 'now') * 1000);
