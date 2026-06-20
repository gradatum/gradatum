-- Migration 0013 — F-55 TemporalIndex (v0.4.3, index-only, pas de surface API).
--
-- Crée la table `temporal_index` qui matérialise l'ancre temporelle de chaque
-- note : date occurrence, source de l'ancre, type documentaire.
--
-- ## Colonnes
--
-- note_id (TEXT PK) : ULID de la note (référence logique — PAS de FK, caveat C7 :
--   PRAGMA foreign_keys non garanti → suppression explicite dans delete_note_from_index).
-- vault_id (TEXT NOT NULL) : tenant de la note (peuple les requêtes par vault).
-- anchor_ms (INTEGER NOT NULL) : epoch UTC en millisecondes — valeur canonique pour
--   tout tri / fenêtrage temporel. Source prioritaire :
--     occurred_at > event-date > valid_from (ExtraFields YAML) > created (fallback).
-- anchor_src (TEXT NOT NULL) : identifiant de la source utilisée pour anchor_ms.
--   Valeurs : 'occurred_at' | 'event-date' | 'valid_from' | 'created'.
-- doc_kind (TEXT NOT NULL) : axe temporel CoALA (Event | Static | Versioned).
--   Dérivé déterministiquement de notes.doc_kind (migration 0008).
--   Backfill : COALESCE(doc_kind, 'Static') — notes pré-0008 avec NULL → 'Static'.
-- valid_until_ms (INTEGER) : epoch UTC ms — borne supérieure optionnelle (réservé
--   pour la fenêtrage avancé v0.5.0, NULL pour toutes les notes à la création).
--
-- ## Indexes
--
-- idx_temporal_anchor : tri / range scan par ancre (requêtes vault_timeline v0.5.0).
-- idx_temporal_vault_anchor : filtre vault + tri ancre (requêtes par tenant).
--
-- ## Backfill
--
-- INSERT OR IGNORE FROM notes : couvre 100% des notes existantes avec anchor_src='created'.
-- Le backfill est idempotent (INSERT OR IGNORE sur PK note_id).
-- Les curates ultérieurs mettent à jour anchor_ms/anchor_src si un champ ExtraFields
-- de priorité supérieure est présent (handle_curate → write_temporal_entry INSERT OR REPLACE).
--
-- ## Rollback
--
-- DROP TABLE temporal_index;  (aucune donnée primaire ici — table dérivée)
-- Requiert SQLite ≥ 3.35 pour DROP TABLE IF EXISTS dans une migration (standard OK).
--
-- ## Cohérence doc_kind
--
-- Notes pré-0008 (doc_kind NULL) → backfill avec 'Static'.
-- doc_kind n'est jamais NULL dans temporal_index : COALESCE garantit le fallback.
-- Valeurs possibles : 'Event', 'Static', 'Versioned'.
-- Migration 0008 ne définit que 'Event'/'Static' — 'Versioned' est réservé (v0.5.0).

CREATE TABLE IF NOT EXISTS temporal_index (
    note_id        TEXT NOT NULL PRIMARY KEY,
    vault_id       TEXT NOT NULL,
    anchor_ms      INTEGER NOT NULL,
    anchor_src     TEXT NOT NULL,   -- 'occurred_at'|'event-date'|'valid_from'|'created'
    doc_kind       TEXT NOT NULL,   -- 'Static'|'Event'|'Versioned'
    valid_until_ms INTEGER          -- NULL — réservé fenêtrage v0.5.0
);

CREATE INDEX IF NOT EXISTS idx_temporal_anchor
    ON temporal_index(anchor_ms);

CREATE INDEX IF NOT EXISTS idx_temporal_vault_anchor
    ON temporal_index(vault_id, anchor_ms);

-- Backfill : toutes les notes existantes avec anchor_src='created'.
-- COALESCE(n.doc_kind, 'Static') : les notes pré-migration 0008 (doc_kind NULL)
-- reçoivent 'Static' par défaut — correction progressive via curate.
-- Exclut les sentinelles (__sentinel__*) : non temporelles, hors périmètre index.
-- INSERT OR IGNORE : idempotent si la migration est rejouée sur un sous-ensemble
-- (impossible via le runner normal, mais garantit robustesse des tests de migration).
INSERT OR IGNORE INTO temporal_index (note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms)
SELECT
    n.id,
    n.vault_id,
    n.created,
    'created',
    COALESCE(n.doc_kind, 'Static'),
    NULL
FROM notes AS n
WHERE n.id NOT LIKE '__sentinel__%';

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0013_temporal_index', CAST(strftime('%s','now') AS INTEGER) * 1000);
