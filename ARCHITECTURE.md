# Gradatum — Architecture

> Source of truth for technical design. Updated as the project evolves.
> Initial design reviewed by an internal multi-expert panel (architect, LLM expert,
> infrastructure expert, security auditor, ops monitoring) — 2026-05-01.

---

## Vision

Gradatum is a **memory backbone** for multi-agent AI systems — not a note-taking tool for humans. The format is Markdown for human readability, but the operational source of truth is an indexed multi-signal store interrogated by agents.

**3 design pillars**:

1. **Learning** — readable end-to-end chain, no opaque external service
2. **Resilience** — works offline, degrades gracefully, no single point of failure when deployed correctly
3. **Autonomy** — OSS Apache-2.0, embedded, runs without LLM if needed (heuristic mode is first-class, not fallback)

> **Phase 1 design detail** : The `Note` pivot is structured as **4 layers** (identity immutable / canonical Note / extensions distributed / versioning + overrides). The design rationale, constraints, and trade-offs are summarised in [CHANGELOG.md](CHANGELOG.md) under the `v0.1.0` entry.

---

## 4 plans

```
┌────────────────────────────────────────────────────────────────────┐
│  CONTROL PLANE   (3 binaires séparés, scalables indépendamment)    │
├────────────────────────────────────────────────────────────────────┤
│  gradatum-server   stateless façade HTTP/MCP rmcp 0.17 SSE :19090  │
│  gradatum-worker   async queue consumer (curator + maintenance)    │
│  gradatum-admin    CLI ops (init/migrate/backup/restore/vault ops) │
└──────────────────────────────────┬─────────────────────────────────┘
                                   │
┌──────────────────────────────────┴─────────────────────────────────┐
│  DATA PLANE      (workspace 28 crates total, single-writer logique)│
├────────────────────────────────────────────────────────────────────┤
│  gradatum-core         shared primitives (errors, ids, types)      │
│  gradatum-markdown     parse/serialize MD + frontmatter + wikilinks│
│  gradatum-vault        multi-vault registry + lifecycle + swap     │
│  gradatum-storage      FS abstraction + loci paths + vault_id      │
│  gradatum-index        SQLite + FTS5 + sqlite-vec ANN + PageRank   │
│  gradatum-search       multi-mode reader (BM25/semantic/graph/RRF) │
│  gradatum-queue        SQLite-backed jobs + lease atomic           │
│  gradatum-cache        moka LRU in-process key=(vault_id, hash)    │
│  gradatum-chat         trait Chat + OpenAICompat + Heuristic       │
│  gradatum-curator      note curation: filtering, routing, tagging   │
│  gradatum-embed        remote/local embedding service + fallback    │
│  gradatum-engine       supervisor for local inference (manages llama-server subprocesses)         │
│  gradatum-acl-policy   ACL preset + config model loading           │
│  gradatum-acl-auth     glob pattern matching + bearer token verify  │
│  gradatum-auth         JWT/OIDC/API-key auth + token validation    │
└────────────────────────────────────────────────────────────────────┘
                                   │
┌──────────────────────────────────┴─────────────────────────────────┐
│  GATEWAY         (autonomous LLM proxy — NEW v0.3.0)               │
├────────────────────────────────────────────────────────────────────┤
│  gradatum-gateway  :8436  LLM proxy + QaEvent enrichment + sink    │
│    → POST /api/v1/event-log (gradatum-server)                      │
│    Routes: /v1/chat/completions (+SSE) · /v1/embeddings            │
│            /v1/rerank · /v1/models · /health · /metrics            │
└────────────────────────────────────────────────────────────────────┘
                                   │
┌──────────────────────────────────┴─────────────────────────────────┐
│  CLIENTS         (3 binaires minces)                               │
├────────────────────────────────────────────────────────────────────┤
│  gradatum-mcp-stub  adapter MCP stdio → HTTP (thin proxy)          │
│  gradatum (CLI)     end-user CLI                                   │
│  gradatum-sdk-rs    Rust SDK for direct integration                │
└────────────────────────────────────────────────────────────────────┘
                                   │
┌──────────────────────────────────┴─────────────────────────────────┐
│  EXTERNAL        (pluggable via trait LlmBackend)                  │
├────────────────────────────────────────────────────────────────────┤
│  Any OpenAI-compatible LLM:                                        │
│  Ollama, vLLM, llama.cpp, OpenRouter, Anthropic-via-proxy, etc.    │
│  Plus: Heuristic mode (no LLM required) and Noop (tests)           │
└────────────────────────────────────────────────────────────────────┘
```

---

## Queue topology (v0.2.0 — Apalis Job Infrastructure)

**Pattern agnostique (F-24)** : `GradatumQueue` façade over Apalis `Backend` trait → pluggable storage via `QueueStore` impl.

