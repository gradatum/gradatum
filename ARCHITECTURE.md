# Gradatum — Architecture

> Source of truth for technical design. Updated as the project evolves.
> Initial design reviewed by an internal multi-expert panel (architect, LLM expert,
> infrastructure expert, security auditor, ops monitoring) — 2026-05-01.

---


## New in v0.7.6

> Drop-in upgrade from v0.6.4. All new endpoint fields are optional; omitting them preserves
> prior behavior exactly. See [CHANGELOG.md](CHANGELOG.md) `[0.7.6]` for the full list of changes.

### Context assembly pipeline (`vault_context`)

Module `gradatum-server/src/context/` — full context-assembly pipeline replacing the prior FTS dump.

```
vault_context (request + params)
    │
    ▼ 1. Retrieval — RRF(BM25 + semantic embedding, k=60) · top-N candidates (configurable)
    │
    ▼ 2. Composite scoring — recency × PageRank × trust
    │       (reuses gradatum-search/src/scoring.rs)
    │
    ▼ 3. Budget-aware selection — descending score sort, lazy body fetch
    │       Pluggable TokenEstimator (calibrated heuristic by default)
    │
    ▼ 4. Structured Markdown output
    │       ### <title> · <section> · <date> · score=<X.XX>
    │       <body>
    │       --- separators · [[ULID]] source references
    │
    ▼ 5. Skill injection (opt-in) — section skills/, index-only, no embedding cost
    │
    ▼ vault_context response
         assembled_text, included[], budget_used, diagnostics
```

- **`ContextConfig`** TOML block (`[context]`): `budget_tokens_default`, `top_n_candidates`,
  `max_skills`, `skill_budget_fraction`, `embed_timeout_secs`.
- `mode=Raw` → byte-for-byte parity with the previous FTS-dump behavior (backward-compatibility fallback).
- Cross-section leak guard on the semantic path.

---

### Context efficiency — reference mode and session window

Extension of `vault_context` — reference stubs and session continuity.

```
vault_context (reference_mode=true, session_id)
    │
    ▼ Dual budget
    │   budget_inline → inline note     [full body]
    │   budget_stub   → stub note       [{ ulid, title, section, snippet }]
    │   beyond        → drop            [dereferenceable via vault_read]
    │
    ▼ Session tracking (session_trace, sent marker)
    │   note already sent inline → stub (never re-inlined)
    │   mode=compact → fold: all prior-sent → stubs, top-K fresh → inline
    │
    ▼ Canonical stubs (cache-stable)
        fixed field order · score truncated to 2 decimals · no volatile timestamp
        ULID tiebreaker (deterministic sort within the same score tier)
```

- `cache_breakpoint_hint`: boolean hint for the consumer to insert a prompt-cache boundary.
- Strict parity with context assembly when `reference_mode=false` (default).

---

### Proactive recall

Module `gradatum-server/src/proactive_recall/` — server-initiated recall with pull surface.

```
gradatum-server/main.rs
    tokio::interval 900 s (configurable, floor 60 s, MissedTickBehavior::Skip)
        │ enqueue ProactiveRefresh job
        ▼
gradatum-worker — JobKind::ProactiveRefresh
    salience query derived from titles+tags of K most recent notes (K=20, configurable)
        │ RRF cross-section retrieval (lessons-learned / reasoning / decisions)
        │ composite scoring → top-N (N=8, configurable) · stored in proactive_surface
        ▼
proactive_surface (latest per tenant, overwrites previous)
        │
        ▼ pull
POST /api/v1/proactive_recall
    mode proactive (no context field) → reads pre-computed surface (cheap path)
    mode contextual (with context field) → on-demand RRF
        │
        ▼ feedback loop
POST /api/v1/proactive_recall/feedback
    accepted_ulids ⊆ surfaced_ulids, idempotent
```

- `vault_lessons_recall` enriched: optional `rank` (`relevance` | `recency-boosted`) and
  `semantic` (`bool`) parameters; degrades gracefully to BM25 if embedding unavailable.
- Prometheus metrics: `gradatum_vault_proactive_recall_surfaced_total`, `_accepted_total`,
  `_duration_seconds`; `gradatum_proactive_refresh_total`, `_duration_seconds`.

---

### Agent identity via MCP (`identity` section — v0.7.3)

13th canonical section. Migrations 0024 (section creation) and 0025 (title backfill from H1).

```
vault_write (section_hint=identity)
    │ Write ACL: agent_id must match JWT sub
    │ validate_soul(): checks INVARIANTS / GATES / NARRATIVE sections + extends field
    │ doc_kind forced to Static
    │
    ▼ gradatum-index (identity section)

MCP initialize →
    vault_search(section=identity, caller=privileged)
        │ FAIL-CLOSED filter for non-privileged callers
        ▼
    MCP instructions override (identity note body)
        injected into the MCP initialize response

write_check (warn-only)
    vault_write_impl → check_category_section(category, section)
        drift detected → WARN + metric gradatum_write_check_total{rule}
        no write blocked
```

- `title_lookup`: deterministic column-first resolution (parity between wikilink and path lookup).
- Worker guard: reclassification of an `identity` note is a no-op.

