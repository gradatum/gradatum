-- Migration 0020 : ANN via sqlite-vec vec0 (v0.5.3 ANN-1)
--
-- Crée la virtual table `note_embeddings_ann` (vec0) pour la recherche
-- de voisins approchés (ANN) par similarité cosine.
--
-- ## Colonnes
--
-- - `note_id TEXT`                         : ULID de la note (PK, FK logique → notes.id)
-- - `vault_id TEXT PARTITION KEY`          : isolation par vault (filtre PARTITION KEY vec0)
-- - `embedder_id TEXT PARTITION KEY`       : isolation par modèle d'embedding
-- - `vector FLOAT[1024] distance_metric=cosine` : vecteur bge-m3 (dim=1024)
--
-- ## PARTITION KEY
--
-- vec0 supporte jusqu'à 4 colonnes PARTITION KEY. Elles servent de filtre d'égalité
-- avant la recherche ANN, réduisant l'espace de recherche. L'UPDATE d'une colonne
-- PARTITION KEY n'est pas supporté dans sqlite-vec 0.1.9 (erreur
-- "UPDATE on partition key columns are not supported yet"). Conséquence : un changement
-- de vault_id/embedder_id nécessite DELETE + INSERT (géré dans sqlite_vec.rs::upsert_ann).
--
-- ## Source de vérité
--
-- `note_embeddings` (BLOB f32 LE) reste la source de vérité. `note_embeddings_ann`
-- est un index dérivé, entièrement reconstruible depuis `note_embeddings` via
-- `SqliteIndex::backfill_ann_index()`.
--
-- ## Backfill
--
-- Le backfill initial N'EST PAS effectué dans cette migration SQL car
-- `vec_f32_from_blob()` n'est disponible que si l'extension sqlite-vec est chargée
-- au runtime. Le backfill est délégué à `SqliteIndex::backfill_ann_index()` qui
-- doit être appelé explicitement après enregistrement de l'extension.
--
-- ## Disponibilité de la table
--
-- Cette migration est toujours appliquée (pas de feature gate SQL).
-- Si l'extension sqlite-vec n'est pas chargée au runtime lors d'une requête ANN,
-- SQLite retourne "no such module: vec0" — comportement attendu en mode
-- brute-force (ANN-5 : le wiring runtime est différé à une phase ultérieure).
--
-- ## Dim 1024
--
-- Correspond au modèle bge-m3 (embedder_id='bge-m3'). Les notes indexées avec
-- d'autres dimensions sont exclues du backfill (filtre embedder_id).

CREATE VIRTUAL TABLE IF NOT EXISTS note_embeddings_ann USING vec0(
    note_id       TEXT PRIMARY KEY,
    vault_id      TEXT PARTITION KEY,
    embedder_id   TEXT PARTITION KEY,
    vector        FLOAT[1024] distance_metric=cosine
);

INSERT INTO _schema_migrations (version, applied_at) VALUES ('0020_ann_sqlite_vec', unixepoch() * 1000);
