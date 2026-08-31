-- Migration 012 — Suppression de la table legacy `jobs_v2` (F-177)
--
-- Contexte :
--   `jobs_v2` (file sqlx historique, lue par le module `queue` retiré en 2.1.0)
--   est hors de tout cycle de vie du vault : ses charges utiles conservent le
--   contenu COMPLET de notes supprimées (payload BLOB) sans qu'aucune
--   suppression ni purge du vault ne l'atteigne. 2 804 lignes y stationnent,
--   figées depuis le 2026-05-29 — de la RÉMANENCE, pas de l'historique.
--
-- Action :
--   DROP TABLE. La suppression de la table emporte ses index
--   (idx_jobs_v2_pending, idx_jobs_v2_lease) — aucun DROP INDEX séparé.
--
-- Idempotente :
--   `DROP TABLE IF EXISTS` tolère une instance fraîche où la table est déjà
--   absente. La migration 009 la crée conditionnellement, mais rien ne la
--   régénère après 2.1.0 : SCHEMA_V1 (gradatum-queue) ne la crée plus.

DROP TABLE IF EXISTS jobs_v2;