---

### Temporal search and decay (`temporal_index` — v0.7.4)

Enrichment of `temporal_index` (introduced v0.4.3, migration 0013).

```
vault_write (occurred_at: Option<String ISO8601>)
    │ validate occurred_at → parse_temporal_str_as_ms (single source of truth)
    │ propagated through CurateSpec → worker → temporal_index.anchor_ms
    │
    ▼ temporal_index
        note_id, anchor_ms, anchor_src, doc_kind

vault_search (from_ms, to_ms filters)
    │ FTS path:      LEFT JOIN temporal_index WHERE anchor_ms BETWEEN from_ms AND to_ms
    │ Semantic path: same filter applied before RRF fusion
    │ anchor_ms included in every SearchHit
    │
    ▼ recency_factor (composite scoring signal)
        exponential decay on canonical anchor_ms (fallback: created_at)
        applied at the RRF layer only (never BM25)
        semantic-only hits enriched with anchor_ms before composite scoring

vault_context
    recency_factor uses anchor_ms (parity with vault_search)
```

**Invariant**: on `vault_write` RMW of a note that already has `anchor_ms`, the existing
anchor is preserved; an anchor is overwritten only when the note body genuinely changes.

---

### Scheduled task health observability (v0.7.5)

Observability for the 8 recurring `tokio::interval` tasks in `gradatum-server`.

```
gradatum-server/main.rs — 8 recurring tasks instrumented:
    telemetry-flush · event_log-purge · session_trace-purge
    read_usage-purge · review_promote · proactive-refresh · metric-sample · (+ 1)
        │
        │ wrap: Instant (duration) + Result capture
        ▼
record_task_run(task_name, outcome, duration_ms, error)
    │ UPSERT scheduled_task_health (run_count++)
    │ on error → INSERT scheduled_task_error + lazy 7-day purge
    │ DB error → WARN (task continues)
    ▼

gradatum-index (migration 0026)
    scheduled_task_health — one row per task
        task_name PK, last_run_ms, last_outcome, last_duration_ms,
        last_error, run_count, updated_at
    scheduled_task_error — append-only (errors only)
        id PK, task_name, occurred_ms, error_msg
        INDEX (task_name, occurred_ms)

GET /api/v1/system/scheduled (JWT auth)
    { tasks: [ { name, last_run_ms, last_outcome, last_duration_ms,
                 last_error, run_count, errors_24h, interval_secs } ] }
    errors_24h = COUNT(occurred_ms > now − 86 400 000)
    last_error sanitized (infrastructure-leak protection)

gradatum-studio/src/pages/SystemPage.tsx
    Per-task badge: ok | error | overdue (now − last_run > interval × margin)
    errors_24h highlighted in red when > 0

gradatum-studio/src/components/DashboardSchedulerWidget
    Compact summary card on DashboardPage → link to SystemPage
```

- Boot seeding: all task rows seeded with `last_run_ms = null` at startup; System page
  shows all tasks immediately, before the first tick fires.

---

### Curated metrics timeseries (v0.7.5)

Periodic curated collection from the Prometheus registry → `metric_sample` persistence → REST endpoints.

```
gradatum-server/src/curated_metrics.rs
    collect_curated_samples(&AppMetrics) → Vec<(String, f64)>
        │ encode(&registry) → String    (OpenMetrics text format)
        │ parse line-by-line
        │ static allowlist CURATED_SERIES (~60 series, 4 groups)
        │   counter/gauge  → direct value
        │   histogram      → _sum + _count as two separate series
        │   http.*         → aggregated (anti-cardinality)
        │   curator/llm    → instrumented:false (zero value, visible in catalog)
        ▼
Vec<(series: String, value: f64)>

gradatum-server/main.rs
    tokio::interval(60 s) — TASK_METRIC_SAMPLE, MissedTickBehavior::Skip
        │ collect_curated_samples
        │ insert_metric_samples(ts_ms, &samples)   batch INSERT OR IGNORE
        │ purge_metric_samples(now − 14 days)      lazy purge
        ▼ errors logged at warn; never panic

gradatum-index (migration 0027)
    metric_sample (series TEXT, ts_ms INTEGER, value REAL, PK (series, ts_ms)) WITHOUT ROWID
    INDEX idx_metric_sample_ts ON metric_sample(ts_ms)

gradatum-core — 4 IndexStore methods (no-op defaults for mocks):
    insert_metric_samples(ts_ms, samples)
    query_metric_timeseries(series, from_ms, to_ms, bucket_ms) → Vec<MetricSamplePoint>
    purge_metric_samples(cutoff_ms)
    list_distinct_metric_series()
    MetricSamplePoint { series: String, ts_ms: i64, value: f64 }

GET /api/v1/system/metrics/catalog   (same ACL scope as /dashboard)
    source = CURATED_SERIES constant (no database query)
    { series: [{ key, group, kind, unit, instrumented }] }

GET /api/v1/system/metrics/timeseries   (same ACL scope as /dashboard)
    params: series (CSV, allowlist, MAX_SERIES=32, deduplicated)
            from_ms / to_ms (inclusive; 400 if from >= to)
            max_points (default 500, cap 2000)
    compute_bucket_ms(span_ms, max_points):
        span > max_points raw points → bucket AVG SQL GROUP BY (ts_ms / bucket_ms)
        span ≤ max_points            → bucket_secs=60 (raw points)
        overflow guard on i64::MAX
    { from_ms, to_ms, bucket_secs, series: [{ key, points: [{ ts_ms, value }] }] }
```

