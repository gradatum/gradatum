-- Migration 0008 — colonnes c_kind + doc_kind sur notes (F-42 c-prime, v0.3.0 Tranche D)
--
-- Capture 2 métadonnées CoALA scoring-only dérivées déterministiquement de `section`.
-- Design spec GO 2026-06-01. Spec : F-42 c-prime, mapping v81 §17 adapté §10a.
--
-- c_kind  : catégorie cognitive CoALA (episodic/semantic/procedural/reflective)
-- doc_kind: axe temporel (Event = incident daté, Static = connaissance stable)
--
-- Design : ADD COLUMN nullable — additif, aucune réécriture de table (SQLite safe).
-- Backfill : déterministe, même mapping que les const Rust section_to_c_kind /
--            section_to_doc_kind dans gradatum-core/src/section.rs.
-- Usage scoring : DIFFÉRÉ v0.4.0 (F-17). Seuils 0.8/0.7 LIVE inchangés.
-- section (g-section) reste colonne autoritaire — c_kind/doc_kind sont dérivés.

ALTER TABLE notes ADD COLUMN c_kind TEXT;
ALTER TABLE notes ADD COLUMN doc_kind TEXT;

-- Backfill déterministe : mapping identique aux const Rust (Task 1).
-- IMPORTANT : toute modification de ce CASE doit être répercutée dans
--             section_to_c_kind() / section_to_doc_kind() (section.rs)
--             ET dans le test c_kind_matches_backfill_sql() (même fichier).
UPDATE notes
SET
    c_kind = CASE section
        WHEN 'architecture'    THEN 'semantic'
        WHEN 'decisions'       THEN 'episodic'
        WHEN 'council'         THEN 'episodic'
        WHEN 'debug'           THEN 'episodic'
        WHEN 'reasoning'       THEN 'semantic'
        WHEN 'feedback'        THEN 'reflective'
        WHEN 'lessons-learned' THEN 'semantic'
        WHEN 'retrospectives'  THEN 'reflective'
        WHEN 'experiments'     THEN 'semantic'
        WHEN 'agent-issues'    THEN 'procedural'
        WHEN 'reference'       THEN 'semantic'
        ELSE                        'semantic'
    END,
    doc_kind = CASE section
        WHEN 'debug'        THEN 'Event'
        WHEN 'agent-issues' THEN 'Event'
        ELSE                     'Static'
    END;

-- Index scoring-only : requêtes futures par catégorie cognitive (F-17 v0.4.0).
CREATE INDEX IF NOT EXISTS idx_notes_c_kind   ON notes(c_kind);
CREATE INDEX IF NOT EXISTS idx_notes_doc_kind ON notes(doc_kind);

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0008_note_cognitive_kind', CAST(strftime('%s','now') AS INTEGER) * 1000);
