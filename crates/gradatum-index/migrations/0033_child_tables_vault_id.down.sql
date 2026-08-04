-- Rollback manuel de 0033 (C4-1e, Slice D2) — **NON auto-exécuté** par le runner.
--
-- Le runner de migrations est forward-only (`_schema_migrations` ne trace que les versions
-- appliquées, pas de mécanisme `down`). Ce script est le rollback documenté à appliquer
-- MANUELLEMENT (via `sqlite3 <db> < 0033_child_tables_vault_id.down.sql`) si un rollback
-- LIVE de 0033 est nécessaire — par exemple avant un redéploiement du binaire pré-D2.
--
-- Il reconstruit le schéma pré-0033 : retire `vault_id` des trois tables filles, restaure
-- leurs PK d'origine, recopie les données (la dimension `vault_id` est simplement abandonnée —
-- en mono-vault toutes les lignes valent `'main'`, aucune perte d'information), recrée les
-- index secondaires de `note_audit_trail`, et retire la ligne de registre `_schema_migrations`
-- pour que le runner ré-applique 0033 au prochain démarrage d'un binaire post-D2.
--
-- Choréographie identique à 0033/0032 (foreign_keys=OFF + legacy_alter_table=ON).

PRAGMA foreign_keys=OFF;
PRAGMA legacy_alter_table=ON;

-- ── 1. note_audit_trail → retrait vault_id, PK `id` ─────────────────────────────
ALTER TABLE note_audit_trail RENAME TO note_audit_trail_down_0033;
CREATE TABLE note_audit_trail (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  note_id TEXT NOT NULL,
  event_type TEXT NOT NULL,
  actor_kind TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  payload_json TEXT,
  occurred_at INTEGER NOT NULL,
  correlation_id TEXT
);
INSERT INTO note_audit_trail (id, note_id, event_type, actor_kind, actor_id, payload_json, occurred_at, correlation_id)
  SELECT id, note_id, event_type, actor_kind, actor_id, payload_json, occurred_at, correlation_id
  FROM note_audit_trail_down_0033;
DROP TABLE note_audit_trail_down_0033;
CREATE INDEX idx_audit_note_id ON note_audit_trail(note_id, occurred_at DESC);
CREATE INDEX idx_audit_correlation ON note_audit_trail(correlation_id);

-- ── 2. note_embeddings → retrait vault_id, PK (note_id, embedder_id) ─────────────
ALTER TABLE note_embeddings RENAME TO note_embeddings_down_0033;
CREATE TABLE note_embeddings (
  note_id TEXT NOT NULL,
  embedder_id TEXT NOT NULL,
  vector BLOB NOT NULL,
  dim INTEGER NOT NULL,
  model_version TEXT,
  computed_at INTEGER NOT NULL,
  PRIMARY KEY (note_id, embedder_id)
);
INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at)
  SELECT note_id, embedder_id, vector, dim, model_version, computed_at
  FROM note_embeddings_down_0033;
DROP TABLE note_embeddings_down_0033;

-- ── 3. note_history → retrait vault_id, PK (note_id, to_version) ─────────────────
ALTER TABLE note_history RENAME TO note_history_down_0033;
CREATE TABLE note_history (
  note_id TEXT NOT NULL,
  from_version INTEGER NOT NULL,
  to_version INTEGER NOT NULL,
  diff_text TEXT NOT NULL,
  committed_at INTEGER NOT NULL,
  committed_by_kind TEXT,
  committed_by_id TEXT,
  commit_message TEXT,
  correlation_id TEXT,
  PRIMARY KEY (note_id, to_version)
);
INSERT INTO note_history (note_id, from_version, to_version, diff_text, committed_at, committed_by_kind, committed_by_id, commit_message, correlation_id)
  SELECT note_id, from_version, to_version, diff_text, committed_at, committed_by_kind, committed_by_id, commit_message, correlation_id
  FROM note_history_down_0033;
DROP TABLE note_history_down_0033;

PRAGMA legacy_alter_table=OFF;
PRAGMA foreign_keys=ON;

DELETE FROM _schema_migrations WHERE version = '0033_child_tables_vault_id';
