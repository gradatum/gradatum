-- 0041_feature_counter.sql — F-41-adjacent (allocation de numéros de feature).
--
-- Compteur persistant, per-vault, servant l'allocation ATOMIQUE des numéros de carte
-- project-map (`[[feature:F-XX]]`). Motivation : côté client (skill MCP-natif), calculer
-- `max+1` sur les cartes est impraticable (120 cartes → 120 vault_read) ET non atomique
-- (deux appels concurrents rendraient le même numéro). Le compteur déplace l'allocation
-- côté serveur, où elle est atomique et bon marché.
--
-- Design :
--   * `vault_id` PRIMARY KEY — un compteur par vault (aligné sur le modèle multi-vault :
--     l'allocation est scopée au vault du principal JWT, comme les écritures de cartes).
--   * `value` = DERNIER numéro alloué. L'allocation rend `max(value, max_dérivé_cartes) + 1`
--     puis persiste : le compteur porte la mémoire des numéros alloués mais pas encore
--     matérialisés en carte ; le dérivé (recalculé à CHAQUE appel) corrige le plancher vers
--     le haut si une carte hors-allocateur a pris de l'avance.
--   * PAS de seed en dur ici. Le maximum est DÉRIVÉ des cartes project-map à chaque allocation
--     — source de vérité fiable (rôle `[[feature:F-XX]]` du corps, jamais les tags). Un seed en
--     dur inférieur à une carte existante serait un piège ; dériver de la source est
--     strictement plus sûr et auto-cicatrisant (ligne perdue → re-dérivée au prochain appel).
--
-- Additif pur : `CREATE TABLE`, aucune réécriture, aucun ALTER sur `notes` (règle A5).
-- Table inerte tant qu'aucune allocation n'a lieu (byte-identical au deploy jusqu'au
-- premier `allocate_feature_number`).
--
-- Idempotence : garantie par le registre `_schema_migrations` du runner (forward-only) ;
-- `IF NOT EXISTS` en défense supplémentaire. Rollback manuel : `0041_feature_counter.down.sql`.

CREATE TABLE IF NOT EXISTS feature_counter (
    vault_id TEXT PRIMARY KEY,
    value    INTEGER NOT NULL
) WITHOUT ROWID;

-- Enregistrement dans le registre de migrations (guard idempotence du runner).
INSERT INTO _schema_migrations (version, applied_at)
  VALUES ('0041_feature_counter', CAST(strftime('%s','now') AS INTEGER) * 1000);
