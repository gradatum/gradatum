-- Migration 0029 : compteur d'usage PAR NOTE (F-110 Phase 1 — salience per note).
--
-- Jumeau per-note de read_usage_counters (0019, granularité endpoint). Alimenté par
-- les chemins de lecture (vault_read, vault_search, proactive_recall, lessons), accumulé
-- en mémoire (NoteUsageAccumulators dans AppState) puis flushé toutes les 60s (UPSERT).
--
-- kind : vocabulaire FERMÉ (5 valeurs, constantes note_usage_store.rs) —
--   read · search-hit · search-hit-top3 · recall-surfaced · recall-accepted.
--   search-hit-top3 s'incrémente EN PLUS de search-hit (sous-compteur rangs 1-3).
-- count / last_used_ms : cumul UPSERT (count += excluded.count, last_used_ms = MAX(...)).
--
-- STRICT : typage rigoureux (SQLite refuse toute valeur hors type déclaré).
-- Pas de FK vers notes : une note archivée (F-100) garde son historique d'usage ;
-- le GC des lignes orphelines est un non-problème Phase 1 (réévalué en F-111).
CREATE TABLE IF NOT EXISTS note_usage (
    tenant_id    TEXT NOT NULL,
    note_id      TEXT NOT NULL,
    kind         TEXT NOT NULL,
    count        INTEGER NOT NULL DEFAULT 0,
    last_used_ms INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, note_id, kind)
) STRICT;

-- Balayage temporel (rétention / requêtes as-of F-111) : filtre par tenant + dernière utilisation.
CREATE INDEX IF NOT EXISTS idx_note_usage_last ON note_usage (tenant_id, last_used_ms);

-- Enregistrement dans le registre de migrations (guard idempotence du runner).
INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0029_note_usage', CAST(strftime('%s','now') AS INTEGER) * 1000);
