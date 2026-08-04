-- Migration 0039 — FK composite `(vault_id, note_id) → notes(vault_id, id)` sur les trois
-- tables filles D2 `note_audit_trail`, `note_embeddings`, `note_history` (item pré-flip
-- 01KXV6PJ0X, Option A).
--
-- ## Root-cause / objectif
--
-- La migration 0032 a passé `notes` en PK composite `(vault_id, id)` et RETIRÉ au passage
-- les FK enfants `REFERENCES notes(id)` (devenues invalides : `id` seul n'est plus une clé).
-- La cascade DELETE est depuis MANUELLE (`delete_note_from_index`). Les migrations 0033
-- (colonne `vault_id`) et 0034 (PK composite) ont préparé les enfants, mais le **FK
-- composite** — le garde-fou d'intégrité référentielle — n'a jamais été reposé.
--
-- Cette migration recompose les trois tables filles porteuses de `vault_id` avec un FK
-- composite complet vers la PK de `notes` :
--   * `note_audit_trail` : + FOREIGN KEY (vault_id, note_id) REFERENCES notes(vault_id, id) ;
--   * `note_embeddings`  : idem ;
--   * `note_history`     : idem.
--
-- `PRAGMA foreign_keys = ON` est appliqué au runtime (C12, `sqlite.rs`) → ce FK est
-- EFFECTIF, pas décoratif : il rejette toute insertion enfant orpheline `(vault_id, note_id)`
-- sans note parente, et garantit la cohérence référentielle à la racine.
--
-- ## `ON DELETE CASCADE` — décision (Option ii)
--
-- Le FK est déclaré `ON DELETE CASCADE`. Justification : trois chemins suppriment des lignes
-- `notes` — `delete_note_from_index` (cascade manuelle enfants AVANT parent, scopée), MAIS
-- aussi `write_note_derived_batch` (`DELETE FROM notes WHERE id=?1 AND vault_id=?2`, sans
-- cascade manuelle préalable) et `delete_vault_from_index` (`DELETE FROM notes WHERE
-- vault_id=?1`, idem). Un FK en RESTRICT (sans `ON DELETE CASCADE`) FERAIT ÉCHOUER ces deux
-- derniers si une note à supprimer portait encore un enfant D2 → régression. `ON DELETE
-- CASCADE` supprime les enfants automatiquement : aucune régression, et la cascade manuelle
-- de `delete_note_from_index` reste correcte (elle purge les enfants d'abord ; le DELETE
-- parent cascade alors sur 0 ligne restante — idempotent). Défense en profondeur.
--
-- ## Data-safety
--
-- Données actuelles = `vault_id = 'main'` uniquement (flag multi-tenant OFF), chaque ligne
-- fille a une note parente (invariant 0033 : backfill via sous-SELECT sur `notes`, 0
-- orphelin). Le FK est donc satisfait par construction sur l'existant : `PRAGMA
-- foreign_key_check` renvoie 0 ligne après recreate (prouvé par les tests 0039). SEUL le
-- schéma change (ajout de la contrainte FK) : aucune colonne/PK/index modifié, copie 1:1 par
-- colonnes explicites → zéro perte, zéro changement observable (byte-identical flag OFF).
--
-- ## Choréographie SQLite (recreate) — identique à 0032/0033/0034
--
-- `PRAGMA foreign_keys=OFF` (le runner n'ouvre pas de transaction englobante → le PRAGMA est
-- effectif) + `PRAGMA legacy_alter_table=ON` (empêche `ALTER TABLE RENAME` de réécrire les
-- références des autres objets — pattern 12-steps SQLite). Pour chaque table : recreate avec
-- le FK, `INSERT INTO … SELECT` explicite depuis la table `_old_0039`, DROP de l'old,
-- recréation des index secondaires (perdus au DROP). PAS de rebuild FTS : aucune de ces
-- tables n'est external-content FTS. PRAGMAs restaurés à la fin (`foreign_keys=ON` → le FK
-- neuf devient actif pour les opérations suivantes).
--
-- ## Rollback
--
-- Le runner est forward-only. Rollback manuel documenté : `0039_child_tables_composite_fk.down.sql`
-- (non auto-exécuté ; recompose les trois tables SANS le FK, schéma post-0033).

PRAGMA foreign_keys=OFF;
PRAGMA legacy_alter_table=ON;

-- ── 1. note_audit_trail → + FK (vault_id, note_id) REFERENCES notes(vault_id, id) ─────
-- Schéma post-0033 (PK `id` AUTOINCREMENT, `vault_id NOT NULL`) inchangé + FK composite.
ALTER TABLE note_audit_trail RENAME TO note_audit_trail_old_0039;
CREATE TABLE note_audit_trail (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  note_id TEXT NOT NULL,
  event_type TEXT NOT NULL,
  actor_kind TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  payload_json TEXT,
  occurred_at INTEGER NOT NULL,
  correlation_id TEXT,
  vault_id TEXT NOT NULL,
  FOREIGN KEY (vault_id, note_id) REFERENCES notes(vault_id, id) ON DELETE CASCADE
);
INSERT INTO note_audit_trail (id, note_id, event_type, actor_kind, actor_id, payload_json, occurred_at, correlation_id, vault_id)
  SELECT id, note_id, event_type, actor_kind, actor_id, payload_json, occurred_at, correlation_id, vault_id
  FROM note_audit_trail_old_0039;
DROP TABLE note_audit_trail_old_0039;
CREATE INDEX idx_audit_note_id ON note_audit_trail(note_id, occurred_at DESC);
CREATE INDEX idx_audit_correlation ON note_audit_trail(correlation_id);

-- ── 2. note_embeddings → + FK (vault_id, note_id) REFERENCES notes(vault_id, id) ──────
-- Schéma post-0033 (PK `(note_id, embedder_id, vault_id)`) inchangé + FK composite.
ALTER TABLE note_embeddings RENAME TO note_embeddings_old_0039;
CREATE TABLE note_embeddings (
  note_id TEXT NOT NULL,
  embedder_id TEXT NOT NULL,
  vector BLOB NOT NULL,
  dim INTEGER NOT NULL,
  model_version TEXT,
  computed_at INTEGER NOT NULL,
  vault_id TEXT NOT NULL,
  PRIMARY KEY (note_id, embedder_id, vault_id),
  FOREIGN KEY (vault_id, note_id) REFERENCES notes(vault_id, id) ON DELETE CASCADE
);
INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at, vault_id)
  SELECT note_id, embedder_id, vector, dim, model_version, computed_at, vault_id
  FROM note_embeddings_old_0039;
DROP TABLE note_embeddings_old_0039;

-- ── 3. note_history → + FK (vault_id, note_id) REFERENCES notes(vault_id, id) ─────────
-- Schéma post-0033 (PK `(note_id, to_version, vault_id)`) inchangé + FK composite.
ALTER TABLE note_history RENAME TO note_history_old_0039;
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
  PRIMARY KEY (note_id, to_version, vault_id),
  FOREIGN KEY (vault_id, note_id) REFERENCES notes(vault_id, id) ON DELETE CASCADE
);
INSERT INTO note_history (note_id, from_version, to_version, diff_text, committed_at, committed_by_kind, committed_by_id, commit_message, correlation_id, vault_id)
  SELECT note_id, from_version, to_version, diff_text, committed_at, committed_by_kind, committed_by_id, commit_message, correlation_id, vault_id
  FROM note_history_old_0039;
DROP TABLE note_history_old_0039;

PRAGMA legacy_alter_table=OFF;
PRAGMA foreign_keys=ON;

INSERT INTO _schema_migrations (version, applied_at)
  VALUES ('0039_child_tables_composite_fk', strftime('%s', 'now') * 1000);