```
GRADATUM-SERVER                GRADATUM-WORKER
(HTTP :19090)                  (Apalis Monitor)
    │                                │
    │ POST /api/v1/jobs             │
    │ enqueue(job_kind, payload)    │
    │                                │
    └──→ GradatumQueue              │
         │                           │
         └──→ SqliteQueueStore       │
              │                      │
              └──→ SQLite WAL mode   │
                   jobs table        │
                   (6 columns:       │
                    id, kind,       │
                    status, payload,│
                    lease_until,    │
                    retries)        │
                                    │
                   dequeue (atomic) ←┘
                   status: Pending → Running
                   
                   process job_kind handlers:
                   - classify
                   - downgrade
                   - reindex
                   - backup
                   - purge
                   
                   on_complete:
                   - update status → Done | DLQ
                   - broadcast SSE
                   - export Prometheus
                   
                   on_error:
                   - retry (exponential backoff)
                   - max 3 retries → DLQ
                   - log error_trace
```

**Components** :
- **gradatum-queue** : `GradatumQueue` trait impl + Apalis Backend custom wiring
- **gradatum-db-sqlite** : `SqliteQueueStore` impl `QueueStore` (15 methods atomic UPDATE...RETURNING)
- **gradatum-worker** : Apalis Monitor (multi-worker FSM) + handlers dispatch (JobKind pattern match)
- **gradatum-server** : SSE endpoint + Prometheus :19091 metrics (opt-in)

**JobStatus enum (7 variants)** : Pending, Running, Waiting, Done, Failed, DLQ, Cancelled

**Migrations** :
- `006_apalis_bootstrap` : jobs table + indices + lease column
- `007_jobs_kind_indexed` : (vault_id, job_kind, status) composite index for query perf
- `008_idempotency` : idempotency_key column + unique constraint

---

## Semantic hierarchy (6 levels)

```
GRADATUM        1 instance = 1 systemd service
   ↓
VAULTS          multi-vault first-class (default "main", staging/bench-* on demand)
                each vault = 1 SQLite DB + 1 Tantivy index + 1 MD storage tree
   ↓
LOCI            logical subdivisions, isolated by bearer ACL
                path-like: "human", "main-agent", "projecta/backend", "projectb/tester"
   ↓
SECTIONS        10 canonical: decisions, architecture, debug, reasoning, feedback,
                lessons-learned, retrospectives, experiments, agent-issues, reference
   ↓
NOTES           Markdown + YAML frontmatter + ULID + checksum_md
```

**Multi-vault rationale (council 2026-05-01)** — 6 use cases justify first-class multi-vault from v1.0:
- Bench curator on isolated vault (no prod pollution)
- Migration v1 → staging → drift check → atomic swap
- Schema upgrade testing
- A/B test LLM prompts on parallel vaults
- Security sandbox (test injection / ACL bypass)
- New project onboarding in dedicated vault, swap when mature

---

## ACL hierarchy — configurable via presets

3-layer architecture:

1. **Core (this project)** — generic primitives: `Locus` (path string), `ConsumerAcl { read_patterns, write_patterns, token_hash }`, `globset` pattern matching
2. **Presets (`examples/presets/`)** — shipped templates: `flat.toml`, `hierarchical.toml`, `multi-project.toml`, `team.toml`
3. **User config (`~/.gradatum/config/bearer.toml`)** — generated at init, freely editable

The core knows nothing about "human", "main-agent", or "sub-agent" — those are user-defined patterns.

### Hierarchical preset (typical multi-agent pattern)

```
[HUMAN]                   you
   read=*  write=human,*  sees_personal_classified=true
       ↓
[MAIN AGENT]              orchestrator (any LLM-based agent)
   read=*,!personal-classified  write=main-agent,*/briefing
       ↓
   ├─ [SUB-AGENT WRITER]      coder agents, prompt-engineer
   │     read=main-agent,${P}/briefing,${P}/${A}
   │     write=${P}/${A}
   │
   ├─ [SUB-AGENT VALIDATOR]   reviewer, tester, security-auditor
   │     read=main-agent,${P}/*  (cross-read for audit)
   │     write=${P}/${A}
   │
   └─ [EXPERT]                domain specialists, monitoring
         read=main-agent,${THEME}/*
         write=${THEME}/${A}
```

---

## Source of truth — Option III hybrid

| Layer | Role |
|---|---|
| Markdown files (`vault/md/`) | **Source of truth.** Human-readable. Compatible with Obsidian/Logseq. Survives if Gradatum is down. |
| SQLite DB (`vault/data/gradatum.db`) | Index + cache. Stores `checksum_md` for drift detection. Reconstructible from MD via `gradatum-admin reindex`. |

**Drift detection**: at read time (sampled or `--strict`), compare stored `checksum_md` against current file hash. Diverging = note flagged in audit log + integrity_violation event.

---

## Storage layout

```
~/.gradatum/                       (root, NVMe local)
├── config/
│   ├── gradatum.toml              (global config)
│   ├── bearer.toml                (ACL, generated from preset)
│   └── presets/                   (user-modified templates)
├── vaults/
│   ├── main/                      (default vault)
│   │   ├── data/
│   │   │   ├── gradatum.db        (SQLite WAL: notes + jobs + embeddings)
│   │   │   └── tantivy/           (Tantivy index segments)
│   │   └── md/                    (Markdown source of truth)
│   │       ├── human/...
│   │       ├── main-agent/...
│   │       └── projecta/backend/decisions/note-01HX....md
│   ├── staging/                   (created on demand)
│   └── bench-2026-05-01/          (ephemeral)
└── (optional: previous/ from last vault swap)
```

