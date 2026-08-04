-- Migration 0032 — clé d'identité composite (vault_id, id) sur `notes` (C4-1d, P0/P1 security review).
--
-- ## Root-cause
--
-- `notes.id` était `PRIMARY KEY` SEUL → une seule ligne par ULID. En multi-vault (flag ON), une
-- collision d'ULID cross-vault permettait à un tenant tiers de clobber la ligne `notes` de `main`
-- (ON CONFLICT(id) DO UPDATE) ET son entrée `notes_fts` (external-content, rowid dérivé de la ligne
-- unique). Passer `notes` en PK composite `(vault_id, id)` donne à CHAQUE vault sa propre ligne +
-- rowid → collision impossible par construction sur les couches rowid-dérivées (notes_fts, futures).
--
-- ## Option C (GO Tech Lead 2026-07-18)
--
-- Les FK enfants `REFERENCES notes(id)` (foreign_keys=ON) deviendraient invalides (`id` seul n'est
-- plus une clé). On les RETIRE (recreate sans FK) et la cascade DELETE devient MANUELLE (explicite
-- dans `delete_note_from_index`). Le composite FK complet sur les enfants (option A) est un
-- prérequis pré-2e-vault-writable tracé séparément.
--
-- ## Data-safety
--
-- Données actuelles = `vault_id = 'main'` uniquement → `(main, id)` ≡ `id`. Zéro perte, zéro
-- changement observable (byte-identical). Copie 1:1 par colonnes explicites.
--
-- ## Choreographie SQLite (recreate)
--
-- `PRAGMA foreign_keys=OFF` (effectif : le runner n'ouvre pas de transaction englobante) +
-- `PRAGMA legacy_alter_table=ON` (empêche `ALTER TABLE RENAME` de réécrire auto les références
-- FK/FTS des autres tables — pattern 12-steps SQLite). Rebuild FTS en fin (les rowids de `notes`
-- changent au recreate). PRAGMAs restaurés à la fin.

PRAGMA foreign_keys=OFF;
PRAGMA legacy_alter_table=ON;

-- ── 1. notes → PK composite (vault_id, id) ; replaced_by SANS FK ────────────────
ALTER TABLE notes RENAME TO notes_old_0032;

CREATE TABLE notes (
  id TEXT NOT NULL,
  vault_id TEXT NOT NULL,
  locus TEXT,
  section TEXT NOT NULL,
  status TEXT NOT NULL,
  schema_version INTEGER NOT NULL,
  author_kind TEXT,
  author_id TEXT,
  author_display_name TEXT,
  created INTEGER NOT NULL,
  updated INTEGER,
  status_changed INTEGER,
  status_reason TEXT,
  content_hash BLOB NOT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  body_text TEXT NOT NULL,
  integrity_signature BLOB,
  extra_json TEXT,
  tags TEXT,
  replaced_by TEXT,
  title TEXT,
  c_kind TEXT,
  doc_kind TEXT,
  provenance TEXT,
  trust REAL,
  forgotten INTEGER NOT NULL DEFAULT 0,
  forgotten_at INTEGER,
  forgotten_by TEXT,
  orphaned INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (vault_id, id)
);

INSERT INTO notes (
  id, vault_id, locus, section, status, schema_version,
  author_kind, author_id, author_display_name,
  created, updated, status_changed, status_reason,
  content_hash, version, body_text, integrity_signature, extra_json,
  tags, replaced_by, title, c_kind, doc_kind, provenance, trust,
  forgotten, forgotten_at, forgotten_by, orphaned
)
SELECT
  id, vault_id, locus, section, status, schema_version,
  author_kind, author_id, author_display_name,
  created, updated, status_changed, status_reason,
  content_hash, version, body_text, integrity_signature, extra_json,
  tags, replaced_by, title, c_kind, doc_kind, provenance, trust,
  forgotten, forgotten_at, forgotten_by, orphaned
FROM notes_old_0032;

DROP TABLE notes_old_0032;

-- Index de `notes` (recréés à l'identique — perdus au DROP).
CREATE INDEX idx_notes_vault_locus_status ON notes(vault_id, locus, status);
CREATE INDEX idx_notes_section ON notes(section);
CREATE INDEX idx_notes_author ON notes(author_kind, author_id);
CREATE INDEX idx_notes_updated ON notes(updated DESC);
CREATE INDEX idx_notes_c_kind ON notes(c_kind);
CREATE INDEX idx_notes_doc_kind ON notes(doc_kind);
CREATE INDEX idx_notes_trust ON notes(trust);
CREATE INDEX idx_notes_provenance ON notes(provenance);
CREATE INDEX idx_notes_status_downgrade ON notes(status, status_changed)
  WHERE status = 'downgraded';
CREATE INDEX idx_notes_title ON notes(vault_id, title);
CREATE INDEX idx_notes_forgotten ON notes(forgotten) WHERE forgotten = 1;

-- ── 2. Enfants : retrait des FK `REFERENCES notes(id)` (cascade → manuelle) ─────

-- note_audit_trail
ALTER TABLE note_audit_trail RENAME TO note_audit_trail_old_0032;
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
  SELECT id, note_id, event_type, actor_kind, actor_id, payload_json, occurred_at, correlation_id FROM note_audit_trail_old_0032;
DROP TABLE note_audit_trail_old_0032;
CREATE INDEX idx_audit_note_id ON note_audit_trail(note_id, occurred_at DESC);
CREATE INDEX idx_audit_correlation ON note_audit_trail(correlation_id);

-- note_index
ALTER TABLE note_index RENAME TO note_index_old_0032;
CREATE TABLE note_index (
  note_id TEXT PRIMARY KEY,
  vault_id TEXT NOT NULL,
  locus TEXT,
  bm25_tokens INTEGER NOT NULL,
  last_indexed INTEGER NOT NULL
);
INSERT INTO note_index (note_id, vault_id, locus, bm25_tokens, last_indexed)
  SELECT note_id, vault_id, locus, bm25_tokens, last_indexed FROM note_index_old_0032;
DROP TABLE note_index_old_0032;

-- note_embeddings
ALTER TABLE note_embeddings RENAME TO note_embeddings_old_0032;
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
  SELECT note_id, embedder_id, vector, dim, model_version, computed_at FROM note_embeddings_old_0032;
DROP TABLE note_embeddings_old_0032;

-- note_overrides
ALTER TABLE note_overrides RENAME TO note_overrides_old_0032;
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
  SELECT note_id, vault_id, scope_kind, scope_id, override_type, schema_version, payload_toml, priority, created_by_kind, created_by_id, created_at, reason, file_relative_path, file_hash FROM note_overrides_old_0032;
DROP TABLE note_overrides_old_0032;
CREATE INDEX idx_note_overrides_type ON note_overrides(override_type);
CREATE INDEX idx_note_overrides_vault ON note_overrides(vault_id, scope_kind, scope_id);
CREATE INDEX idx_note_overrides_priority ON note_overrides(note_id, override_type, priority DESC);

-- note_history
ALTER TABLE note_history RENAME TO note_history_old_0032;
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
  SELECT note_id, from_version, to_version, diff_text, committed_at, committed_by_kind, committed_by_id, commit_message, correlation_id FROM note_history_old_0032;
DROP TABLE note_history_old_0032;

-- note_links (FK src_note_id → notes(id) retirée)
ALTER TABLE note_links RENAME TO note_links_old_0032;
CREATE TABLE note_links (
  src_note_id TEXT NOT NULL,
  dst_note_id TEXT NOT NULL,
  vault_id    TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (src_note_id, dst_note_id, vault_id)
);
INSERT INTO note_links (src_note_id, dst_note_id, vault_id, created_at)
  SELECT src_note_id, dst_note_id, vault_id, created_at FROM note_links_old_0032;
DROP TABLE note_links_old_0032;
CREATE INDEX idx_note_links_dst ON note_links (dst_note_id, vault_id);
CREATE INDEX idx_note_links_src ON note_links (src_note_id, vault_id);

-- ── 3. Rebuild FTS (external-content) : les rowids de `notes` ont changé ────────
INSERT INTO notes_fts(notes_fts) VALUES('rebuild');

PRAGMA legacy_alter_table=OFF;
PRAGMA foreign_keys=ON;

INSERT INTO _schema_migrations (version, applied_at)
  VALUES ('0032_notes_composite_pk', strftime('%s', 'now') * 1000);
