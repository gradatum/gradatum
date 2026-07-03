-- Migration 0024 — Section::Identity backfill (v0.7.3 F-34 DEPORT).
--
-- Contexte : la section "identity" est la 13ᵉ section gradatum (soul/persona agent).
-- C'est une section NEUVE (ACL write-restrictive, bypass curator LLM, A1/A4 F-34).
-- Aucune note legacy n'a jamais été routée "identity" hors write-path contrôlé.
--
-- Cette migration aligne c_kind + doc_kind pour toute note "identity"
-- (présente ou future) sur le mapping CoALA des const Rust :
--   c_kind   = 'procedural' (gouvernance/comportement agent — F-34)
--   doc_kind = 'Static'     (âme stable, mutée par RMW uniquement — A4)
--
-- IMPORTANT : toute modification de ce mapping doit être répercutée dans
--             section_to_c_kind() / section_to_doc_kind() (gradatum-core/src/section.rs)
--             ET dans le test c_kind_matches_backfill_sql() (même fichier).
--
-- Rollback : voir docs/ops/migration-rollback.md.

-- 0024_identity_section_backfill — v0.7.3 F-34 : section canonique `identity` (13e).
-- Backfill c_kind/doc_kind pour les notes éventuelles déjà classées "identity"
-- (cohérent avec section_to_c_kind=Procedural / section_to_doc_kind=Static en Rust).
UPDATE notes SET c_kind = 'procedural' WHERE section = 'identity' AND (c_kind IS NULL OR c_kind = '');
UPDATE notes SET doc_kind = 'Static'    WHERE section = 'identity' AND (doc_kind IS NULL OR doc_kind = '');

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0024_identity_section_backfill', CAST(strftime('%s','now') AS INTEGER) * 1000);