---

## SQL schema (core tables — Phase 1, migration `0001_phase1`)

> Implémenté dans `crates/gradatum-index/migrations/0001_phase1.sql` (T09 commit `112f55a`).
> Colonne `extra_json TEXT` (non `extra_yaml`) — serde_json stable sur `toml::Value::Datetime`.

```sql
-- Suivi des migrations
_schema_migrations (version TEXT PRIMARY KEY, applied_at TEXT NOT NULL)

-- Notes (source of truth indexed)
notes (
    id          TEXT PRIMARY KEY,      -- ULIDv4
    vault_id    TEXT NOT NULL,
    locus       TEXT,
    section     TEXT NOT NULL,
    status      TEXT NOT NULL,
    content_hash TEXT NOT NULL,        -- SHA-256 JCS pour drift detection
    body_text   TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '[]',
    author_kind TEXT,
    author_id   TEXT,
    schema_version INTEGER NOT NULL DEFAULT 1,
    extra_json  TEXT NOT NULL DEFAULT '{}',  -- ExtraFields sérialisé JSON
    created_at  TEXT NOT NULL,
    updated_at  TEXT
);
CREATE INDEX idx_notes_vault_section ON notes(vault_id, section);
CREATE INDEX idx_notes_vault_status  ON notes(vault_id, status);
CREATE INDEX idx_notes_content_hash  ON notes(content_hash);

-- FTS5 full-text search (content=notes, tokenize='unicode61')
notes_fts USING fts5(body_text, tags, content=notes, tokenize='unicode61')

-- Overrides génériques : 1 payload actif par (note, scope, type)
note_overrides (
    id               TEXT PRIMARY KEY,
    note_id          TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    scope_kind       TEXT NOT NULL,   -- 'vault' | 'locus' | 'bearer'
    scope_id         TEXT NOT NULL,
    override_type    TEXT NOT NULL,
    schema_version   INTEGER NOT NULL DEFAULT 1,
    payload_toml     TEXT NOT NULL,
    file_relative_path TEXT NOT NULL, -- placeholder '_unset/...' jusqu'à T11
    created_at       TEXT NOT NULL,
    updated_at       TEXT,
    UNIQUE(note_id, scope_kind, scope_id, override_type)
);

-- Checksums fichiers (drift Phase A §5.3)
file_checksums (
    relative_path           TEXT PRIMARY KEY,
    file_kind               TEXT NOT NULL,   -- 'note' | 'override' | 'config'
    expected_size           INTEGER NOT NULL,
    expected_hash_prefix_4kb BLOB NOT NULL,  -- SHA-256 premiers 4KB (32 bytes)
    expected_hash           BLOB NOT NULL,   -- SHA-256 complet (32 bytes)
    checked_at              TEXT NOT NULL
);

-- Audit trail (AuditEvent typé)
audit_trail (
    id          TEXT PRIMARY KEY,
    note_id     TEXT REFERENCES notes(id) ON DELETE SET NULL,
    event_type  TEXT NOT NULL,
    actor_kind  TEXT,
    actor_id    TEXT,
    payload_json TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL
);
CREATE INDEX idx_audit_note_id    ON audit_trail(note_id);
CREATE INDEX idx_audit_event_type ON audit_trail(event_type);
CREATE INDEX idx_audit_created_at ON audit_trail(created_at);

-- Placeholders Phase 2+ (structure réservée)
note_index      (id TEXT PRIMARY KEY, note_id TEXT, ...)  -- index sémantique
note_embeddings (id TEXT PRIMARY KEY, note_id TEXT, ...)  -- vecteurs
note_history    (id TEXT PRIMARY KEY, note_id TEXT, ...)  -- historique versions
```

### Migration 0006 (`event_log`) — v0.3.0 Tranche B1

```sql
-- Append-only telemetry table — OUTSIDE notes/notes_fts (zero FTS5 pollution).
-- Forward-compat: processed flag consumed by Job::Distill v0.5.0.
CREATE TABLE IF NOT EXISTS event_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           INTEGER NOT NULL,              -- epoch ms (parsed from QaEvent.timestamp RFC3339)
    tenant_id    TEXT    NOT NULL,
    route        TEXT    NOT NULL,
    model_alias  TEXT    NOT NULL,
    model_used   TEXT,                          -- actual resolved model (fallback-aware) — nullable
    provider     TEXT    NOT NULL,
    feature_id   TEXT,                          -- nullable (header X-Feature-Id may be absent)
    status_code  INTEGER NOT NULL,
    latency_ms   INTEGER NOT NULL,
    tokens_input  INTEGER,                      -- nullable (streaming → None)
    tokens_output INTEGER,                      -- nullable (streaming → None)
    cost_usd     REAL,                          -- NULL in v0.3.0 (no pricing table yet)
    processed    INTEGER NOT NULL DEFAULT 0,   -- 0=pending, 1=consumed by Job::Distill (F-19 v0.5.0)
    created_at   INTEGER NOT NULL               -- epoch ms server-side insertion
);
CREATE INDEX IF NOT EXISTS idx_event_log_created   ON event_log(created_at);
CREATE INDEX IF NOT EXISTS idx_event_log_tenant    ON event_log(tenant_id);
CREATE INDEX IF NOT EXISTS idx_event_log_feature   ON event_log(feature_id);
CREATE INDEX IF NOT EXISTS idx_event_log_processed ON event_log(processed);
```

