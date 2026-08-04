-- Migration 0037 — `uidx_archive_active` recomposé en `(vault_id, note_id)` (Groupe B, W-idx).
--
-- ## Root-cause / objectif
--
-- `uidx_archive_active` (migration 0028) est l'unique partiel garantissant « au plus UNE
-- archive active par note » :
--   CREATE UNIQUE INDEX uidx_archive_active
--     ON archive_index(note_id) WHERE gc_at IS NULL AND restored_at IS NULL;
--
-- La clé est GLOBALE sur `note_id` seul, alors que la table porte `vault_id` (0028) et que
-- `notes` a été recomposée en PK composite `(vault_id, id)` (0032). Au régime multi-vault :
--   * si vault-A détient une archive ACTIVE de l'ULID X, alors vault-B archivant le MÊME
--     ULID X voit son `insert_archive_entry` rejeté par l'unicité — **DoS cross-vault**
--     (un vault empêche l'archivage d'un ULID homonyme dans un autre vault).
--
-- Ce n'est PAS une fuite (Task 20 a scopé lectures et mutations par `vault_id` :
-- `get_active_archive`/`mark_archive_gc`/`mark_archive_restored` filtrent `note_id ET
-- vault_id`) — c'est un défaut de disponibilité/correction multi-vault, masqué mono-vault.
--
-- Cette migration recompose l'unique partiel en `(vault_id, note_id)` : au plus UNE archive
-- active PAR VAULT et PAR ULID. Cohérent avec 0032/0033/0034/0035 (dimension `vault_id`).
--
-- ## Data-safety / byte-identical mono-vault
--
-- Simple rebuild d'index (DROP INDEX + CREATE UNIQUE INDEX) — pas de recreate de table
-- (SQLite modifie les index in-place). L'ancien index garantissait déjà « au plus une
-- archive active par ULID globalement » ; en mono-vault `main`, toutes les lignes actives
-- ont `vault_id = 'main'`, donc `(main, note_id)` ≡ ancienne clé `note_id` : la construction
-- du nouvel index sur les données existantes ne peut PAS lever de collision d'unicité (le
-- sur-ensemble d'unicité ancien implique l'unicité composite). Zéro changement observable,
-- byte-identical flag OFF. Le prédicat partiel `WHERE gc_at IS NULL AND restored_at IS NULL`
-- est conservé à l'identique (seules les archives actives sont contraintes).
--
-- ## Rollback
--
-- Runner forward-only. Rollback manuel documenté : `0037_archive_active_vault_scope.down.sql`
-- (restaure l'unique global sur `note_id` seul).

DROP INDEX IF EXISTS uidx_archive_active;
CREATE UNIQUE INDEX IF NOT EXISTS uidx_archive_active
  ON archive_index(vault_id, note_id) WHERE gc_at IS NULL AND restored_at IS NULL;

INSERT INTO _schema_migrations (version, applied_at)
  VALUES ('0037_archive_active_vault_scope', strftime('%s', 'now') * 1000);
