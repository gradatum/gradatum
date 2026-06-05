-- Migration V0001 — création de la table api_keys
-- gradatum-acl-auth auth.T2 (spec V2 2026-05-06)
-- DBs séparées : db/api_keys.sqlite (argon2id hash) | db/revocation.sqlite (JWT JTI)
--
-- Note : PRAGMA journal_mode = WAL est appliqué via SqliteConnectOptions avant la migration,
-- pas ici. SQLite interdit le changement de journal_mode dans une transaction implicite
-- (sqlx migrate! wraps chaque migration dans une transaction).

CREATE TABLE IF NOT EXISTS api_keys (
    id              TEXT    PRIMARY KEY,        -- ULID
    prefix          TEXT    NOT NULL UNIQUE,    -- "ak_" + 8 premiers chars (display only, non-secret)
    hash            TEXT    NOT NULL,           -- argon2id PHC string — jamais le secret en clair
    owner           TEXT    NOT NULL,           -- ex: "smoke-test", "agent-backend", "claude-code"
    scopes_json     TEXT    NOT NULL,           -- JSON array : ["admin"] ou ["vault.read", "vault.write"]
    tenant_id       TEXT    NOT NULL,           -- "main" par défaut (D3-complet, D10 multi-tenancy)
    created_at      INTEGER NOT NULL,           -- epoch secondes
    last_used_at    INTEGER,                    -- nullable, mis à jour à chaque verify réussi
    revoked_at      INTEGER,                    -- nullable, positionné sur revoke
    description     TEXT                        -- optionnel, CLI --description
);

-- Index partiel sur owner (clés actives uniquement) — accélère list() without revoked.
CREATE INDEX IF NOT EXISTS idx_api_keys_owner ON api_keys(owner) WHERE revoked_at IS NULL;

-- Index sur prefix — accélère verify() et revoke() par prefix.
CREATE INDEX IF NOT EXISTS idx_api_keys_prefix ON api_keys(prefix);
