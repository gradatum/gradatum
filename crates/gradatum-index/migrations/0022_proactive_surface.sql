-- 0022_proactive_surface.sql — surface proactive pré-calculée (F-46, Active Recall v0.7.1)
--
-- Stocke la dernière surface proactive par tenant_id (latest-par-clé).
-- Pattern UPSERT : un seul rang par tenant, écrasé à chaque refresh.
-- `surface_json` = sérialisation JSON de `Vec<ProactiveHit>` (gradatum-dto).
--
-- Hors FTS5 — lecture/écriture via ProactiveSurfaceStore uniquement.
-- Pas de rétention automatique (la surface est toujours valide tant que le refresh tourne).
CREATE TABLE IF NOT EXISTS proactive_surface (
    tenant_id    TEXT    PRIMARY KEY,               -- clé : un rang par tenant
    surface_json TEXT    NOT NULL,                  -- JSON Vec<ProactiveHit> (serde_json)
    updated_ms   INTEGER NOT NULL                   -- epoch ms du dernier upsert
);

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0022_proactive_surface', CAST(strftime('%s','now') AS INTEGER) * 1000);