- Retention: 14 days (lazy purge on each tick).
- Scope: server registry (`:19090`) only; worker (`:19091`) is out of scope.
- `/metrics` Prometheus endpoint unchanged (loopback-only).

**Studio metrics charts** (`gradatum-studio`, React + TypeScript + `uplot@^1.6.32`):

```
useMetricsCatalog
    GET /api/v1/system/metrics/catalog
    → CatalogEntry[] { key, group, kind, unit, instrumented }

useMetricsTimeseries(keys, fromMs, toMs, refreshMs)
    GET /api/v1/system/metrics/timeseries?series=<csv>&from_ms&to_ms
    re-fetches on param change · setInterval(refreshMs) auto-refresh
    cleanup setInterval + AbortController on unmount

metricsTransform.ts (pure functions, independently testable)
    deriveRatePerMin:    (v[i]−v[i−1]) / ((ts[i]−ts[i−1])/60000) ; Δ<0 → 0 ; first point omitted
    deriveHistogramAvg:  Δsum/Δcount at aligned timestamps; Δcount=0 → point omitted
    groupCatalog:        catalog → ChartSpec[] by family + group

<MetricChart> (imperative uPlot wrapper)
    useEffect mount   → uplot.create(options)
    useEffect cleanup → uplot.destroy()
    useEffect data    → uplot.setData(data)   // timestamps in epoch seconds, gaps as null
    ResizeObserver    → uplot.setSize(...)
    instrumented:false → grayed out + "not instrumented" badge

SystemPage — Metrics section
    Range selector: 1h | 24h (default) | 7d | 14d   → from_ms computed client-side
    4 collapsible groups: usage / context / server / write
    Auto-refresh 60 s (toggle, default on)
    max_points = 500 (server-side downsampling)
```

Deploy: `bun run build` → `sudo rsync dist/ /usr/share/gradatum/ui` (served by `ServeDir` at `/ui/*`).

---

## v0.5.0 → v0.6.4 subsystems

> Shipped between v0.4.6 and v0.6.4. See [CHANGELOG.md](CHANGELOG.md) for the full change log.

### Code-map subsystem — `code-<project>` logical vault

A derived code index, **distinct from the main memory vault**. Zero LLM cost — static ingestion only (tree-sitter). The logical vault is identified by the `code-<project-name>` naming convention.

```
GIT SOURCES
    │
    ▼
gradatum-ingest (crate)
    feature = "code-rust"
    tree-sitter Rust parser
    → DerivedSymbol { path, name, kind, span, sha256, visibility }
    │
    ▼
gradatum-admin code ingest    (initial ingest, idempotent)
gradatum-admin code update    (O(diff) git-driven incremental)
    │
    ▼
SQLite — dedicated tables (migrations 0016/0017/0018)
    code_freshness  : (path, sha256) per file — drift detection
    code_vault      : vault-level metadata (repo path, last commit)
    code_vault.visibility : pub|all per symbol (migration 0018)
    │
    ▼
IndexStore::code_scope (gradatum-index)
    check_freshness → Freshness { Fresh | Stale }
    drift flag checked before any read (freshness invariant)
    │
    ▼
POST /api/v1/code_scope     (HTTP, auth, gradatum-server)
MCP tool code_scope         (thin proxy, schemars auto-derive)
```

**Freshness invariant (7 mandatory criteria)** :
- Freshness key = `(path, sha256_hash)` — timestamp alone is insufficient
- Drift checked **before** any `code_scope` read (check_freshness)
- Index can be regenerated from sources (`gradatum-admin code ingest`)
- Accuracy over coverage (pub-only by default, --visibility all opt-in)
- Golden tests: `rebuild == incremental`
- Anti-traversal applied unconditionally (symlink safeguards included)
- `NoteId::derived_from` : deterministic SHA-256 without ordinal (idempotence)

**`include_body` / symbol-level body** :
- `include_body: bool` (default false) — symbol body not transmitted by default
- `body_budget_tokens` — cap on returned body size
- Anti-traversal applied unconditionally even when `include_body = false`
- Additive contract: fully backward-compatible with requests that omit `include_body`

**Migrations** :
| Migration | Table | Role |
|---|---|---|
| 0016 `code_freshness` | `code_freshness` | Per-file hash + batch helpers |
| 0017 `code_vault` | `code_vault` | Vault-level metadata (repo path, commit) |
| 0018 `code_vault_visibility` | `code_vault.visibility` | Visibility column per symbol (backward-compatible NULL = pub) |

**Vault isolation** : derived notes live in a `code-<project>` logical vault (by convention), distinct from the main memory vault. The cross-tenant `vault_id` clamp in the middleware applies equally to these derived vaults.

