-- Migration 0038 — `note_embeddings_ann` (vec0) recomposée en PARTITION KEY natif (A4).
--
-- ## Root-cause / objectif
--
-- Le schéma 0020 déclarait `note_id TEXT PRIMARY KEY` — clé d'identité **GLOBALE** :
--   CREATE VIRTUAL TABLE note_embeddings_ann USING vec0(
--       note_id TEXT PRIMARY KEY, vault_id TEXT PARTITION KEY, ...);
-- Deux vaults indexant le **même ULID** entrent en collision sur cette PK globale :
-- l'`upsert_ann` (`INSERT OR REPLACE`) **évince** la ligne ANN de l'autre vault
-- (une seule ligne ANN par ULID, quelle que soit la partition). C'est la dernière
-- surface d'éviction cross-vault de la couche multi-vault (caveat pré-flip A4).
--
-- Cette migration retire la PK globale et fait de `note_id` une colonne ordinaire :
-- l'identité de partition vec0 devient `(vault_id, embedder_id)` (déjà PARTITION KEY),
-- le rowid vec0 (implicite) sert de clé de ligne. Le **même ULID coexiste** désormais
-- sur deux partitions `(vault_id, embedder_id)` distinctes → fin de l'éviction.
--
-- ## SQL inchangé (delete / search / GC)
--
-- Les chemins de suppression (`sqlite.rs` `delete_note_from_index` / `gc_orphan_ann`)
-- et de recherche (`sqlite_vec.rs` `search_ann_inner`) filtrent DÉJÀ
-- `WHERE note_id = ? AND vault_id = ?` (scoping composite livré C4-1e Slice D2) :
-- ils restent INCHANGÉS. Seul `upsert_ann` (sqlite_vec.rs) bascule de `INSERT OR REPLACE`
-- (PK globale) vers `DELETE WHERE vault_id=? AND note_id=?` puis `INSERT` (upsert scopé
-- partition, sans éviction) — cf. Task 6.
--
-- ## Data-safety : index DÉRIVÉ reconstructible (pas de backfill vec0→vec0 in-SQL)
--
-- `note_embeddings_ann` est un index dérivé **entièrement reconstructible** depuis
-- `note_embeddings` (source de vérité, BLOB f32 LE, porteuse de `vault_id`) via
-- `SqliteIndex::backfill_ann_index()`, appelé au boot serveur (`main.rs`, juste avant
-- le GC des orphelins). vec0 interdit `ALTER` sur une virtual table → le changement de
-- clé impose `DROP + CREATE`. On NE copie PAS les vecteurs vec0→vec0 (opération
-- vec0-spécifique, non testable hors extension) : la table est recréée VIDE et
-- reconstruite au prochain démarrage depuis la source de vérité. Aucune perte de
-- donnée durable (les vecteurs vivent dans `note_embeddings`, intacte ici).
--
-- ## Byte-identical flag OFF (vérifié pré-vol)
--
-- ANN est OFF sur LIVE : `search.ann_backend = BruteForce` → l'extension `vec0` n'est
-- jamais enregistrée (`register_sqlite_vec` skippé) → la migration 0020 est ignorée
-- (gate `vec_version()`, table ABSENTE sur LIVE, `note_embeddings` = 2247, `notes` =
-- `notes_fts` = 10185 au pré-vol 2026-07-20) → 0038, gatée de la même façon, N'EST PAS
-- appliquée au deploy. Zéro schéma touché à OFF, byte-identical trivial. La correction
-- ne prend effet qu'à la ré-activation ANN (`ann_backend = SqliteVec`), régime où vec0
-- est chargé et où `DROP TABLE` / `CREATE VIRTUAL TABLE` sont valides.
--
-- ## Rollback
--
-- Runner forward-only. Rollback manuel documenté : `0038_ann_composite_vault.down.sql`
-- (restaure le schéma 0020 : `note_id TEXT PRIMARY KEY` global).

DROP TABLE IF EXISTS note_embeddings_ann;

CREATE VIRTUAL TABLE IF NOT EXISTS note_embeddings_ann USING vec0(
    vault_id      TEXT PARTITION KEY,
    embedder_id   TEXT PARTITION KEY,
    note_id       TEXT,
    vector        FLOAT[1024] distance_metric=cosine
);

INSERT INTO _schema_migrations (version, applied_at)
  VALUES ('0038_ann_composite_vault', unixepoch() * 1000);
