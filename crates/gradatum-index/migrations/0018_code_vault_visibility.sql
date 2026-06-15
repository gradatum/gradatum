-- Migration 0018 : ajout colonne visibility à code_vault (v0.5.2 Phase C — feature visibility)
--
-- Persiste le mode d'ingestion choisi par l'opérateur au moment du `code ingest` :
--   - 'pub'  : seuls les items publics sont indexés (comportement par défaut historique).
--   - 'all'  : tous les items sont indexés, y compris les items privés.
--
-- DEFAULT 'pub' : les vaults créés par les versions antérieures (≤ v0.5.2 sans cette feature)
-- ou par la migration 0017 seule conservent le comportement public-only implicite.
--
-- Le mode est relu par `code update` pour ré-ingérer les fichiers changés avec le même
-- mode que l'ingest initial, garantissant la cohérence de l'index entre builds.
--
-- Rétrocompat : les notes pré-0018 n'ont pas de champ `visibility` dans extra_json["cs"] ;
-- le champ est `Option<String>` dans `CodeSymbolMeta` — la désérialisation JSON est tolérante
-- grâce à `#[serde(default)]`.

ALTER TABLE code_vault ADD COLUMN visibility TEXT NOT NULL DEFAULT 'pub';

INSERT INTO _schema_migrations (version, applied_at) VALUES ('0018_code_vault_visibility', unixepoch() * 1000);
