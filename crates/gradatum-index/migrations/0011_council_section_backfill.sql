-- Migration 0011 — Section::Council backfill (v0.4.1 C6-a).
--
-- Contexte : avant v0.4.1, la section "council" était absente de l'enum `Section`.
-- Les notes routées "council" par le curator tombaient en `Section::Reference`
-- (fallback `section_from_str` dans apalis_handlers.rs). Ces notes ont été stockées
-- avec `section='reference'` alors que leur contenu appartient à la section "council".
--
-- Cette migration effectue deux actions :
-- 1. Backfill heuristique : mise à jour `section='council'` pour les notes avec
--    `section='reference'` dont le `body_text` contient le marqueur [COUNCIL].
--    Heuristique conservative : préfixe [COUNCIL] = signal fort du curator routing.
--    Les notes `section='council'` déjà correctes (insertions SQL directes) ne sont
--    pas touchées (WHERE section = 'reference' uniquement).
--
-- 2. Mise à jour c_kind + doc_kind pour toutes les notes `section='council'`
--    (nouvelles et backfillées) pour aligner avec le mapping CoALA v0.4.1.
--
-- IMPORTANT : toute modification du mapping doit être répercutée dans
--             section_to_c_kind() / section_to_doc_kind() (gradatum-core/src/section.rs)
--             ET dans le test c_kind_matches_backfill_sql() (même fichier).
--
-- Rollback : voir docs/ops/migration-rollback.md.

-- Étape 1 : backfill section 'reference' → 'council' pour les notes avec marqueur [COUNCIL].
UPDATE notes
SET section = 'council'
WHERE section = 'reference'
  AND body_text LIKE '%[COUNCIL]%';

-- Étape 2 : correction c_kind + doc_kind pour toutes les notes council (y compris legacy).
UPDATE notes
SET
    c_kind   = 'episodic',
    doc_kind = 'Event'
WHERE section = 'council';

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0011_council_section_backfill', CAST(strftime('%s','now') AS INTEGER) * 1000);