---

### Additional subsystems (post-v0.4.6)

- **vault_timeline** : `POST /api/v1/vault_timeline` + MCP tool + `IndexStore::timeline` + `TimelineFilter/Row/Cursor` types. Excludes `Section::PROTECTED_FORGET` (security constraint — zero confirmed leak in production). Temporal validity sections (`valid_until` frontmatter, `as_of_ms`, `include_expired`).
- **session-log Tier 1** : `session_trace` table (migration 0015, 90-day retention) + `POST /api/v1/session-log/trace` (append-only, PII-safe, `agent_id` = JWT sub).
- **vault_write in-place** : `note_id` + `expected_sha256` → in-place update; anti-fail-open guard (malformed SHA → 400 before 409).
- **Cross-tenant mitigation** : defense-in-depth across 6 layers (gate /auth/exchange + central middleware + JWT-derived handlers + worker + api_key + audit). Validated end-to-end in production.

---

## v0.4.4 → v0.4.6 — 2026-06-11

- **gradatum-studio** (new crate, `publish = false`) : React+TS+Vite admin UI (5 surfaces / 6 routes), served by gradatum-server via `tower-http` ServeDir on `/ui/*` (SPA fallback, CSP + security headers — `gradatum-server/src/studio.rs`). Auth: API key → JWT (**localStorage** client-side, key `gradatum_studio_jwt_persist`, persisted across reloads; the API key itself is never persisted). Studio requests a short-TTL JWT (scope `human`, 1 h). Bundle deployed to `/usr/share/gradatum/ui` (configurable `[studio] ui_dir`).
- **Worker type-erased** (v0.4.5) : all handlers consume `Arc<dyn Index>`; 8 inherent methods promoted to `IndexStore` trait (neutral no-op defaults). New crate `index-parity-tests` (24 backend-agnostic tests, CI matrix `index-backends`).
- **Distillation pipeline** (v0.4.4) : `Job::Distill(DistillSource)` semantic clustering → synthesis (pluggable `DistillSynthesizer`, deterministic template MVP) → PendingReview notes; trust decay active in composite score at RRF layer; `TRUST_SCORES["distilled"]=0.60`. No live enqueue path in this version.
- **Event-log semantics** : `agent_id`/`feature_id` emitted by engines, `outcome` column (migration 0014), `fetch_pending`/`mark_processed` internal readers.
- **Lessons recall** : `GET /api/v1/lessons/recall` (BM25-only, 12 controlled classes) + MCP tool `vault_lessons_recall` + harness hook (lesson-recall.sh, PostToolUse/UserPromptSubmit).
- **vault_search contract** : `include_scores` opt-in (`ScoreBreakdown` incl. bm25/sem ranks), `status` filter + `status` in hits; section filter fixed on the semantic path (degrades BM25-only on batch failure).
- **Curator routing** : `CurateOutcome::Pending` → `PendingReview` (SSOT `outcome_to_status`); `GET /api/v1/review` queue + `GET /api/v1/dashboard` (auth) + `POST /api/v1/notes/{id}/move` (index-level, locus preserved on re-upsert via content_hash discriminant; physical `.md` relocation deferred).

## Vision

Gradatum is a **memory backbone** for multi-agent AI systems — not a note-taking tool for humans. The format is Markdown for human readability, but the operational source of truth is an indexed multi-signal store interrogated by agents.

**3 design pillars**:

1. **Learning** — readable end-to-end chain, no opaque external service
2. **Resilience** — works offline, degrades gracefully, no single point of failure when deployed correctly
3. **Autonomy** — OSS Apache-2.0, embedded, runs without LLM if needed (heuristic mode is first-class, not fallback)

> **Initial design detail** : The `Note` pivot is structured as **4 layers** (identity immutable / canonical Note / extensions distributed / versioning + overrides). The design rationale, constraints, and trade-offs are summarised in [CHANGELOG.md](CHANGELOG.md) under the `v0.1.0` entry.

---

## 4 plans

```
┌────────────────────────────────────────────────────────────────────┐
│  CONTROL PLANE   (3 separate binaries, independently scalable)     │
├────────────────────────────────────────────────────────────────────┤
│  gradatum-server   stateless facade HTTP/MCP rmcp 0.17 SSE :19090  │
│  gradatum-worker   async queue consumer (curator + maintenance)    │
│  gradatum-admin    CLI ops (init/migrate/backup/restore/vault ops) │
└──────────────────────────────────┬─────────────────────────────────┘
                                   │
┌──────────────────────────────────┴─────────────────────────────────┐
│  DATA PLANE      (workspace 31 crates total, 27 published)         │
├────────────────────────────────────────────────────────────────────┤
│  gradatum-core         shared primitives (errors, ids, types)      │
│  gradatum-dto          wire-contract DTOs (single source of truth) │
│  gradatum-markdown     parse/serialize MD + frontmatter + wikilinks│
│  gradatum-vault        multi-vault registry + lifecycle + swap     │
│  gradatum-storage      FS abstraction + loci paths + vault_id      │
│  gradatum-index        SQLite + FTS5 + brute-force cosine + PageRank│
│  gradatum-db-sqlite    SqliteQueueStore — Apalis job queue impl    │
│  gradatum-search       multi-mode reader (BM25/semantic/graph/RRF) │
│  gradatum-ingest       code index (tree-sitter, zero LLM)         │
│  gradatum-queue        job queue facade (GradatumQueue trait)      │
│  gradatum-warden       network guard L0 (IP filter, rate-limit)    │
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

**Agnostic pattern** : `GradatumQueue` facade over Apalis `Backend` trait → pluggable storage via `QueueStore` impl.

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
                each vault = 1 SQLite DB + 1 FTS5 index + 1 MD storage tree
   ↓
LOCI            logical subdivisions, isolated by bearer ACL
                path-like: "human", "main-agent", "projecta/backend", "projectb/tester"
   ↓
SECTIONS        13 canonical: decisions, architecture, debug, reasoning, feedback,
                lessons-learned, retrospectives, experiments, agent-issues, reference, council,
                project-map, identity
   ↓
NOTES           Markdown + YAML frontmatter + ULID + checksum_md
```

