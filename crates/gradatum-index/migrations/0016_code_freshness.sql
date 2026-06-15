-- Migration 0016 : table code_freshness pour l'index-only code-ingest (v0.5.2 Phase A)
--
-- Invariant de fraîcheur §4 : chaque source_path est indexé avec son content_hash_source
-- (hash du fichier source git). Cette table sert de référence pour l'idempotence (skip si
-- hash inchangé) et pour la propagation des suppressions (source_path absent de git ls-files).
--
-- Colonne note_ids : JSON array de strings (ULIDs) — les notes dérivées de ce source_path.
-- Stocké en JSON pour éviter une table de jointure (les accès sont toujours batch par source_path).
--
-- Clé primaire composite (vault_id, source_path) : un vault peut indexer plusieurs repos,
-- et un repo peut être indexé dans plusieurs vaults (cas d'usage futur).

CREATE TABLE IF NOT EXISTS code_freshness (
    vault_id            TEXT NOT NULL,
    source_path         TEXT NOT NULL,
    content_hash_source TEXT NOT NULL,   -- hex sha256 du fichier source
    ingested_sha        TEXT NOT NULL,   -- HEAD git sha au moment de l'ingest
    note_ids            TEXT NOT NULL,   -- JSON array de ULID strings
    PRIMARY KEY (vault_id, source_path)
);

CREATE INDEX IF NOT EXISTS idx_code_freshness_vault ON code_freshness(vault_id);

INSERT INTO _schema_migrations (version, applied_at) VALUES ('0016_code_freshness', unixepoch() * 1000);
