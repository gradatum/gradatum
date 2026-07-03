-- 0023_proactive_recall_sessions.sql — sessions de rappel proactif (F-46, Active Recall v0.7.1)
--
-- Deux tables complémentaires :
--   proactive_recall_sessions : trace chaque session de surface proactive émise (recall_id, tenant,
--     mode, liste d'ULIDs surfacés en JSON, timestamp de création).
--   proactive_recall_feedback  : capture l'acceptation des hits (quels ULIDs retenus par le rappel).
--
-- Sémantique :
--   - Un rang par recall_id dans chaque table.
--   - proactive_recall_feedback.recall_id : FK logique vers sessions (pas de contrainte FK SQLite).
--   - Rétention automatique via ProactiveRecallStore::purge (âge + cap).
--   - Hors FTS5 — lecture/écriture via ProactiveRecallStore uniquement.
--
-- Additif strict : ne modifie aucune table existante.
CREATE TABLE IF NOT EXISTS proactive_recall_sessions (
    recall_id     TEXT    PRIMARY KEY,   -- ULID de la session de rappel (serveur-généré)
    tenant        TEXT    NOT NULL,      -- identifiant tenant (ex: "main")
    mode          TEXT    NOT NULL,      -- mode de rappel (ex: "salience", "semantic")
    surfaced_json TEXT    NOT NULL,      -- JSON Vec<String> — liste d'ULIDs surfacés
    created_ms    INTEGER NOT NULL       -- epoch ms de création (serveur)
);

CREATE TABLE IF NOT EXISTS proactive_recall_feedback (
    recall_id     TEXT    PRIMARY KEY,   -- ULID de la session (lien logique vers sessions)
    accepted_json TEXT    NOT NULL,      -- JSON Vec<String> — ULIDs acceptés par l'utilisateur
    created_ms    INTEGER NOT NULL       -- epoch ms de l'enregistrement du feedback
);

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0023_proactive_recall_sessions', CAST(strftime('%s','now') AS INTEGER) * 1000);
