-- Schéma Phase 1 (spec v3.2 §5.2 + caveats infra-reviewer C1/C3 + B20 generic note_overrides)
-- Colonne extra_json TEXT : ExtraFields sérialisée en JSON (serde_json) plutôt que YAML
-- pour éviter les problèmes de round-trip toml::Value via serde_yaml. Changement documenté.

-- ─ Notes ──────────────────────────────────────────────────────
CREATE TABLE notes (
  id TEXT PRIMARY KEY,                  -- ULID
  vault_id TEXT NOT NULL,
  locus TEXT,
  section TEXT NOT NULL,
  status TEXT NOT NULL,
  schema_version INTEGER NOT NULL,
  author_kind TEXT,
  author_id TEXT,
  author_display_name TEXT,
  created INTEGER NOT NULL,             -- unix epoch ms
  updated INTEGER,
  status_changed INTEGER,
  status_reason TEXT,
  content_hash BLOB NOT NULL,           -- 32 bytes sha256
  version INTEGER NOT NULL DEFAULT 1,
  body_text TEXT NOT NULL,              -- body Markdown pour FTS5/BM25 cache
  integrity_signature BLOB,             -- Phase 2+ NULL dans Phase 1
  extra_json TEXT                       -- ExtraFields sérialisée JSON (serde_json)
);
CREATE INDEX idx_notes_vault_locus_status ON notes(vault_id, locus, status);
CREATE INDEX idx_notes_section ON notes(section);
CREATE INDEX idx_notes_author ON notes(author_kind, author_id);
CREATE INDEX idx_notes_updated ON notes(updated DESC);

-- ─ Audit trail (1:N append-only) ─────────────────────────────
CREATE TABLE note_audit_trail (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  event_type TEXT NOT NULL,
  actor_kind TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  payload_json TEXT,                    -- AuditEventType variant payload (serde_json::to_string)
  occurred_at INTEGER NOT NULL,
  correlation_id TEXT
);
CREATE INDEX idx_audit_note_id ON note_audit_trail(note_id, occurred_at DESC);
CREATE INDEX idx_audit_correlation ON note_audit_trail(correlation_id);

-- ─ Index entries (1:1, computed by gradatum-index) ───────────
CREATE TABLE note_index (
  note_id TEXT PRIMARY KEY REFERENCES notes(id) ON DELETE CASCADE,
  vault_id TEXT NOT NULL,
  locus TEXT,
  bm25_tokens INTEGER NOT NULL,
  last_indexed INTEGER NOT NULL
);
CREATE VIRTUAL TABLE notes_fts USING fts5(body_text, tags, content=notes, tokenize='unicode61');

-- ─ Embeddings (1:N par modèle, Phase 1 stub schema, Phase 2 impl) ─
CREATE TABLE note_embeddings (
  note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  embedder_id TEXT NOT NULL,
  vector BLOB NOT NULL,
  dim INTEGER NOT NULL,
  model_version TEXT,
  computed_at INTEGER NOT NULL,
  PRIMARY KEY (note_id, embedder_id)
);

-- ─ Generic overrides (table unique B20 / Q7) ─────────────────
CREATE TABLE note_overrides (
  note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  vault_id TEXT NOT NULL,
  scope_kind TEXT NOT NULL,             -- vault | locus | bearer
  scope_id TEXT NOT NULL,
  override_type TEXT NOT NULL,          -- "metadata" P1, "acl" P2, "index" P3, "score" P4, ...
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
CREATE INDEX idx_note_overrides_type ON note_overrides(override_type);
CREATE INDEX idx_note_overrides_vault ON note_overrides(vault_id, scope_kind, scope_id);
CREATE INDEX idx_note_overrides_priority ON note_overrides(note_id, override_type, priority DESC);

-- ─ File checksums (drift detection Phase A — caveat infra-reviewer C3) ─
CREATE TABLE file_checksums (
  relative_path TEXT PRIMARY KEY,
  file_kind TEXT NOT NULL,              -- "note" | "override" | "config"
  expected_size INTEGER NOT NULL,
  expected_hash_prefix_4kb BLOB NOT NULL,  -- 32 bytes sha256(first 4096 bytes)
  expected_hash BLOB NOT NULL,             -- 32 bytes sha256(full file content)
  expected_mtime INTEGER NOT NULL,         -- cosmétique re-anchor only (not primary discriminant)
  last_verified INTEGER NOT NULL
);
CREATE INDEX idx_file_checksums_kind ON file_checksums(file_kind, last_verified);

-- ─ History (Phase 1 scaffold, Phase 2 full impl) ─────────────
CREATE TABLE note_history (
  note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
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

-- ─ Schema migrations tracking ────────────────────────────────
-- NOTE : la table _schema_migrations est créée par le runner (migrations.rs bootstrap).
-- Le script n'insère que la ligne de tracking pour cette version.
INSERT INTO _schema_migrations (version, applied_at) VALUES ('0001_phase1', strftime('%s', 'now') * 1000);