Retention: tokio task (30-day TTL / 6-hour interval / 5M-row cap). Prometheus gauge `gradatum_event_log_rows`.
Backup: exclude `event_log` (telemetry disposable, reconstructible from gateway logs).

### Migration 0007 (`event_log_agent_id`) — v0.3.0 Tranche B1 council caveat

```sql
ALTER TABLE event_log ADD COLUMN agent_id TEXT;  -- source: header X-Agent-Id (max 256 chars)
CREATE INDEX IF NOT EXISTS idx_event_log_agent ON event_log(agent_id);
```

### Migration 0008 (`note_cognitive_kind`) — v0.3.0 Tranche F-42

```sql
ALTER TABLE notes ADD COLUMN c_kind   TEXT;  -- CoALA: episodic/semantic/procedural/reflective
ALTER TABLE notes ADD COLUMN doc_kind TEXT;  -- temporal: Event or Static (Versioned conceptual, not implemented v0.3.0)

-- Deterministic backfill — identical to section_to_c_kind() / section_to_doc_kind() in section.rs
UPDATE notes SET
    c_kind = CASE section
        WHEN 'architecture' THEN 'semantic'
        WHEN 'decisions'    THEN 'episodic'
        WHEN 'debug'        THEN 'episodic'
        WHEN 'reasoning'    THEN 'semantic'
        WHEN 'feedback'     THEN 'reflective'
        WHEN 'lessons-learned' THEN 'semantic'
        WHEN 'retrospectives'  THEN 'reflective'
        WHEN 'experiments'  THEN 'semantic'
        WHEN 'agent-issues' THEN 'procedural'
        WHEN 'reference'    THEN 'semantic'
        ELSE                     'semantic'
    END,
    doc_kind = CASE section
        WHEN 'debug'        THEN 'Event'
        WHEN 'agent-issues' THEN 'Event'
        ELSE                     'Static'
    END;
CREATE INDEX IF NOT EXISTS idx_notes_c_kind   ON notes(c_kind);
CREATE INDEX IF NOT EXISTS idx_notes_doc_kind ON notes(doc_kind);
```

**Note (C12)** : `council` (11th section in §10a) is absent from the `Section` enum (10 variants). Notes with `section_hint="council"` fall to `Section::Reference` → stored as `section="reference"` → `c_kind="semantic"` (not `episodic`). **No `WHEN 'council'` branch exists in this backfill SQL** (the enum variant is missing) — pre-existing miscategorized notes are NOT corrected by this migration. Fix deferred v0.4.0 (requires adding the `Council` variant to the enum).

### Migration 0004 (`vault_downgrade`) — alpha.9

```sql
ALTER TABLE notes ADD COLUMN replaced_by TEXT REFERENCES notes(id);
CREATE INDEX idx_notes_status_downgrade ON notes(vault_id, status) WHERE status='downgraded';
```

### Migration 0005 (`add_title_column`) — alpha.10

```sql
ALTER TABLE notes ADD COLUMN title TEXT;
CREATE INDEX idx_notes_title ON notes(vault_id, title) WHERE title IS NOT NULL;
-- Backfill H1 depuis body_text existant
UPDATE notes SET title = TRIM(SUBSTR(body_text, 3, ...)) WHERE body_text LIKE '# %';
```

Nouvelles méthodes `SqliteIndex` (alpha.10) :
- `live_note_count(vault_id)` — `COUNT(*) WHERE status='live'` (Bug1)
- `total_body_size_bytes(vault_id)` — `COALESCE(SUM(LENGTH(body_text)),0)` (Bug2)
- `search_fts_scored_filtered(vault_id, query, section?, limit)` — filtre section conditionnel (B1)
- `search_fts_with_snippet(vault_id, query, section?, limit)` — snippet FTS5 natif + title (M9)
- `list_notes(vault_id, section?, limit, cursor?)` — pagination ULID lexicographique (M6)
- `upsert_note_title(note_id, title)` — mise à jour colonne title post-curate (M8)

### Méthodes `SqliteIndex` (alpha.12 — multi-facteur)

- `get_indegree(vault_id, note_id)` — count `note_links` entrants (T12 backlinks).
- `get_note_created_and_indegree(vault_id, note_id)` — `(created_at, indegree)` en un round-trip pour composite scoring.

### Méthodes `SqliteIndex` (alpha.13 — endpoints completeness)

