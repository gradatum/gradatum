-- Rollback manuel de 0037 (Groupe B, W-idx) — **NON auto-exécuté** par le runner.
--
-- Le runner de migrations est forward-only (`_schema_migrations` ne trace que les versions
-- appliquées, pas de mécanisme `down`). Ce script est le rollback documenté à appliquer
-- MANUELLEMENT (via `sqlite3 <db> < 0037_archive_active_vault_scope.down.sql`) si un
-- rollback LIVE de 0037 est nécessaire — par exemple avant un redéploiement d'un binaire
-- pré-W-idx.
--
-- Il restaure l'unique partiel GLOBAL sur `note_id` seul (schéma 0028). En mono-vault
-- `'main'`, chaque ULID a au plus une archive active, donc la projection
-- `(vault_id, note_id) → note_id` ne viole aucune unicité au rebuild. Si des archives
-- actives de plusieurs vaults partagent un même `note_id` (régime multi-vault, hors flag
-- OFF), ce rollback lèverait une violation d'unicité au CREATE INDEX — c'est intentionnel :
-- revenir à un index mono-tenant alors que des données multi-vault existent réintroduit le
-- DoS et refuse la contrainte (ne jamais forcer le rollback dans ce cas).
--
-- La ligne de registre `_schema_migrations` est retirée pour que le runner ré-applique 0037
-- au prochain démarrage d'un binaire post-W-idx.

DROP INDEX IF EXISTS uidx_archive_active;
CREATE UNIQUE INDEX IF NOT EXISTS uidx_archive_active
  ON archive_index(note_id) WHERE gc_at IS NULL AND restored_at IS NULL;

DELETE FROM _schema_migrations WHERE version = '0037_archive_active_vault_scope';
