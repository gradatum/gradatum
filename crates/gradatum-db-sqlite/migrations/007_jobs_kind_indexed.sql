-- Migration 007 — Colonne `kind` dénormalisée + index (F-16 Phase 3)
--
-- Ajoute la colonne `kind` dans gradatum_jobs pour permettre le filtrage SQL natif
-- via `SELECT ... WHERE kind = ?` sans désérialisation du payload BLOB.
--
-- Le fix E-10 (Phase 1.1) filtrait `kind` en mémoire via désérialisation de tous
-- les payloads sélectionnés. Cette migration résout l'inefficacité avec un index.
--
-- Backfill : le tag Apalis encode le variant comme le premier mot de la repr Debug.
-- ex: `Job::Curate(...)` → payload JSON `"spec"."kind"` sérialisé → extraction via json_extract.
-- Si la valeur n'est pas extractible → chaîne vide (défensif, jamais NULL).
--
-- Compatibilité : SQLite ALTER TABLE (ajout colonne avec DEFAULT) — O(1) sur SQLite.
-- SQLite 3.35+ : RETURNING + ALTER TABLE ADD COLUMN supportés.

ALTER TABLE gradatum_jobs ADD COLUMN kind TEXT NOT NULL DEFAULT '';

-- Backfill depuis le payload JSON (best-effort, defensif)
-- Le payload sérialisé encode Job via serde_json. La structure est :
--   { "spec": { "kind": { "<VariantName>": ... } } }
-- Ou pour les variants unitaires : { "spec": { "kind": "<VariantName>" } }
-- On extrait la première clé de l'objet kind, ou la valeur scalaire.
UPDATE gradatum_jobs
SET kind = COALESCE(
    -- Cas 1 : kind est un objet { "VariantName": ... } → extraire la clé
    (SELECT key FROM json_each(json_extract(payload, '$.spec.kind')) LIMIT 1),
    -- Cas 2 : kind est une chaîne scalaire (variant unitaire)
    json_extract(payload, '$.spec.kind'),
    ''
);

-- Index pour le filtrage F-16 par kind
CREATE INDEX IF NOT EXISTS idx_jobs_kind ON gradatum_jobs (kind);

-- Index composite status+kind pour les requêtes F-16 filtrées
-- ex: GET /api/v1/jobs?status=dead&kind=embed
CREATE INDEX IF NOT EXISTS idx_jobs_status_kind ON gradatum_jobs (status, kind);
