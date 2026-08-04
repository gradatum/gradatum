-- Migration 0036 — backfill du `vault_id` des overrides Locus/Bearer legacy (Task 15, Groupe B).
--
-- ## Root-cause / objectif
--
-- Avant Task 15, `upsert_override_raw` persistait les scopes `OverrideScope::Locus` et
-- `OverrideScope::Bearer` avec une sentinelle `vault_id = '_unset'` (bucket GLOBAL, partagé
-- par tous les vaults). Sous la PK composite `(vault_id, note_id, scope_kind, scope_id,
-- override_type)` de `note_overrides` (migration 0034), deux vaults distincts portant un
-- override Locus/Bearer au MÊME `note_id` (ULID collisionné) et au MÊME `scope_id`
-- collisionnaient sur la clé `('_unset', note, 'locus'|'bearer', scope_id, type)` → clobber
-- (write) + cross-read (read). Le model-change lie désormais le `vault` réel à `vault_id`.
--
-- Cette migration re-clé les lignes legacy laissées à `'_unset'` vers le vault mono-tenant
-- `'main'` (seul vault existant tant que `multi_tenant.enabled` reste OFF), pour qu'elles
-- restent lisibles par le nouveau read-path (qui bind le `vault` réel, plus jamais `'_unset'`).
--
-- ## Data-safety
--
-- SEULE la valeur de la colonne `vault_id` change ('_unset' → 'main') pour les lignes
-- Locus/Bearer ; aucune colonne ajoutée/retirée, aucun changement de schéma. Les overrides
-- de scope `Vault` n'ont JAMAIS utilisé '_unset' (leur `vault_id` == le vault visé) → non
-- concernés. En régime mono-vault legacy, toutes les lignes '_unset' appartiennent à `main`
-- et aucune ligne `('main', note, 'locus'|'bearer', scope_id, type)` préexistante ne peut
-- entrer en collision (le legacy n'écrivait jamais 'main' pour ces scopes) → UPDATE sûr,
-- sans violation de PK. Le garde `scope_kind IN ('locus', 'bearer')` est explicite (belt-and
-- -suspenders : toute ligne '_unset' est par construction un override Locus/Bearer legacy).
--
-- ## Rollback
--
-- Le runner est forward-only. Rollback manuel documenté : `0036_override_locus_bearer_vault.down.sql`
-- (non auto-exécuté ; re-bascule 'main' → '_unset' pour les scopes Locus/Bearer — voir la
-- réserve d'irréversibilité qui y est documentée).

UPDATE note_overrides
   SET vault_id = 'main'
 WHERE vault_id = '_unset'
   AND scope_kind IN ('locus', 'bearer');

INSERT INTO _schema_migrations (version, applied_at)
  VALUES ('0036_override_locus_bearer_vault', strftime('%s', 'now') * 1000);
