-- Rollback manuel de 0036 (Task 15, Groupe B) — **NON auto-exécuté** par le runner.
--
-- Le runner de migrations est forward-only (`_schema_migrations` ne trace que les versions
-- appliquées, pas de mécanisme `down`). Ce script est le rollback documenté à appliquer
-- MANUELLEMENT (via `sqlite3 <db> < 0036_override_locus_bearer_vault.down.sql`) si un rollback
-- LIVE de 0036 est nécessaire — par exemple avant un redéploiement du binaire pré-Task 15,
-- dont le read/write des overrides Locus/Bearer attend encore la sentinelle '_unset'.
--
-- Il re-bascule le `vault_id` des overrides Locus/Bearer du vault mono-tenant 'main' vers la
-- sentinelle '_unset', puis retire la ligne de registre `_schema_migrations` pour que le
-- runner ré-applique 0036 au prochain démarrage d'un binaire post-Task 15.
--
-- ## Réserve d'irréversibilité (documentée)
--
-- 0036 backfille '_unset' → 'main' pour les scopes Locus/Bearer. En régime mono-vault legacy
-- (multi_tenant OFF) cet ensemble se confond avec l'ensemble total des overrides Locus/Bearer
-- de 'main', donc la re-bascule est exacte. Si des overrides Locus/Bearer GENUINEMENT scopés
-- 'main' ont été écrits par un binaire post-Task 15 (write-path bindant le vault réel), ce
-- down les re-basculera aussi vers '_unset' — comportement volontaire (retour à la sémantique
-- pré-Task 15) mais non distinguable des lignes backfillées. Réserve identique en nature à
-- celle des down 0034/0035.

UPDATE note_overrides
   SET vault_id = '_unset'
 WHERE vault_id = 'main'
   AND scope_kind IN ('locus', 'bearer');

DELETE FROM _schema_migrations WHERE version = '0036_override_locus_bearer_vault';
