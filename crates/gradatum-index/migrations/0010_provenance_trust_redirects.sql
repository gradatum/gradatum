-- Migration 0010 — F-47 provenance/trust + F-39 redirect_table (lot v0.4.0 Écriture durable).
--
-- provenance (TEXT) : source de confiance de la note (ex: "agent-log", "human-decision").
--   Valeur initiale : "agent-log" (défaut conservateur — sur-confiance dangereuse sur legacy).
-- trust (REAL) : score de confiance [0,1] dérivé de provenance. Colonne index.db UNIQUEMENT.
--   Invariant : ne jamais persister trust comme float dans Frontmatter JCS (casse ContentHash).
--
-- redirect_table : table de lookup titre→ULID après renommage (F-39 Stable Wikilinks).
--   Permet à resolve_redirect() de retrouver un ULID par l'ancien slug de titre.
--
-- SQLite : ADD COLUMN si NOT EXISTS n'existe pas — idempotence via _schema_migrations.
-- Rollback : voir docs/ops/migration-rollback.md.

ALTER TABLE notes ADD COLUMN provenance TEXT;
ALTER TABLE notes ADD COLUMN trust REAL;

-- Backfill conservateur : toutes les notes legacy reçoivent provenance="agent-log" / trust=0.5.
-- Rationale : confiance neutre préférable à une sur-confiance (spec §2.4, rejet "human-decision").
UPDATE notes
SET
    provenance = 'agent-log',
    trust      = 0.5
WHERE provenance IS NULL;

-- Index scoring : consommation par le décay F-17 (v0.4.1). Créés maintenant pour ne pas bloquer.
CREATE INDEX IF NOT EXISTS idx_notes_trust       ON notes(trust);
CREATE INDEX IF NOT EXISTS idx_notes_provenance  ON notes(provenance);

-- Table de redirection titre→ULID (F-39 Stable Wikilinks).
-- Peuplée par gradatum-admin vault rename. Consultée par la couche lecture en fallback.
CREATE TABLE IF NOT EXISTS redirect_table (
    title_slug TEXT NOT NULL PRIMARY KEY,   -- slug normalisé (lowercase, espaces→tirets)
    ulid       TEXT NOT NULL,               -- ULID de la note cible (après renommage)
    renamed_at INTEGER NOT NULL             -- timestamp unix ms du renommage
);
CREATE INDEX IF NOT EXISTS idx_redirect_ulid ON redirect_table(ulid);

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0010_provenance_trust_redirects', CAST(strftime('%s','now') AS INTEGER) * 1000);
