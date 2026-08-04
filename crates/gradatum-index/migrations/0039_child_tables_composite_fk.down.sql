-- Rollback manuel de 0039 (item pré-flip 01KXV6PJ0X, Option A) — **NON auto-exécuté** par le runner.
--
-- Le runner de migrations est forward-only (`_schema_migrations` ne trace que les versions
-- appliquées, pas de mécanisme `down`). Ce script est le rollback documenté à appliquer
-- MANUELLEMENT (via `sqlite3 <db> < 0039_child_tables_composite_fk.down.sql`) si un rollback
-- LIVE de 0039 est nécessaire — par exemple avant un redéploiement du binaire pré-0039.
--
-- Il recompose les trois tables filles D2 SANS le FK composite (schéma post-0033 : colonnes,
-- PK et index secondaires strictement identiques à l'état laissé par 0033), recopie les
-- données 1:1, recrée les index secondaires de `note_audit_trail`, et retire la ligne de
-- registre `_schema_migrations` pour que le runner ré-applique 0039 au prochain démarrage
-- d'un binaire post-0039.
--
-- Choréographie identique à 0039 (foreign_keys=OFF + legacy_alter_table=ON).

PRAGMA foreign_keys=OFF;
PRAGMA legacy_alter_table=ON;

-- ── 1. note_audit_trail → retrait du FK, schéma post-0033 (PK `id`) ──────────────
ALTER TABLE note_audit_trail RENAME TO note_audit_trail_down_0039;
CREATE TABLE note_audit_trail (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  note_id TEXT NOT NULL,
  event_type TEXT NOT NULL,
  actor_kind TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  payload_json TEXT,
  occurred_at INTEGER NOT NULL,
  correlation_id TEXT,
  vault_id TEXT NOT NULL
);
INSERT INTO note_audit_trail (id, note_id, event_type, actor_kind, actor_id, payload_json, occurred_at, correlation_id, vault_id)
  SELECT id, note_id, event_type, actor_kind, actor_id, payload_json, occurred_at, correlation_id, vault_id
  FROM note_audit_trail_down_0039;
DROP TABLE note_audit_trail_down_0039;
CREATE INDEX idx_audit_note_id ON note_audit_trail(note_id, occurred_at DESC);
CREATE INDEX idx_audit_correlation ON note_audit_trail(correlation_id);

-- ── 2. note_embeddings → retrait du FK, schéma post-0033 (PK `(note_id, embedder_id, vault_id)`) ─
ALTER TABLE note_embeddings RENAME TO note_embeddings_down_0039;
CREATE TABLE note_embeddings (
  note_id TEXT NOT NULL,
  embedder_id TEXT NOT NULL,
  vector BLOB NOT NULL,
  dim INTEGER NOT NULL,
  model_version TEXT,
  computed_at INTEGER NOT NULL,
  vault_id TEXT NOT NULL,
  PRIMARY KEY (note_id, embedder_id, vault_id)
);
INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at, vault_id)
  SELECT note_id, embedder_id, vector, dim, model_version, computed_at, vault_id
  FROM note_embeddings_down_0039;
DROP TABLE note_embeddings_down_0039;

-- ── 3. note_history → retrait du FK, schéma post-0033 (PK `(note_id, to_version, vault_id)`) ─
ALTER TABLE note_history RENAME TO note_history_down_0039;
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
  vault_id TEXT NOT NULL,
  PRIMARY KEY (note_id, to_version, vault_id)
);
INSERT INTO note_history (note_id, from_version, to_version, diff_text, committed_at, committed_by_kind, committed_by_id, commit_message, correlation_id, vault_id)
  SELECT note_id, from_version, to_version, diff_text, committed_at, committed_by_kind, committed_by_id, commit_message, correlation_id, vault_id
  FROM note_history_down_0039;
DROP TABLE note_history_down_0039;

PRAGMA legacy_alter_table=OFF;
PRAGMA foreign_keys=ON;

DELETE FROM _schema_migrations WHERE version = '0039_child_tables_composite_fk';