**Multi-vault rationale** — 6 use cases justify first-class multi-vault from v1.0:
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
2. **Presets (`crates/gradatum-admin/presets/`)** — shipped templates embedded in the `gradatum-admin` binary: `flat.toml`, `hierarchical.toml`
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
│   │   │   └── fts5_segments/           (FTS5 index segments)
│   │   └── md/                    (Markdown source of truth)
│   │       ├── human/...
│   │       ├── main-agent/...
│   │       └── projecta/backend/decisions/note-01HX....md
│   ├── staging/                   (created on demand)
│   └── bench-2026-05-01/          (ephemeral)
└── (optional: previous/ from last vault swap)
```

---

## SQL schema (core tables)

> Implemented in `crates/gradatum-index/migrations/0001_phase1.sql`.
> Column `extra_json TEXT` (not `extra_yaml`) — serde_json stability with `toml::Value::Datetime`.

```sql
-- Migration tracking
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
    extra_json  TEXT NOT NULL DEFAULT '{}',  -- ExtraFields serialized as JSON
    created_at  TEXT NOT NULL,
    updated_at  TEXT
);
CREATE INDEX idx_notes_vault_section ON notes(vault_id, section);
CREATE INDEX idx_notes_vault_status  ON notes(vault_id, status);
CREATE INDEX idx_notes_content_hash  ON notes(content_hash);

-- FTS5 full-text search (content=notes, tokenize='unicode61')
notes_fts USING fts5(body_text, tags, content=notes, tokenize='unicode61')

-- Generic overrides: 1 active payload per (note, scope, type)
note_overrides (
    id               TEXT PRIMARY KEY,
    note_id          TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    scope_kind       TEXT NOT NULL,   -- 'vault' | 'locus' | 'bearer'
    scope_id         TEXT NOT NULL,
    override_type    TEXT NOT NULL,
    schema_version   INTEGER NOT NULL DEFAULT 1,
    payload_toml     TEXT NOT NULL,
    file_relative_path TEXT NOT NULL, -- reserved for future use
    created_at       TEXT NOT NULL,
    updated_at       TEXT,
    UNIQUE(note_id, scope_kind, scope_id, override_type)
);

-- File checksums (drift detection)
file_checksums (
    relative_path           TEXT PRIMARY KEY,
    file_kind               TEXT NOT NULL,   -- 'note' | 'override' | 'config'
    expected_size           INTEGER NOT NULL,
    expected_hash_prefix_4kb BLOB NOT NULL,  -- SHA-256 first 4KB (32 bytes)
    expected_hash           BLOB NOT NULL,   -- SHA-256 full (32 bytes)
    checked_at              TEXT NOT NULL
);

-- Audit trail (typed AuditEvent)
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

-- Placeholders for future enhancements
note_index      (id TEXT PRIMARY KEY, note_id TEXT, ...)  -- semantic index
note_embeddings (id TEXT PRIMARY KEY, note_id TEXT, ...)  -- vectors
note_history    (id TEXT PRIMARY KEY, note_id TEXT, ...)  -- version history
```

### Migration 0006 (`event_log`) — v0.3.0

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
    processed    INTEGER NOT NULL DEFAULT 0,   -- 0=pending, 1=consumed by Job::Distill (v0.5.0)
    created_at   INTEGER NOT NULL               -- epoch ms server-side insertion
);
CREATE INDEX IF NOT EXISTS idx_event_log_created   ON event_log(created_at);
CREATE INDEX IF NOT EXISTS idx_event_log_tenant    ON event_log(tenant_id);
CREATE INDEX IF NOT EXISTS idx_event_log_feature   ON event_log(feature_id);
CREATE INDEX IF NOT EXISTS idx_event_log_processed ON event_log(processed);
```

Retention: tokio task (30-day TTL / 6-hour interval / 5M-row cap). Prometheus gauge `gradatum_event_log_rows`.
Backup: exclude `event_log` (telemetry disposable, reconstructible from gateway logs).

### Migration 0007 (`event_log_agent_id`) — v0.3.0

