-- Migration 0014 — colonne outcome sur event_log (F-19 M6, v0.4.4 distillation)
--
-- Discriminateur de résultat best-effort pour l'event-log : prépare la consommation
-- F-22 Distill (filtrer les events réussis des erreurs avant clustering/synthèse).
--
-- Valeurs conventionnelles (TEXT nullable) : 'Success' | 'Rejected' | 'Error'.
-- Rempli au best-effort par l'émetteur/le lecteur quand dérivable du status_code :
--   2xx → Success · 4xx → Rejected · 5xx → Error · sinon NULL.
-- Nullable : les rows historiques (pré-0014) restent NULL — aucune réécriture.
--
-- Design : ADD COLUMN nullable — additif, aucune réécriture de table (SQLite safe).
-- Forward-compat : les callers qui ne fournissent pas d'outcome obtiennent NULL.

ALTER TABLE event_log ADD COLUMN outcome TEXT;

-- Index outcome : requêtes d'attribution par résultat (F-22 filtrage qualité events).
CREATE INDEX IF NOT EXISTS idx_event_log_outcome ON event_log(outcome);

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0014_event_log_outcome', CAST(strftime('%s','now') AS INTEGER) * 1000);