- `find_note_by_title(vault_id, title)` — lookup titre exact filtré `status='live'` (T14 B4 `vault_read` accepte `{title}` ou `{id}`).
- `trace_by_query(vault_id, query, limit)` — FTS5 multi-match → top-N notes (T15 M4 mode FTS).
- `get_note_lineage(vault_id, note_id)` — parents (`note_links` outgoing) + children (incoming) (T15).
- `context_top_notes(vault_id, query, limit)` — agrégation top-10 notes pour budget tokens (T16 M5).

---

## Storage trait carve (v0.3.0 — Étapes 0.1 + 0.2a)

The monolithic `trait Index` was decomposed into 3 granular traits in `gradatum-core`, with backward-compatible façade:

```rust
// gradatum-core/src/document_store.rs
#[async_trait]
pub trait DocumentStore: Send + Sync {
    async fn write_note(&self, note: &Note) -> Result<(), GradatumError>;
    async fn get_note(&self, vault_id: &VaultId, note_id: &NoteId) -> Result<Option<Note>, GradatumError>;
    async fn delete_note(&self, vault_id: &VaultId, note_id: &NoteId) -> Result<bool, GradatumError>;
    // + batch read, exists, count
}

// gradatum-core/src/index_store.rs
#[async_trait]
pub trait IndexStore: Send + Sync {
    async fn search_fts(&self, ...) -> Result<Vec<SearchHitRaw>, GradatumError>;
    async fn upsert_override_raw(&self, ...) -> Result<(), GradatumError>;
    async fn get_note_created_and_indegree(&self, ...) -> Result<(String, i64), GradatumError>;
    // + checksums, neighbors, lineage, PageRank, wikilinks, …
}

// gradatum-core/src/vector_store.rs
#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn upsert_embedding(&self, ...) -> Result<(), GradatumError>;
    async fn search_semantic(&self, ...) -> Result<Vec<SearchHitRaw>, GradatumError>;
    async fn get_embedding(&self, ...) -> Result<Option<Vec<f32>>, GradatumError>;
}

// gradatum-core/src/index.rs — façade + blanket impl
pub trait Index: DocumentStore + IndexStore + VectorStore {}
impl<T: DocumentStore + IndexStore + VectorStore + ?Sized> Index for T {}
```

**AppState wiring** (Étape 0.2a): `AppState.search: Arc<dyn Index>` — vtable dispatch. `SqliteIndex` (gradatum-index) implements all 3 sub-traits via delegation to `*_inner` helpers (5 collision renames applied).

**Stability**: traits documented as `#[stability::unstable]` (comment-only, macro activation deferred v0.4.0). API frozen at Silver (v0.4.0 target).

---

## Secrets DI + JWT key persistence (v0.3.0 — Tranche C)

```rust
// gradatum-core/src/secrets.rs
pub trait SecretsProvider: Send + Sync {
    fn get_secret(&self, key: &str) -> Result<SecretBytes, SecretsError>;
}

pub struct SecretBytes(SecretBox<[u8]>);  // Drop-zeroize, Debug masked

pub struct EnvSecretsProvider;            // reads from process environment
pub struct FileSecretsProvider { path: PathBuf, /* mode check */ }
// Refuses secrets if file permissions are too open (> 0600)
```

**JWT signing key persistence** (P0 fix):
- **Before v0.3.0**: `JwtService::new_ephemeral()` — Ed25519 key generated fresh each boot → all JWTs invalidated on every server restart (bug triggered by power loss)
- **From v0.3.0**: `load_or_generate_jwt_key()` in `gradatum-server/src/jwt_key_boot.rs` — raw 32-byte Ed25519 seed loaded from `FileSecretsProvider` (path from config `jwt_private_key_path`), atomically written chmod 600 (O_CREAT mode) on first boot, dir 0700
- **Deploy impact (C13)**: first deploy replaces ephemeral key → **one-time invalidation** of all live JWTs. Consumers must re-exchange API key for new JWT. Operator gate required.

---

## LLM abstraction

```rust
#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn chat(&self, prompt: &str, opts: ChatOpts) -> Result<String>;
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn health(&self) -> HealthStatus;
    fn capabilities(&self) -> Capabilities;
}
```

3 implementations:

- **`OpenAICompat`** — covers OpenAI, Anthropic-via-proxy, OpenRouter, Ollama, vLLM, llama.cpp, and any OpenAI-compatible local gateway (95% of market)
- **`Heuristic`** — first-class autonomous mode (regex/keywords classifier, no LLM call). Classifies as `section=reference, status=pending_review` if signal weak.
- **`Noop`** — tests

Per-job configuration: `curator.model`, `maintenance.model`, `embed.model` can each point to different backends.

**Resilience** (T6 P2.0c-tris, commit `b6249c0`): `CuratorPipeline` configurable `fallback_on_error` strategy — LLM error → `Pending` (default) or `Rejected` (if `fallback=reject`). The previous `CircuitBreaker` wrapping at the curator level was removed (transparent breaker court-circuited the explicit fallback). Dette P2.1: re-introduire `CircuitBreaker` au niveau `gradatum-server/state.rs` (couche service) pour couper les appels répétés vers backend mort.