```sql
ALTER TABLE event_log ADD COLUMN agent_id TEXT;  -- source: header X-Agent-Id (max 256 chars)
CREATE INDEX IF NOT EXISTS idx_event_log_agent ON event_log(agent_id);
```

### Migration 0008 (`note_cognitive_kind`) — v0.3.0

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

**Note on section mapping** : the `Section` enum has 13 variants (was 11 before the identity/project-map sections were added). Notes with `section_hint="council"` map to `Section::Council` → stored as `section="council"` → `c_kind="episodic"` (decision-based). Prior versions (v0.3.x) lacked this variant; notes fell back to `Section::Reference` in those older versions.

### Migration 0004 (`vault_downgrade`)

```sql
ALTER TABLE notes ADD COLUMN replaced_by TEXT REFERENCES notes(id);
CREATE INDEX idx_notes_status_downgrade ON notes(vault_id, status) WHERE status='downgraded';
```

### Migration 0005 (`add_title_column`)

```sql
ALTER TABLE notes ADD COLUMN title TEXT;
CREATE INDEX idx_notes_title ON notes(vault_id, title) WHERE title IS NOT NULL;
-- Backfill H1 from existing body_text
UPDATE notes SET title = TRIM(SUBSTR(body_text, 3, ...)) WHERE body_text LIKE '# %';
```

New `SqliteIndex` methods:
- `live_note_count(vault_id)` — `COUNT(*) WHERE status='live'` (Bug1)
- `total_body_size_bytes(vault_id)` — `COALESCE(SUM(LENGTH(body_text)),0)` (Bug2)
- `search_fts_scored_filtered(vault_id, query, section?, limit)` — conditional section filter
- `search_fts_with_snippet(vault_id, query, section?, limit)` — native FTS5 snippet + title
- `list_notes(vault_id, section?, limit, cursor?)` — ULID lexicographic pagination
- `upsert_note_title(note_id, title)` — updates title column after curation

### Additional `SqliteIndex` methods (multi-factor scoring)

- `get_indegree(vault_id, note_id)` — incoming `note_links` count (backlinks).
- `get_note_created_and_indegree(vault_id, note_id)` — `(created_at, indegree)` in a single round-trip for composite scoring.

### Endpoint-support `SqliteIndex` methods

- `find_note_by_title(vault_id, title)` — exact title lookup filtered by `status='live'` (`vault_read` accepts `{title}` or `{id}`).
- `trace_by_query(vault_id, query, limit)` — FTS5 multi-match → top-N notes.
- `get_note_lineage(vault_id, note_id)` — parents (`note_links` outgoing) + children (incoming).
- `context_top_notes(vault_id, query, limit)` — top-10 note aggregation for token budget.

---

## Storage trait carve (v0.3.0)

The monolithic `trait Index` was decomposed into 3 granular traits in `gradatum-core`, with backward-compatible facade:

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

// gradatum-core/src/index.rs — facade + blanket impl
pub trait Index: DocumentStore + IndexStore + VectorStore {}
impl<T: DocumentStore + IndexStore + VectorStore + ?Sized> Index for T {}
```

**AppState wiring** : `AppState.search: Arc<dyn Index>` — vtable dispatch. `SqliteIndex` (gradatum-index) implements all 3 sub-traits via delegation to `*_inner` helpers (5 collision renames applied).

**Stability**: traits documented as `#[stability::unstable]` (comment-only, macro activation deferred v0.4.0). API frozen at Silver (v0.4.0 target).

---

## Secrets DI + JWT key persistence (v0.3.0)

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

**JWT signing key persistence** :
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

**Resilience** : `CuratorPipeline` configurable `fallback_on_error` strategy — LLM error → `Pending` (default) or `Rejected` (if `fallback=reject`). The previous `CircuitBreaker` wrapping at the curator level was removed (transparent breaker short-circuited the explicit fallback). Known limitation: a `CircuitBreaker` at the `gradatum-server/state.rs` service layer (to cut repeated calls to a dead backend) has not yet been re-introduced.

**Embed fallback**: if remote embedding service unavailable, fallback to local CPU `fastembed` (bge-small-en 768d). Semantic search degrades but doesn't fail.

---

## Search pipeline

```
Query
  ├─ FTS5 BM25 (notes_fts)              top-K  (section filter support)
  ├─ Semantic cosine (f32 LE BLOB)      top-K  (brute-force ANN)
  └─ (PageRank lookup)                  graph signal aggregated as `pagerank_factor`
       ↓
   RRF fusion (k=60, stable sort)
       ↓
   Composite scoring
       composite = rrf × (1 + α·recency_factor) × (1 + β·pagerank_factor)
       α=0.2 (recency), β=0.1 (PageRank), λ=0.01 (recency exp-decay)
       ↓
   Top-20 → Optional reranker (feature `onnx-reranker`)
       NoopReranker (default) | OnnxCrossEncoderReranker
       ↓
   Top-N hits — DTO `SearchHit { id, score, title?, section, snippet }`
```

**Graceful degradation** : embed service unavailable → skip semantic branch, use BM25 only.
**Reranker** : disabled by default; enable via feature flag `onnx-reranker` and configure model path.
**Weights** : composite scores tuned per vault configuration.

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

