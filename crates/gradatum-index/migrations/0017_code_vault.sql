-- Migration 0017 : table code_vault — métadonnées vault-level du code-ingest (v0.5.2 Phase C)
--
-- Le serveur gradatum (process long-running) est isolé des repos git ingérés. Pour
-- la drift-detection §4.3 (hash du fichier source courant comparé au hash stocké
-- AVANT de servir une entrée code_scope), le handler doit localiser les fichiers
-- sur disque. Cette table mappe vault_id → chemin absolu du repo, peuplé au moment
-- du `code ingest`/`code update` (qui reçoivent ce chemin en argument CLI).
--
-- repo_abs_path : chemin absolu du repo git racine. Source unique pour :
--   1. code_scope drift-detection (hash {repo_abs_path}/{source_path}).
--   2. (futur) toute opération vault-level nécessitant l'accès au repo.
--
-- Si une entrée est absente (vault jamais ingéré, ou ingéré par une version antérieure
-- à Phase C), la drift-detection est SKIP : les entrées sont servies sans flag stale
-- mais accuracy>coverage est préservé car l'absence est explicite (pas un faux Fresh).

CREATE TABLE IF NOT EXISTS code_vault (
    vault_id      TEXT PRIMARY KEY,
    repo_abs_path TEXT NOT NULL
);

INSERT INTO _schema_migrations (version, applied_at) VALUES ('0017_code_vault', unixepoch() * 1000);
