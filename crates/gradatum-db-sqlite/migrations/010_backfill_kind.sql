-- Migration 010 — Backfill colonne `kind` depuis le payload JSON (fix routing DLQ)
--
-- Contexte : la migration 007 a ajouté la colonne `kind` avec DEFAULT '' mais l'enqueue
-- ne la remplissait pas. Résultat : tous les jobs ont kind = '' → le filtrage SQL
-- `WHERE kind = ?` est inopérant, les workers embed/reindex peuvent dequeue des Curate jobs.
--
-- Cette migration backfille `kind` pour les jobs existants depuis le payload JSON.
-- Le payload utilise #[serde(tag = "type", content = "data")] :
--   { "spec": { "kind": { "type": "Curate", "data": {...} } } }
--   json_extract(payload, '$.spec.kind.type') retourne "Curate", "Embed", "ReIndex", etc.
--
-- Pour les variants unitaires (sans "data") :
--   { "spec": { "kind": "Agent" } } → json_extract retourne NULL pour '$.spec.kind.type'
--   Fallback : json_extract(payload, '$.spec.kind') retourne la chaîne "Agent".
--
-- Idempotente : WHERE kind = '' OR kind IS NULL → sans effet si déjà rempli.

UPDATE gradatum_jobs
SET kind = COALESCE(
    -- Cas 1 : variant avec data → {"type": "Curate", "data": {...}}
    json_extract(payload, '$.spec.kind.type'),
    -- Cas 2 : variant unitaire → "Agent" (chaîne scalaire)
    json_extract(payload, '$.spec.kind'),
    -- Cas 3 : défensif — ne devrait pas arriver
    ''
)
WHERE kind = '' OR kind IS NULL;