**Embed fallback**: if remote embedding service unavailable, fallback to local CPU `fastembed` (bge-small-en 768d). Semantic search degrades but doesn't fail.

---

## Search pipeline (alpha.13)

```
Query
  ├─ FTS5 BM25 (notes_fts)              top-K  ← B1 section filter (alpha.10)
  ├─ Semantic ANN (cosine f32 LE BLOB)  top-K  ← gradatum-index search_semantic (alpha.11)
  └─ (PageRank lookup)                  graph signal aggregated as `pagerank_factor`
       ↓
   RRF fusion (k=60, stable sort)               ← gradatum-search rrf_fuse() (alpha.11)
       ↓
   Composite scoring (alpha.12)
       composite = rrf × (1 + α·recency_factor) × (1 + β·pagerank_factor)
       α=0.2 (recency), β=0.1 (PageRank), λ=0.01 (recency exp-decay)
       ↓
   Top-20 → Reranker trait (alpha.12)
       NoopReranker (default) | JinaOnnxReranker (feature `onnx-reranker`)
       ↓
   Top-N hits — DTO `SearchHit { id, score, title?, section, snippet }`
```

**Graceful degradation** : embed service down → WARN + skip semantic branch + BM25 fallback transparent.
**Reranker** câblé `Noop` par défaut. Activation Jina via env `RERANKER_ONNX_PATH` + feature flag (Phase 2.x.5 backlog amend).
**Multi-signal weights** (α, β, λ, RRF k) configurable per vault dans `gradatum.toml` (Phase 2.x.5 — actuellement codé en dur conservateur).

---

## Concurrency model

- **Single writer** = `gradatum-worker` (consumes jobs queue)
- **Multiple readers** = `gradatum-server` (HTTP/MCP queries) via SQLite WAL mode
- **Server never writes directly** — always enqueues
- **Lease pattern** for jobs: `UPDATE jobs SET status='leased', leased_at=NOW(), leased_by=? WHERE id=? AND status='pending' RETURNING *`
- **Lease timeout** 5 minutes max + watchdog requeues expired leases
- **Cache** is per-(vault_id, query_hash), invalidated on write to that vault

---

## Deployment scope

> **Critical**: Gradatum core is single-instance. HA/redundancy patterns are **external** and documented in [`docs/DEPLOYMENT-HA.md`](docs/DEPLOYMENT-HA.md), not built-in.

Recommended patterns:

