-- Migration 0033 — `vault_id NOT NULL` sur les tables filles à clé `note_id` (C4-1e, Slice D2).
--
-- ## Root-cause / objectif
--
-- Trois tables filles de `notes` portaient encore une clé purement `note_id` sans dimension
-- de tenant : `note_embeddings` (PK `(note_id, embedder_id)`), `note_history`
-- (PK `(note_id, to_version)`) et `note_audit_trail` (PK `id`, ligne-unique). Depuis la PK
-- composite `(vault_id, id)` de `notes` (migration 0032), deux vaults distincts peuvent
-- porter des notes de MÊME ULID. Sans colonne `vault_id` sur ces enfants :
--   * `note_embeddings` : une collision `(note_id, embedder_id)` cross-vault laissait un
--     `ON CONFLICT` clobber l'embedding de `main` avec celui d'un tenant tiers ;
--   * la cascade `delete_note_from_index` ne pouvait pas scoper la suppression (D2.3) —
--     supprimer la note d'un vault effaçait les lignes filles homonymes de l'autre.
--
-- Cette migration ajoute `vault_id NOT NULL` à ces trois tables et recompose leurs clés :
--   * `note_embeddings` : PK → `(note_id, embedder_id, vault_id)` (unicité par tenant) ;
--   * `note_history`     : PK → `(note_id, to_version, vault_id)` ;
--   * `note_audit_trail` : PK reste `id` (AUTOINCREMENT, ligne-unique — pas de write-collision
--     possible) ; `vault_id` sert uniquement au scoping de la cascade.
--
-- ## Data-safety
--
-- Données actuelles = `vault_id = 'main'` uniquement (flag multi-tenant OFF). Le backfill
-- résout `vault_id` via un sous-SELECT sur `notes` par `note_id` : en mono-vault chaque
-- `note_id` mappe une unique ligne `notes` → résolution non ambiguë, 0 NULL (sanity : 0
-- orphelin, chaque ligne fille a une note parente). `(main, …)` ≡ ancienne clé → zéro perte,
-- zéro changement observable (byte-identical flag OFF). Copie 1:1 par colonnes explicites.
--
-- ## Choréographie SQLite (recreate) — identique à 0032
--
-- `PRAGMA foreign_keys=OFF` (le runner n'ouvre pas de transaction englobante) +
-- `PRAGMA legacy_alter_table=ON` (empêche `ALTER TABLE RENAME` de réécrire les références
-- des autres tables — pattern 12-steps SQLite). Recreate table + INSERT SELECT explicite +
-- DROP de la table `_old_0033`. PAS de rebuild FTS : ces trois tables ne sont pas
-- external-content FTS (aucun rowid dérivé de `notes`). PRAGMAs restaurés à la fin.
--
-- ## Rollback
--
-- Le runner est forward-only. Rollback manuel documenté : `0033_child_tables_vault_id.down.sql`
-- (non auto-exécuté ; reconstruit le schéma pré-0033 sans `vault_id`).

PRAGMA foreign_keys=OFF;
PRAGMA legacy_alter_table=ON;

-- ── 1. note_audit_trail → ADD vault_id NOT NULL, PK inchangée (`id`) ─────────────
ALTER TABLE note_audit_trail RENAME TO note_audit_trail_old_0033;
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
  SELECT id, note_id, event_type, actor_kind, actor_id, payload_json, occurred_at, correlation_id,
         (SELECT n.vault_id FROM notes n WHERE n.id = note_audit_trail_old_0033.note_id)
  FROM note_audit_trail_old_0033;
DROP TABLE note_audit_trail_old_0033;
CREATE INDEX idx_audit_note_id ON note_audit_trail(note_id, occurred_at DESC);
CREATE INDEX idx_audit_correlation ON note_audit_trail(correlation_id);

-- ── 2. note_embeddings → ADD vault_id NOT NULL, PK (note_id, embedder_id, vault_id) ─
ALTER TABLE note_embeddings RENAME TO note_embeddings_old_0033;
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
  SELECT note_id, embedder_id, vector, dim, model_version, computed_at,
         (SELECT n.vault_id FROM notes n WHERE n.id = note_embeddings_old_0033.note_id)
  FROM note_embeddings_old_0033;
DROP TABLE note_embeddings_old_0033;

-- ── 3. note_history → ADD vault_id NOT NULL, PK (note_id, to_version, vault_id) ───
ALTER TABLE note_history RENAME TO note_history_old_0033;
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
  SELECT note_id, from_version, to_version, diff_text, committed_at, committed_by_kind, committed_by_id, commit_message, correlation_id,
         (SELECT n.vault_id FROM notes n WHERE n.id = note_history_old_0033.note_id)
  FROM note_history_old_0033;
DROP TABLE note_history_old_0033;

PRAGMA legacy_alter_table=OFF;
PRAGMA foreign_keys=ON;

INSERT INTO _schema_migrations (version, applied_at)
  VALUES ('0033_child_tables_vault_id', strftime('%s', 'now') * 1000);
