-- Migration 0021 — Section::ProjectMap backfill (v0.6.0.stage1, project-map).
--
-- Contexte : la section "project-map" est la 12ᵉ section gradatum, ratifiée par
-- council Art.19 (spec §16). C'est une section NEUVE — aucune note legacy n'a
-- jamais été routée "project-map" (entrée uniquement par section_hint explicite +
-- validateur de liens, exclue du routing curator auto, spec §16 B3). Il n'y a donc
-- pas de backfill heuristique 'reference' → 'project-map' (contrairement à 0011).
--
-- Cette migration aligne c_kind + doc_kind pour toute note "project-map"
-- (présente ou future) sur le mapping CoALA des const Rust :
--   c_kind   = 'procedural' (unité de travail/process — spec §16 B1)
--   doc_kind = 'Static'     (entité mutée par RMW, pas événement immuable — B1)
--
-- IMPORTANT : toute modification de ce mapping doit être répercutée dans
--             section_to_c_kind() / section_to_doc_kind() (gradatum-core/src/section.rs)
--             ET dans le test c_kind_matches_backfill_sql() (même fichier).
--
-- Rollback : voir docs/ops/migration-rollback.md.

-- Alignement c_kind + doc_kind pour toutes les notes 'project-map' (défensif/idempotent).
UPDATE notes
SET
    c_kind   = 'procedural',
    doc_kind = 'Static'
WHERE section = 'project-map';

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0021_project_map_section_backfill', CAST(strftime('%s','now') AS INTEGER) * 1000);
