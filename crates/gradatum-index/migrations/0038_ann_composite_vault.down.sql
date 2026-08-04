-- Rollback manuel de 0038 (A4) — **NON auto-exécuté** par le runner (forward-only).
--
-- Restaure le schéma 0020 de `note_embeddings_ann` : `note_id TEXT PRIMARY KEY` GLOBAL.
-- À appliquer MANUELLEMENT (`sqlite3 <db> < 0038_ann_composite_vault.down.sql`) si un
-- rollback LIVE est nécessaire — par exemple avant le redéploiement d'un binaire pré-A4.
--
-- ## Data-safety
--
-- `note_embeddings_ann` est un index dérivé reconstructible depuis `note_embeddings`
-- (source de vérité). Le rollback recrée la table VIDE au schéma 0020 ; elle est
-- reconstruite au prochain démarrage via `backfill_ann_index()`. Aucune perte durable.
--
-- ## Avertissement multi-vault
--
-- Le schéma 0020 réintroduit la PK GLOBALE `note_id` : deux vaults partageant un même
-- ULID ne peuvent plus coexister dans cette table (retour de l'éviction cross-vault).
-- En régime mono-vault `'main'` (flag OFF) c'est sans effet ; ne PAS appliquer ce
-- rollback si des vecteurs ANN multi-vault existent.
--
-- ## Requiert l'extension vec0
--
-- `DROP TABLE` / `CREATE VIRTUAL TABLE ... USING vec0` exigent que l'extension sqlite-vec
-- soit chargée (identique à 0020). Sur une DB où ANN est inactif (table absente), ce
-- rollback est un no-op sur `DROP TABLE IF EXISTS` mais le `CREATE VIRTUAL TABLE`
-- échouerait sans l'extension — à n'appliquer que sur un binaire ANN-actif.
--
-- La ligne de registre `_schema_migrations` est retirée pour que le runner ré-applique
-- 0038 au prochain démarrage d'un binaire post-A4.

DROP TABLE IF EXISTS note_embeddings_ann;

CREATE VIRTUAL TABLE IF NOT EXISTS note_embeddings_ann USING vec0(
    note_id       TEXT PRIMARY KEY,
    vault_id      TEXT PARTITION KEY,
    embedder_id   TEXT PARTITION KEY,
    vector        FLOAT[1024] distance_metric=cosine
);

DELETE FROM _schema_migrations WHERE version = '0038_ann_composite_vault';