See [`docs/RFC/RFC-0002-cross-platform-support.md`](docs/RFC/RFC-0002-cross-platform-support.md) for full tiered support model and portability rules R1–R13. Established 2026-05-04.

---

## Auth flow

**3 auth paths** (progressive complexity):

| Path | Flow | Use case | Status |
|---|---|---|---|
| **Path 1** | OIDC verifier (JWT external issuer) | Enterprise SSO + Keycloak/OAuth2 | v0.5.0 (planned) |
| **Path 2** | API key (`POST /auth/exchange`) | Consumer-grade (agents, SDK, MCP) | v0.4.0+ ✅ |
| **Path 3** | CLI `gradatum-admin token issue` | Bootstrap + operator recovery | v0.4.0+ ✅ |

**Path 2 implementation**:

- **API key storage**: `gradatum-acl-auth::ApiKeyStore` trait + `SqliteApiKeyStore` impl, keys hashed argon2id (OWASP 2023 compliant: m=19456 KiB, t=2, p=1)
- **Endpoint**: `POST /auth/exchange {api_key: "..."}` (hors JWT middleware, standalone route)
- **Response**: `ExchangeResponse { token (JWT EdDSA), ttl_secs, scopes, tenant_id, kid }`
- **JWT**: EdDSA Ed25519 (24h service TTL / 1h human TTL per scope)
- **Revocation**: `SqliteRevocationStore` checked runtime on all exchange calls
- **TrustContext**: Mandatory `tenant_id` propagation via middleware
- **Grants**: flat scopes (`["admin"]`), granular scopes deferred

**ACL integration**: Bearer token hashed + stored in preset `bearer.toml`, glob patterns match loci read/write.

---

## Deployment

**Systemd pattern** (production-ready):

- **Service units**: `gradatum-server.service` (MemoryMax=512M) + `gradatum-worker.service` (MemoryMax=1G, MemorySwapMax=0)
- **User/group**: `gradatum` UID/GID 985 (static allocation, verified non-collision on the deployment host)
- **State directory**: `/var/lib/gradatum/{config,db,md,vault}/` (StateDirectory + LogDirectory systemd directives)
- **Startup order**: `gradatum-admin init --root /var/lib/gradatum` once (idempotent, includes `--preset flat|hierarchical|...`), then `systemctl start gradatum-server` → wait health OK → `systemctl start gradatum-worker`
- **Init script**: `scripts/install-gradatum-services.sh` (end-to-end setup: user check, build, binary install, state init, service creation, startup, acceptance test)

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
- `smoke-alpha-5.sh`: 9-step acceptance test (auth path 2 criteria, write→curator→read, audit JSONL generation)
- Smoke result (2026-05-07): 4 PASS / 5 WARN / 0 FAIL (auth runtime + deploy patterns validated, docs/non-blocking warnings)

**Post-tag fixes** :

- **Drift #5**: queue_path convention unified `<root>/db/queue.sqlite` (align db/ folder layout)
- **Drift #6**: `gradatum-admin init --preset` hierarchical/flat embedded via `include_str!` (reproducible install)
- **README.md** (`packaging/systemd/`): clarified init command signature + UID/GID alignment + phase B lessons

---

## Audit trail

Two distinct audit systems coexist:

| Layer | Type | Location |
|---|---|---|
| Core audit | `AuditEvent` + `AuditEventType` (rich enum, ULID correlation) | `gradatum-core::audit` — SQLite `audit_trail` + JSONL SIEM |
| HTTP audit | `HttpAuditEvent` flat (bearer JWT actor, JCS content_hash) | `gradatum-core::audit::http` + `gradatum-server::audit_jsonl` |

`JsonlFileSink` (production): daily rotation on UTC date, files `audit.YYYY-MM-DD.jsonl` mode `0640`, immediate flush per event. Trait `AuditSink` is pluggable (noop for tests).

`content_hash_jcs()`: `sha256(JCS RFC 8785 canonical)` → `"sha256:<hex64>"`. Produces identical hashes for JSON objects with different key ordering.

---

## Endpoints

Server `:19090` exposes the following HTTP (REST) endpoints — MCP parity via `gradatum-mcp-stub`.

**Body limits** : `/mcp` capped at **512 KiB** (`RequestBodyLimitLayer`, applied at the service level — `DefaultBodyLimit` is ineffective on rmcp) ; `/internal/v1/persist/embedding` capped at **512 KiB** (`DefaultBodyLimit::max(EMBEDDING_BODY_LIMIT)`).

