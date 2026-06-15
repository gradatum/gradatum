-- Migration 0012 — F-44 Semantic Forget : colonnes forgotten/orphaned (v0.4.3).
--
-- forgotten (INTEGER NOT NULL DEFAULT 0) : flag booléen (0/1). Une note avec
--   forgotten=1 voit son score de recherche dégradé par un decay exponentiel
--   (half-life 1 jour) — elle n'est PAS supprimée de l'index.
--
-- forgotten_at (INTEGER) : timestamp epoch ms du marquage. NULL si forgotten=0.
--   Utilisé par le calcul decay : elapsed_days = (now_ms - forgotten_at) / 86_400_000.0
--   Valeur 0.0 elapsed → 0.5^0 = 1.0 (pas de decay immédiat le jour même).
--
-- forgotten_by (TEXT) : identifiant de l'acteur ayant posé le marquage (agent_id,
--   nom utilisateur…). Optionnel — NULL si non fourni. Tracé dans le frontmatter
--   pour auditabilité (F-44, C2). Cette colonne n'est pas utilisée par le scoring.
--
-- orphaned (INTEGER NOT NULL DEFAULT 0) : réservé pour la cascade F-22 (v0.4.4).
--   Créé ici pour éviter une migration future. NON alimenté en v0.4.3.
--
-- Index partiel forgotten=1 : seul un sous-ensemble des notes sera forgotten.
--   Un index partiel réduit l'empreinte et accélère les requêtes de listing
--   et de decay (WHERE forgotten=1).
--
-- SQLite : ALTER TABLE ADD COLUMN ne supporte pas IF NOT EXISTS.
--   L'idempotence est garantie par le runner via _schema_migrations.
-- Rollback : voir docs/ops/migration-rollback.md (DROP COLUMN requiert SQLite ≥ 3.35).

ALTER TABLE notes ADD COLUMN forgotten    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE notes ADD COLUMN forgotten_at INTEGER;
ALTER TABLE notes ADD COLUMN forgotten_by TEXT;
ALTER TABLE notes ADD COLUMN orphaned     INTEGER NOT NULL DEFAULT 0;

-- Index partiel : n'indexe que les notes effectivement forgotten.
-- WHERE forgotten = 1 couvre : listing vault forgotten + decay scoring (lecture) + unforgot.
CREATE INDEX IF NOT EXISTS idx_notes_forgotten
    ON notes(forgotten)
    WHERE forgotten = 1;

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0012_forgotten_columns', CAST(strftime('%s','now') AS INTEGER) * 1000);
