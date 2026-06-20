-- Migration 006 — Bootstrap table jobs Apalis (v0.2.0 ARCH-D15)
--
-- Table principale pour le QueueStore (SqliteQueueStore).
-- Toutes les opérations (enqueue, dequeue, complete, fail, etc.) passent
-- par cette table via sqlx async.
--
-- Compatibilité : SQLite WAL mode (journal_mode=WAL, synchronous=NORMAL).
-- Toutes les colonnes NOT NULL sauf celles explicitement NULL.

-- Table des jobs queue (v0.2.0)
CREATE TABLE IF NOT EXISTS gradatum_jobs (
    -- Identifiant unique — ULID stocké en TEXT (26 chars, monotone)
    id              TEXT NOT NULL PRIMARY KEY,

    -- Payload JSON complet du JobRecord (sérialisé via serde_json)
    payload         TEXT NOT NULL,

    -- Statut courant — enum JobStatus sérialisé en TEXT
    -- Valeurs : 'Pending' | 'Running' | 'Waiting' | 'Done' | 'Failed' | 'DLQ' | 'Cancelled'
    status          TEXT NOT NULL DEFAULT 'Pending',

    -- Priorité dénormalisée pour ORDER BY priority DESC sans désérialisation
    -- Valeurs : 3=High | 2=Normal | 1=Low | 0=Deferred
    priority        INTEGER NOT NULL DEFAULT 2,

    -- Classe de job pour le filtrage et le monitoring
    -- Valeurs : 'System' | 'Agent' | 'Human' | 'Api'
    class           TEXT NOT NULL DEFAULT 'System',

    -- Timestamps (ISO-8601 UTC, format chrono::DateTime<Utc>)
    created_at      TEXT NOT NULL,
    scheduled_at    TEXT NOT NULL,    -- run_at (peut être dans le futur pour les retries)
    started_at      TEXT,             -- NULL jusqu'au premier dequeue
    completed_at    TEXT,             -- NULL jusqu'à la fin (Done/Failed/DLQ/Cancelled)

    -- Lease anti-doublon — expiration du lease actif
    -- NULL si pas de lease active (status != 'Running')
    lease_until     TEXT,

    -- Compteur de tentatives (incrémenté à chaque dequeue)
    attempt_count   INTEGER NOT NULL DEFAULT 0,

    -- Deadline optionnelle — annulation automatique si dépassée
    deadline        TEXT,

    -- Dernière erreur (tronquée à 2048 chars)
    last_error      TEXT,

    -- Jobs dont ce job attend la complétion (sérialisé JSON : array de ULID)
    -- NULL si immédiat (pas de chaînage)
    await_jobs      TEXT
);

-- Index pour le dequeue ordonné par priorité et scheduled_at
-- ORDER BY priority DESC, scheduled_at ASC
-- WHERE status = 'Pending' AND scheduled_at <= ?
CREATE INDEX IF NOT EXISTS idx_gradatum_jobs_dequeue
    ON gradatum_jobs (status, priority DESC, scheduled_at ASC);

-- Index pour la recherche par statut (monitoring, sweep)
CREATE INDEX IF NOT EXISTS idx_gradatum_jobs_status
    ON gradatum_jobs (status);

-- Index pour le sweep des leases expirées
-- WHERE status = 'Running' AND lease_until < ?
CREATE INDEX IF NOT EXISTS idx_gradatum_jobs_lease
    ON gradatum_jobs (status, lease_until);

-- Index pour la cascade (find_awaiting)
-- Cherche les jobs en Waiting qui référencent un job_id donné dans await_jobs.
-- Note : SQLite ne supporte pas les index JSON natifs — filtrage par LIKE sur await_jobs TEXT.
-- Performance acceptable pour les volumes attendus (Phase 1.1 : < 10k jobs actifs).
CREATE INDEX IF NOT EXISTS idx_gradatum_jobs_waiting
    ON gradatum_jobs (status)
    WHERE status = 'Waiting';

-- Index pour le promote_retries sweep
-- WHERE status = 'Failed' AND scheduled_at <= ?
CREATE INDEX IF NOT EXISTS idx_gradatum_jobs_retry
    ON gradatum_jobs (status, scheduled_at)
    WHERE status = 'Failed';

-- Index pour la deadline sweep
-- WHERE deadline IS NOT NULL AND deadline < ? AND status NOT IN ('Done','DLQ','Cancelled')
CREATE INDEX IF NOT EXISTS idx_gradatum_jobs_deadline
    ON gradatum_jobs (deadline)
    WHERE deadline IS NOT NULL;