| Endpoint | Method | Description | Version |
|---|---|---|---|
| `/health` | GET | Service + dependency status | v0.1.0 |
| `/auth/exchange` | POST | API key → JWT EdDSA Ed25519 (TTL 24h) | v0.4.0 |
| `/api/v1/vault_write` | POST | Async note ingestion (curator + embed pipeline) | v0.1.0 |
| `/api/v1/vault_read` | GET | Read by ID or title (title lookup + redirect support) | v0.4.0 |
| `/api/v1/vault_search` | POST | Hybrid search (RRF + composite + optional rerank) | v0.3.0 |
| `/api/v1/vault_status` | GET | Vault stats (note count + total size) | v0.1.0 |
| `/api/v1/vault_list` | GET | Paginated note listing (ULID cursor) | v0.1.0 |
| `/api/v1/vault_trace` | POST | Lineage multi-mode (ID / title / FTS query) | v0.3.0 |
| `/api/v1/vault_context` | POST | Context aggregation (token budget aware) | v0.3.0 |
| `/api/v1/vault_history` | POST | Note version history (v0.4.0+) | v0.4.0 |
| `/api/v1/vault_downgrade` | POST | Soft downgrade (status + replaced_by field) | v0.2.0 |
| `/api/v1/vault_classify` | POST | Heuristic + LLM curator routing | v0.1.0 |
| `/api/v1/jobs/:id` | GET | Job worker status (lease + progress) | v0.1.0 |
| `/api/v1/event-log` | POST | Gateway QaEvent ingestion (JWT + ACL) | v0.3.0 |
| `/api/v1/lessons/recall` | GET | BM25 lesson recall by class (`rank`, `semantic` params) | v0.4.4 |
| `/api/v1/proactive_recall` | POST | Pull proactive or contextual recall surface | v0.7.1 |
| `/api/v1/proactive_recall/feedback` | POST | Acceptance feedback for surfaced notes | v0.7.1 |
| `/api/v1/system/scheduled` | GET | Scheduled task health (all tasks) | v0.7.5 |
| `/api/v1/system/metrics/catalog` | GET | Curated metrics series catalog | v0.7.5 |
| `/api/v1/system/metrics/timeseries` | GET | Metrics timeseries query with downsampling | v0.7.5 |
| `/api/v1/system/traces` | GET | Filtered session trace log | v0.7.6 |
| `/api/v1/notes/by-status` | GET | Paginated notes listing by status bucket | v0.7.6 |
| `/metrics` | GET | Prometheus metrics | v0.1.0 |

---

## Workspace dependencies

- **MSRV** : 1.88 (Rust stable).
- **HTTP stack** : `axum 0.8.9` + `tower-http 0.6.10` + `reqwest 0.13.3`.
- **MCP** : `rmcp 1.x` + `schemars 1.x`.
- **Crypto** : `sha2 0.11` + `governor 0.10` + `nix 0.31` + `jsonwebtoken 10` + `ed25519-dalek 2.x`.
- **TOML** : `toml 1.1.2` + `toml_edit 0.25.11`.
- **Serde YAML** : `serde_yml 0.0.12` (replacement for deprecated `serde_yaml`).
- **Reranker** : `ort 2.0.0-rc.9` + `tokenizers 0.21` (feature `onnx-reranker` opt-in).
- **DB** : `rusqlite 0.32`, `sqlx 0.8.6` (pinned to resolve linking conflict).

---

## Worker pipeline

`gradatum-worker` consumes jobs (vault_write, embed_note, curate_note, ...) via atomic lease. Ingestion pipeline:

```
vault_write → curate_note → [B5 wikilinks] + [embed_note chained]
                  ↓
            note_links (graph)
                  ↓
            note_embeddings (semantic)
```

**Wikilink processing** : parses `[[wikilinks]]` from curated body and inserts edges into `note_links` table. Non-fatal: link resolution errors do not block job completion.

---

## Deployment scripts

| Script | Role |
|---|---|
| `scripts/install-gradatum-services.sh` | Install systemd `gradatum-server` + `gradatum-worker` (Linux x86_64). |
| `scripts/install-gradatum-stub-mcp.sh` | Install MCP stub binary + API key + sample config. |
| `scripts/smoke-*.sh` | End-to-end acceptance tests (auth, write, curate, search, lineage). |

**Auth deployment** : API key stored at `/etc/gradatum/gradatum-mcp.api-key` (mode 600) → `POST /auth/exchange` → JWT (24h TTL). MCP stub auto-refreshes when TTL < 30%.

---

## Future work

- SQL injection hardening: LIKE escape in title lookups.
- Batch lineage queries: reduce N×N round-trips via IN(...).
- Title backfill: complete for production corpus.
- Parallel wikilink resolution via `tokio::join_all`.
- Reranker configuration: make ONNX path and feature flag optional.
- Composite weight tuning: expose α, β, λ, RRF k per vault.

---

## References

- Feature history: see CHANGELOG.md for implemented features per version
- Predecessor design: the predecessor system (archived, v1.6.x) — internal, not public
- Inspirations: [mycelium-io/mycelium](https://github.com/mycelium-io/mycelium) (Rooms pattern, OpenAPI, install one-liner)
- Standards: [Apache-2.0](LICENSE) license, [CLA](CLA.md), [Contributor guide](CONTRIBUTING.md)

---

*This document is updated by Gradatum maintainers after each architectural change. Last update: 2026-07-01 — v0.7.6: context assembly pipeline, reference mode + session window, proactive recall, agent identity injection via MCP, temporal search and decay, scheduled task health observability, curated metrics timeseries, Studio activity and notes browsing, distill validation gate.*