- **Database HA**: [Litestream](https://litestream.io/) → SFTP/S3/NFS continuous replication (RPO ~seconds)
- **Markdown HA**: rsync cron 5min OR Syncthing continuous OR lsyncd
- **Backup**: `gradatum-admin backup` produces atomic tar.gz (DB + MD + config snapshot)

Failover is manual (or scripted via systemd watchdog if user wants).

---

## Platform support

| Tier | Platform | CI compile | CI runtime | Guarantees |
|---|---|---|---|---|
| Primary | Linux x86_64 | Forgejo Actions | All tests | Full feature set, official support |
| Secondary | Windows x86_64 | Cross-compile mingw-w64 (`continue-on-error`) | Manual pre-release | Compile clean, core tests, NFS check warn |
| Future | macOS | — | — | Code remains portable by design (`cfg(unix)` preferred) |

See [`docs/RFC/RFC-0002-cross-platform-support.md`](docs/RFC/RFC-0002-cross-platform-support.md) for full tiered support model and portability rules R1–R13. Sprint X-1 (2026-05-04) established this policy.

---

## Auth flow (Phase 2.0c-bis alpha.5)

**3 auth paths** (progressive complexity):

| Path | Flow | Use case | Status |
|---|---|---|---|
| **Path 1** | OIDC verifier (JWT external issuer) | Enterprise SSO + Keycloak/OAuth2 | Phase 3 (β) |
| **Path 2** | API key (`POST /auth/exchange`) | Consumer-grade (agents, SDK, MCP) | **Phase 2.0c-bis (α.5)** ✅ |
| **Path 3** | CLI `gradatum-admin token issue` | Bootstrap + operator recovery | **Phase 2.0c-bis (α.5)** ✅ |

**Path 2 implementation** (alpha.5, spec `2026-05-06-p2.0c-bis-auth-path-2-design.md`):

- **API key storage**: `gradatum-acl-auth::ApiKeyStore` trait + `SqliteApiKeyStore` impl, keys hashed argon2id (OWASP 2023 compliant: m=19456 KiB, t=2, p=1)
- **Endpoint**: `POST /auth/exchange {api_key: "..."}` (hors JWT middleware, standalone route)
- **Response**: `ExchangeResponse { token (JWT EdDSA), ttl_secs, scopes, tenant_id, kid }`
- **JWT**: EdDSA Ed25519 (24h service TTL / 1h human TTL per scope, spec R-A1)
- **Revocation**: `SqliteRevocationStore` checked runtime on all exchange calls
- **TrustContext**: Mandatory `tenant_id` propagation via middleware (D3-complet, breaking change Claims struct Phase 2.0c-bis)
- **Grants**: flat scopes alpha.5 (`["admin"]`), granular scopes deferred Phase 2.1

**ACL integration**: Bearer token hashed + stored in preset `bearer.toml`, glob patterns match loci read/write (unchanged from Phase 1 design, auth path 2 just adds exchange endpoint).

---

## Deployment (Phase B alpha.5)

**Systemd pattern** (production-ready):

- **Service units**: `gradatum-server.service` (MemoryMax=512M) + `gradatum-worker.service` (MemoryMax=1G, MemorySwapMax=0)
- **User/group**: `gradatum` UID/GID 985 (static allocation, verified non-collision on the deployment host)
- **State directory**: `/var/lib/gradatum/{config,db,md,vault}/` (StateDirectory + LogDirectory systemd directives)
- **Startup order**: `gradatum-admin init --root /var/lib/gradatum` once (idempotent, includes `--preset flat|hierarchical|...`), then `systemctl start gradatum-server` → wait health OK → `systemctl start gradatum-worker`
- **Init script**: `scripts/install-gradatum-services.sh` (10 étapes end-to-end: check user, build, install binaries, init state, create services, enable + start, smoke acceptance)

**Storage layout**:

```
/var/lib/gradatum/
├── config/
│   ├── server.toml (host:port, db paths, llm config)
│   ├── bearer.toml (ACL preset, generated or user-provided)
│   └── *.pem (JWT keys if generated)
├── db/
│   ├── index.sqlite (WAL: notes, jobs, audit)
│   ├── queue.sqlite (worker queue lease atomic updates)
│   └── revocation.sqlite (auth path 2 revocation store)
└── md/ (optional: Markdown source tree — drift detection)
```

**Health checks**:

- `/health` endpoint returns `{"status":"ok"|"degraded", ...}` (sync with RFC-0003 §8)
- `smoke-alpha-5.sh`: 9 étapes acceptance (auth path 2 criteria, write→curator→read, audit JSONL generation)
- Smoke result Phase B (2026-05-07): 4 PASS / 5 WARN / 0 FAIL (auth runtime + deploy patterns validated, docs/non-blocking warnings)

**Post-tag fixes** (Phase B drifts):

- **Drift #5** (`7be4cd2`): queue_path convention unified `<root>/db/queue.sqlite` (align db/ folder layout)
- **Drift #6** (`a951b52`): `gradatum-admin init --preset` hiérarchique/flat embedded via `include_str!` (reproducible install)
- **README.md** (`packaging/systemd/`): clarified init command signature + UID/GID alignment + phase B lessons

---

## Audit trail (P2.0b — caveat C4)

Two distinct audit systems coexist:

| Layer | Type | Location |
|---|---|---|
| Phase 1 (internal) | `AuditEvent` + `AuditEventType` (rich enum, ULID correlation) | `gradatum-core::audit` — SQLite `audit_trail` + JSONL SIEM |
| P2.0b HTTP service | `HttpAuditEvent` flat (bearer JWT actor, JCS content_hash) | `gradatum-core::audit::http` + `gradatum-server::audit_jsonl` |

`JsonlFileSink` (production): daily rotation on UTC date, files `audit.YYYY-MM-DD.jsonl` mode `0640`, immediate flush per event. Trait `AuditSink` is pluggable (noop for tests).

`content_hash_jcs()`: `sha256(JCS RFC 8785 canonical)` → `"sha256:<hex64>"`. Produces identical hashes for JSON objects with different key ordering.

---

## Endpoints (Phase 2.x.4 alpha.13)

Server `:19090` expose les endpoints HTTP (REST) suivants — parité MCP via `gradatum-mcp-stub` :

| Endpoint | Méthode | Description | Phase |
|---|---|---|---|
| `/health` | GET | Statut service + dépendances | 2.0a |
| `/auth/exchange` | POST | API key → JWT EdDSA Ed25519 (TTL 24h) | 2.0c-bis (alpha.5) |
| `/api/v1/vault_write` | POST | Ingestion async note (curator + embed chained) | 2.0a |
| `/api/v1/vault_read` | GET | Lecture par `{id}` ou `{title}` (lookup B4 alpha.13) | 2.x.4 (alpha.13) |
| `/api/v1/vault_search` | POST | Search hybride RRF + composite + reranker | 2.x.3 (alpha.12) |
| `/api/v1/vault_status` | GET | Stats vault (note_count + total_size_bytes) | 2.x.1 (alpha.10) |
| `/api/v1/vault_list` | GET | Liste paginée notes (curseur ULID) | 2.x.1 (alpha.10) |
| `/api/v1/vault_trace` | POST | Lineage multi-mode (`{id}` / `{title}` / `{query}` FTS) | 2.x.4 (alpha.13) |
| `/api/v1/vault_context` | POST | Agrégation context budget tokens (chars/3.0) | 2.x.4 (alpha.13) |
| `/api/v1/vault_downgrade` | POST | Soft downgrade (status='downgraded' + replaced_by) | 2.1.2 (alpha.9) |
| `/api/v1/vault_classify` | POST | Heuristic + LLM curator routing | 2.0a |
| `/api/v1/jobs/:id` | GET | Statut job worker (lease + progress) | 2.0a |
| `/api/v1/event-log` | POST | Gateway QaEvent ingestion (auth Path-2 JWT + ACL `write_patterns`) | **v0.3.0** |
| `/metrics` | GET | Prometheus exposition | 2.0b |

---

## Workspace deps (alpha.13)

- **MSRV** : `1.85` (bump de 1.75 alpha.12-bumps.1 PR-6).
- **HTTP stack** : `axum 0.8.9` + `tower-http 0.6.10` + `reqwest 0.13.3` (PR-3).
- **MCP** : `rmcp 1.x` + `schemars 1.x` (PR-2).
- **Crypto** : `sha2 0.11` + `governor 0.10` + `nix 0.31` + `jsonwebtoken 10` (PR-5). Chaîne `ed25519-dalek 2.x` + `rand_core 0.6` figée — bumps reportés PR-7.
- **TOML** : `toml 1.1.2` + `toml_edit 0.25.11` (PR-6).
- **Serde YAML** : `serde_yml 0.0.12` (drop-in fork post-deprecation `serde_yaml`).
- **Reranker** : `ort 2.0.0-rc.9` + `tokenizers 0.21` (feature `onnx-reranker` opt-in).
- **DB** : `rusqlite 0.32` (bump 0.39 reporté Phase 2.3 — conflit `links="sqlite3"` vs `sqlx 0.8.6`).

---

## Workers — pipeline post-curate (alpha.13)

`gradatum-worker` consomme les jobs `kind={vault_write, embed_note, curate_note, ...}` via lease atomique. Pipeline ingestion :

```
vault_write → curate_note → [B5 wikilinks] + [embed_note chained]
                  ↓
            note_links (graph)
                  ↓
            note_embeddings (semantic)
```

**B5 wikilinks** (T13 alpha.13) : `process_wikilinks_b5` parse les `[[wikilinks]]` du body curaté + INSERT OR IGNORE dans `note_links`. **Non-fatal** : un échec wikilink n'invalide pas le job curate.

---

## Deployment scripts

| Script | Rôle |
|---|---|
| `scripts/install-gradatum-services.sh` | Install systemd `gradatum-server` + `gradatum-worker` (Linux x86_64). Renamed from the previous install script at commit `50fa520`. |
| `scripts/install-gradatum-stub-mcp.sh` | Install `/usr/bin/gradatum-mcp-stub` + clé API `/etc/gradatum/claude-code.api-key` + génère `scripts/mcp.json.sample` (artefact runtime, gitignored). |
| `scripts/smoke-alpha-13.sh` | E2E acceptance Phase 2.x.4 (auth JWT exchange + `/health` + write→curate→read+trace+context). |
| `scripts/smoke-alpha-{10,11,12}-*.sh` | Smokes phases antérieures (régression historique). |

**Auth flow déploiement** : api-key fichier `/etc/gradatum/claude-code.api-key` (chmod 600 root:gradatum) → POST `/auth/exchange` → JWT TTL 24h. Le stub MCP auto-refresh quand TTL restant < 30 %.

---

## Backlog Phase 2.x.5 (alpha.14 — polish)

- **C1** LIKE escape sur `find_note_by_title` (sécurité injection wildcard `%` `_`).
- **C2** `vault_trace IN(...)` batch query au lieu de N×N round-trips lineage.
- **C7-bis** Backfill colonne `title` 551/552 notes production (M8 alpha.10 introduit la colonne, backfill complet à finir).
- **C9** Wikilinks N×N parallèle `tokio::join_all` (actuellement séquentiel dans `process_wikilinks_b5`).
- **Reranker câblage `main.rs`** : activation `JinaOnnxReranker` conditionnel sur env `RERANKER_ONNX_PATH` + feature flag (alpha.12 backlog amend).
- **PR-4** `rusqlite 0.32 → 0.39` (Phase 2.3, attente `sqlx 0.9` stable pour résoudre conflit `links="sqlite3"`).
- **Multi-signal weights configurables** : α, β, λ, RRF k via `gradatum.toml` per-vault.

---

## References

- Phase 2.x design: see CHANGELOG.md for implemented features per version
- Predecessor design: the predecessor system (archived, v1.6.x) — internal, not public
- Inspirations: [mycelium-io/mycelium](https://github.com/mycelium-io/mycelium) (Rooms pattern, OpenAPI, install one-liner)
- Standards: [Apache-2.0](LICENSE) license, [CLA](CLA.md), [Contributor guide](CONTRIBUTING.md)

---

*This document is updated by Gradatum maintainers after each architectural change. Last update: 2026-06-02 — v0.3.0 code-complete (storage trait carve + Arc<dyn Index> + event-log B1 + F-42 c_kind + Secrets DI + JWT key persistence + gradatum-gateway; workspace 28 crates).*
