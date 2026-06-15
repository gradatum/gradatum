-- 0015_session_trace.sql — session-log Tier 1 (F-19 sémantique agent-fed, council Art.15bis 2026-06-12)
--
-- Table append-only (pattern event_log) hors FTS5 — zéro pollution du recall sémantique.
-- Mono-vault (OP4 Option C) : tenant_id provient du JWT, jamais du body client.
--
-- Invariants sécu council (spec §3.1 + §10) :
--   C-SA1 : agent_id = JWT `sub` (server-side), jamais du body.
--   C-SA2 : bornes des champs enforced côté handler (intent≤200, target≤512, etc.).
--   C-SA6 : session_id = ULID server-gen si omis.
--
-- Design : INSERT-only. Aucun chemin UPDATE/DELETE par record. La purge de rétention
-- (90j, tâche tokio interval) supprime EN MASSE par âge — maintenance interne, pas d'ACL.
-- Le champ `marker` reste NULL en Phase 1a (promotion Tier 2 = Phase 1b).
CREATE TABLE IF NOT EXISTS session_trace (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id   TEXT    NOT NULL,              -- ULID, server-gen (C-SA6)
    agent_id     TEXT    NOT NULL,              -- = JWT sub (C-SA1)
    tenant_id    TEXT    NOT NULL,              -- du JWT, jamais du body
    ts_ms        INTEGER NOT NULL,              -- epoch ms de l'action
    action_type  TEXT    NOT NULL,              -- plan|edit|tool-call|decision|verdict|deploy|...
    target       TEXT,                          -- objet (≤512, allow-list C-SA2)
    intent       TEXT,                          -- court (≤200, C-SA2)
    outcome      TEXT,                          -- success|failure|partial
    marker       TEXT,                          -- NULL (Tier 2 promotion = Phase 1b)
    ref          TEXT,                          -- sha7|ulid|section/ulid (C-SA2)
    created_at   INTEGER NOT NULL               -- epoch ms insert serveur
);
CREATE INDEX IF NOT EXISTS idx_session_trace_session ON session_trace(session_id);
CREATE INDEX IF NOT EXISTS idx_session_trace_created ON session_trace(created_at);
CREATE INDEX IF NOT EXISTS idx_session_trace_agent   ON session_trace(tenant_id, agent_id);

INSERT INTO _schema_migrations (version, applied_at)
VALUES ('0015_session_trace', CAST(strftime('%s','now') AS INTEGER) * 1000);
