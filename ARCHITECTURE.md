# Gradatum — Architecture

> Source of truth for technical design. Updated as the project evolves.
> Last architectural review : 2026-08-23 — workspace version 2.1.0, Rust edition 2024, MSRV 1.91.
> Delta since 2026-08-15: no change to the crate graph, the module boundaries or the data model.
> Increments 2.0.2 through 2.0.9 touched the delivery chain only — CI gates, release manifest and
> release policy. Notably `2.0.9`: the public-surface gate now derives its escalation regime from
> the release rank, and the deviation inventory it reads became a list.
> This header deliberately carries no commit anchor. An anchor written by hand into the
> very file it dates cannot be kept accurate: making it current requires naming the commit
> that writes it, which is a fixed point of the hash. For the document's actual position in
> history, ask git — `git log -1 -- ARCHITECTURE.md`.
> Initial design reviewed by an internal multi-expert panel — 2026-05-01.

---


## Memory intelligence layer (introduced in v0.7.6)

> All endpoint fields introduced here are optional; omitting them preserves prior behavior
> exactly. See [CHANGELOG.md](CHANGELOG.md) `[0.7.6]` for the full list of changes in that
> release, and `[2.0.0]` for the current one — including its **breaking changes**.

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

- **`ContextConfig`** TOML block (`[context]`): `default_budget_tokens`, `top_n_candidates`,
  `max_skills`, `skills_budget_fraction`, `embed_timeout_ms`.
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

### Agent identity via MCP (`identity` section — v0.7.6)

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

### Temporal search and decay (`temporal_index` — v0.7.6)

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

### Scheduled task health observability (v0.7.6)

Observability for the 9 recurring `tokio::interval` tasks in `gradatum-server`.

```
gradatum-server/main.rs — 9 recurring tasks instrumented:
    telemetry-flush · purge-event-log · purge-session-trace · purge-read-usage
    review-promote · proactive-refresh · active-recall-purge · metric-sample · audit-dedup
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

### Curated metrics timeseries (v0.7.6)

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

## v0.5.2 → v0.6.4 subsystems

> Shipped between v0.4.6 and v0.6.4. See [CHANGELOG.md](CHANGELOG.md) for the full change log.

### Code-map subsystem — `code-<project>` logical vault

A derived code index, **distinct from the main memory vault**. Zero LLM cost — static ingestion only (tree-sitter). The logical vault is identified by the `code-<project-name>` naming convention.

```
GIT SOURCES
    │
    ▼
gradatum-ingest (crate)
    tree-sitter parsers, one Cargo feature per language:
        code-rust (default) · code-python · code-bash · code-typescript
        (gradatum-admin enables all four)
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
- Since F-70 (2026-07-12, `4663740`) : deps stored in BOTH terminal and qualified `Type::method` form (self receivers + explicitly-typed bindings resolved ; precision>recall — unresolvable receivers fall back to terminal-only ; stdlib container types denylisted). `reverse_deps("Type::method")` now finds idiomatic method callers.
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
- **session-log Tier 1** : `session_trace` table (migration 0015, 90-day retention) + `POST /api/v1/session-log/trace` (append-only, `agent_id` = JWT sub, server-assigned rather than client-supplied). No prompt or response content is stored, but `target` (≤ 512 chars) and `intent` (≤ 200 chars) are free-form client-supplied strings and are not filtered.
- **vault_write in-place** : `note_id` + `expected_sha256` → in-place update; anti-fail-open guard (malformed SHA → 400 before 409).
- **Cross-tenant mitigation** : defense-in-depth across 6 layers (gate /auth/exchange + central middleware + JWT-derived handlers + worker + api_key + audit). Validated end-to-end in production.

---

## v0.4.4 → v0.4.6 — 2026-06-11

- **gradatum-studio** (publishable crate, bundle `dist/` versionné — F-131 `2e274bea`) : React+TS+Vite admin UI (dashboard, notes and note detail, search, review queue, jobs, system, activity, login — the route table in `crates/gradatum-studio/src/App.tsx` is authoritative), served by gradatum-server via `tower-http` ServeDir on `/ui/*` (SPA fallback, CSP + security headers — `gradatum-server/src/studio.rs`). Auth: API key → JWT (**localStorage** client-side, key `gradatum_studio_jwt_persist`, persisted across reloads; the API key itself is never persisted). Studio requests a short-TTL JWT (scope `human`, 1 h). Bundle deployed to `/usr/share/gradatum/ui` (configurable `[studio] ui_dir`).
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

> **Initial design detail** : The `Note` pivot is structured as **4 layers** (identity immutable / canonical Note / extensions distributed / versioning + overrides). The design rationale, constraints, and trade-offs are not recorded in [CHANGELOG.md](CHANGELOG.md) — this document is their only written trace.

---

## 4 plans

```
┌────────────────────────────────────────────────────────────────────┐
│  CONTROL PLANE   (3 separate binaries, independently scalable)     │
├────────────────────────────────────────────────────────────────────┤
│  gradatum-server   stateless facade HTTP/MCP rmcp 1.6 stream :19090│
│  gradatum-worker   async queue consumer (curator + maintenance)    │
│  gradatum-admin    CLI ops (init/token/api-key/backfill/jobs/vault) │
└──────────────────────────────────┬─────────────────────────────────┘
                                   │
┌──────────────────────────────────┴─────────────────────────────────┐
│  DATA PLANE      (workspace 31 crates total, 26 publishable)         │
├────────────────────────────────────────────────────────────────────┤
│  gradatum-core         shared primitives (errors, ids, types)      │
│  gradatum-dto          wire-contract DTOs (single source of truth) │
│  gradatum-markdown     parse/serialize MD + frontmatter + wikilinks│
│  gradatum-vault        multi-vault registry + lifecycle + swap     │
│  gradatum-storage      FS/object abstraction (opendal): fs, s3     │
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
│                        + speculative decoding config fields (v0.7.6+ : spec_type,               │
│                        draft_model_path, spec_draft_n_max, spec_draft_p_min — validated,         │
│                        default-deny ALLOWED_EXTRA_FLAGS unchanged, absent = zero args delta)      │
│                        + v0.7.7 (locally deployed, E1 cutover 2026-07-10): wait_ready()         │
│                        detects dead child immediately (bind-fail resilience) + ExecStartPre      │
│                        wait-for-port-free.sh guard against port-race on restart;                  │
│                        --backend-sampling added to ALLOWED_EXTRA_FLAGS                            │
│  gradatum-acl-policy   ACL presets + globset pattern matching       │
│  gradatum-acl-auth     API-key store + bearer token verification    │
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
│    + v0.7.7 (locally deployed, E1 cutover): router.rs +            │
│      smart_router.rs — curator router (think/no-think              │
│      pre-classifier, X-Reasoning-Mode overrides router default),    │
│      router.enabled=false by default (opt-in). metrics.rs:          │
│      gateway_router_decisions_                                      │
│      total{source}, gateway_router_fallback_total{reason},         │
│      gateway_router_curator_latency_seconds,                       │
│      gateway_router_system_latency_seconds sur /metrics            │
└────────────────────────────────────────────────────────────────────┘
                                   │
┌──────────────────────────────────┴─────────────────────────────────┐
│  CLIENTS         (1 distributed binary + 2 libraries;              │
│                    1 retired binary, source kept in-tree)          │
├────────────────────────────────────────────────────────────────────┤
│  gradatum-mcp-stub  RETIRED in 2.0.0 — was adapter MCP stdio → HTTP│
│                     (thin proxy); publish = false, no longer built │
│                     or distributed. MCP clients connect to /mcp    │
│                     on gradatum-server directly (see § API surface │
│                     topology, further down in this document).      │
│  gradatum-cli       end-user CLI (placeholder, not implemented)    │
│  gradatum-sdk-rs    Rust SDK for direct integration                │
│  gradatum           umbrella SDK facade (feature-gated re-exports) │
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

**JobStatus enum (8 variants)** : Pending, Running, Waiting, Done, Failed, DLQ, Cancelled, Conflict

**Migrations** (queue database, `crates/gradatum-db-sqlite/migrations/`) :
- `006_apalis_bootstrap` : `gradatum_jobs` table + indices + lease column
- `007_jobs_kind_indexed` : denormalised `kind` column + index — native SQL filtering
  (`WHERE kind = ?`) without deserialising the payload BLOB
- `008_idempotency` : idempotency_key column + unique constraint
- `009_jobs_v2_drain` : one-shot drain of the legacy `jobs_v2` pending rows to DLQ
- `010_backfill_kind` : backfill of `kind` from the payload JSON (`$.spec.kind.type`)
- `011_jobs_tenant_scope` : `tenant_id TEXT NOT NULL DEFAULT 'main'` + index. Value derived
  from the job spec at enqueue time (`gradatum_core::spec_tenant`), **not** from the caller.
  Filtering is conditional: `None` → no clause (byte-identical, flag OFF) ·
  `Some(t)` → `AND tenant_id = ?` (isolation ON, 404 anti-disclosure)

---

## Semantic hierarchy (6 levels)

```
GRADATUM        1 instance = 1 systemd service
   ↓
VAULTS          multi-vault first-class (default "main", staging/bench-* on demand)
                gated by [multi_tenant] enabled — OFF by default, registry = {main}
                one shared SQLite index + FTS5, partitioned by the vault_id dimension
                of every composite key; one Markdown storage tree per vault
   ↓
LOCI            logical subdivisions, isolated by bearer ACL
                path-like: "human", "main-agent", "projecta/backend", "projectb/tester"
   ↓
SECTIONS        14 canonical: decisions, architecture, debug, reasoning, feedback,
                lessons-learned, retrospectives, experiments, agent-issues, reference, council,
                project-map, identity, snapshot
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

1. **Core (this project)** — generic primitives: `Locus` (path string), `ConsumerEntry { identity, read_patterns, write_patterns }` (identity matched in clear, no token hashing), `globset` pattern matching
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
   ├─ [SUB-AGENT VALIDATOR]   reviewer, tester
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
| Markdown files (`vault/<tenant>/*.md`) | **Source of truth.** Human-readable. Compatible with Obsidian/Logseq. Survives if Gradatum is down. |
| SQLite index (`vault/.gradatum/index.db`) | Index + cache. Derived from the Markdown files, but **not currently rebuildable from them** — see below. |

**Drift detection was inert from `1.0.0` to `2.0.0`; the write path was wired in `2.0.1`.** The
helper had always existed (`gradatum-index::drift::scan_phase_a`, three-level size → prefix-4 KiB →
full SHA-256 against the `file_checksums` table), but **`file_checksums` was never populated on the
write path** — so every scan iterated an empty table and returned all zeros *while exposing its
metrics*. That is the costly shape of a false green: it does not stay silent, it reassures. The
condition was documented in the code itself and survived 99 days.

`2.0.1` adds the `upsert_file_checksum` call in `write_note_inner`, **fail-open** (a checksum
failure logs a warning and preserves the note write). Measured after the first real write:
`file_checksums` 0 → 1.

**Still missing, tracked as follow-up work** — the scan remains partial and must not be read as
full coverage:
1. the 3206 pre-existing files are **not** back-filled, so the scan only sees what was written
   after the wiring;
2. `scan_phase_a` enumerates `list_file_checksums()`, i.e. **the index** — it is structurally
   blind to a file the index ignores, whatever the back-fill does;
3. a live note with no embedding is drift of the same nature as a diverging hash, and is currently
   detected by no one.

**Index rebuild remains unavailable**: detection signals, it does not repair — the repair entry
point stays gated by design.

**Index rebuild is likewise not available in `1.0.0`.** There is no `gradatum-admin reindex`
subcommand, and the `ReIndex` job kind is a stub: every mode (`FtsOnly`, `MissingOnly`,
`VectorsOnly`, `Full`) is rejected by the handler rather than silently returning `Ok`. Recovering
from a lost or corrupted `index.db` is therefore a manual operation in `1.0.0`; treat the index as
state to back up, not as a derived artefact you can regenerate on demand.

---

## Storage layout

> **Single source of truth for these paths**: `crates/gradatum-core/src/paths.rs`, whose
> golden tests pin `vault_index_path`, `vault_dir_index_path`, `queue_db_path` and
> `config_dir` byte-for-byte. Hand-written `root.join(...)` derivations are forbidden
> elsewhere in the workspace. If this tree and `paths.rs` ever disagree, `paths.rs` wins
> and this tree is the bug.

```
<storage.root>/                        (default: /var/lib/gradatum)
│
├── config/                            ← config_dir(root)
│   ├── server.toml                    (server config)
│   ├── bearer.toml                    (ACL, generated from a preset)
│   ├── admin.bearer.txt               (admin bearer token)
│   └── jwt-signing-key.secret         (Ed25519 signing seed, chmod 600 — BACK THIS UP)
│
├── db/                                (three distinct databases — not one)
│   ├── queue.sqlite                   ← queue_db_path(root) — worker job queue;
│   │                                   holds note bodies (gradatum_jobs.payload,
│   │                                   jobs.payload_json) — see SECURITY.md
│   ├── revocation.sqlite              (auth path 2 revocation store)
│   └── api_keys.sqlite                (ApiKeyStore, argon2id hashes)
│
├── audit/                             ← <storage.root>/audit — NOT under vault/
│   ├── audit.YYYY-MM-DD.jsonl         (HTTP audit sink, daily UTC rotation, mode 0640;
│   │                                   holds note bodies — see SECURITY.md; always local,
│   │                                   independent of [storage])
│   └── audit-report-<vault>-*.{json,md}, audit-commands-<vault>-*.sh   (audit/dedup job
│                                       output, opt-in via [audit] enabled; written through
│                                       the same [storage] backend as the vault — follows it
│                                       to the object store when service = "s3"; see SECURITY.md)
│
├── md/                                (optional, empty by default)
│
└── vault/                             ← <vault_root>, singular
    ├── .gradatum/
    │   ├── index.db                   ← vault_index_path(root) — WAL: notes, embeddings, audit;
    │   │                               holds note bodies (body_text column) — see SECURITY.md
    │   ├── config.toml                (optional; [history] and [audit] blocks — see SECURITY.md.
    │   │                               Absent by default, in which case defaults apply)
    │   └── overrides/<tenant>/
    ├── .archive/                      (deleted notes, mirror layout, destroyed at retention deadline)
    │   └── <tenant>/
    │       ├── <ULID>.md
    │       └── .history/<ULID>/
    ├── main/                          (one directory per tenant / logical vault)
    │   ├── <ULID>.md                  (note; nested under a locus/section subdirectory
    │   │                               only when the note carries one)
    │   └── .history/<ULID>/<ts_ms>.md (Copy-on-Write version snapshots)
    └── <other-tenant>/                (e.g. default, test, code-<project>)
```

Five locations hold note bodies in plaintext and are the ones an operator must locate
before reasoning about data at rest: three directories — `vault/<tenant>/` (and its
`.history/`), `vault/.archive/`, and `<storage.root>/audit/` (delete tombstones only) —
plus two SQLite files outside this tree, `vault/.gradatum/index.db` (`body_text` column)
and `<storage.root>/db/queue.sqlite` (`gradatum_jobs.payload` / `jobs.payload_json`
columns). See SECURITY.md for the complete inventory and the retention and erasure
semantics of each — in particular, `forget` removes none of them.

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
-- Forward-compat (F-19): processed flag reserved for a future Job::Distill consumer.
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
    processed    INTEGER NOT NULL DEFAULT 0,   -- 0=pending, 1=consumed by Job::Distill (F-19)
    created_at   INTEGER NOT NULL               -- epoch ms server-side insertion
);
CREATE INDEX IF NOT EXISTS idx_event_log_created   ON event_log(created_at);
CREATE INDEX IF NOT EXISTS idx_event_log_tenant    ON event_log(tenant_id);
CREATE INDEX IF NOT EXISTS idx_event_log_feature   ON event_log(feature_id);
CREATE INDEX IF NOT EXISTS idx_event_log_processed ON event_log(processed);
```

> **The excerpt above is not comment-for-comment faithful, deliberately.** The migration file
> (`crates/gradatum-index/migrations/0006_event_log.sql`) attributes the `processed` flag to a
> `0.5.0` release in three of its comments. That version was never published — the 0.5 line went
> 0.5.2 → 0.6.4 with nothing in between — and a migration is immutable once applied, sqlx
> checksumming it at startup. The comment therefore cannot be corrected in place. The excerpt
> states the intent (F-19 forward-compat) without propagating a version that never existed.

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

**Note on section mapping** : the `Section` enum has 14 variants (was 11 before the identity/project-map and snapshot sections were added). Notes with `section_hint="council"` map to `Section::Council` → stored as `section="council"` → `c_kind="episodic"` (decision-based). Prior versions (v0.3.x) lacked this variant; notes fell back to `Section::Reference` in those older versions.

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

### Multi-vault schema migrations (0030 → 0043)

The 1.0.0 line makes `vault_id` a **first-class key dimension** rather than a filter column.
Before this work a single ULID identified at most one row globally; two vaults holding the
same ULID could therefore clobber each other's rows. Migrations 0032 → 0039 recompose the
keys so that identity is `(vault_id, …)` everywhere.

All of them are byte-identical at `[multi_tenant] enabled = false` (the production default):
existing data is `vault_id = 'main'` only, so `(main, X) ≡ X`. Each migration ships a
matching `*.down.sql` (the runner itself is forward-only; rollback is manual).

| Migration | Object | Change |
|---|---|---|
| 0030 `tenants_grants` | `tenants`, `tenant_vault_grants` | Tenant↔vault allow-list (`access ∈ {read, write}`). Absence of a row is a **refusal** (fail-closed). Additive: no `ALTER` on `notes`. |
| 0031 `tenants_status_deleted` | `tenants.status` | CHECK widened to `{active, suspended, deleted}` — vault soft-delete. |
| 0032 `notes_composite_pk` | `notes` | PRIMARY KEY `id` → `(vault_id, id)`. Root cause of the cross-vault hijack class. Child `REFERENCES notes(id)` FKs removed (no longer valid); cascade DELETE becomes explicit in `delete_note_from_index`. |
| 0033 `child_tables_vault_id` | `note_embeddings`, `note_history`, `note_audit_trail` | `vault_id NOT NULL` added + PKs recomposed (`note_audit_trail` keeps its `id` PK; `vault_id` scopes the cascade). |
| 0034 `child_tables_composite_pk` | `note_index`, `temporal_index`, `note_overrides` | PKs recomposed to include `vault_id` — closes the write-collision class on `INSERT OR REPLACE` / `ON CONFLICT DO UPDATE`. |
| 0035 `redirect_table_vault_id` | `redirect_table` | Adds `vault_id`; PK `title_slug` → `(vault_id, title_slug)`. Closes clobber (write), cross-read (resolve) and cross-delete (by ULID). |
| 0036 `override_locus_bearer_vault` | `note_overrides` | Re-keys legacy `Locus`/`Bearer` overrides from the `'_unset'` global sentinel to the real vault. Data-only. |
| 0037 `archive_active_vault_scope` | `uidx_archive_active` | Partial unique index `note_id` → `(vault_id, note_id)`. Availability fix (cross-vault archive DoS), not a leak. |
| 0038 `ann_composite_vault` | `note_embeddings_ann` (vec0) | Drops the global `note_id PRIMARY KEY`; partition identity becomes `(vault_id, embedder_id)`. Table recreated **empty** and rebuilt at boot from `note_embeddings` (source of truth). Gated by the `vec0` extension: not applied while `search.ann_backend = BruteForce`. |
| 0039 `child_tables_composite_fk` | `note_audit_trail`, `note_embeddings`, `note_history` | Restores the referential guard removed by 0032: `FOREIGN KEY (vault_id, note_id) REFERENCES notes(vault_id, id) ON DELETE CASCADE`. Effective, not decorative (`PRAGMA foreign_keys = ON` at runtime). |
| 0040 `grants_section_scope` | `tenant_vault_grants` | L3 (F-121, ledger pré-flip) : grant **SECTION-scopé**. `ALTER TABLE ... ADD COLUMN section` nullable — `NULL` = grant vault-entier = sémantique C1 stricte. Rows existing (seed `main↔main`, self-grants `provision_vault`) stay `NULL` → zero data migration. Serveur `tenant_guard` exige que le grant COUVRE la section demandée. PK reste `(tenant_id, vault_id)` — au plus une ligne par (tenant, vault). Inerte à OFF (byte-identical v1.0.0). |
| 0041 `feature_counter` | `feature_counter` | F-41-adjacent : compteur persistant **per-vault** (`vault_id` PK) pour l'allocation ATOMIQUE des numéros de carte project-map (`[[feature:F-XX]]`). `value` = dernier numéro alloué ; l'allocation rend `max(value, max dérivé des cartes) + 1` (le dérivé recalculé à chaque appel corrige le plancher). Pas de seed en dur. Inerte tant qu'aucune allocation (`allocate_feature_number`). |
| 0042 `agent_vault_grants` | `agent_vault_grants` | Substrat **agent↔vault** (lot B6, plan v1.0.0) : duplique `tenant_vault_grants` (0030) un cran plus bas — l'agent, pas le tenant. `access ∈ {read, write}` ('write' couvre la lecture), colonne `section` nullable, PK `(agent_id, vault_id)`. Absence de ligne = **REFUS** (fail-closed, invariant 5). Seed idempotent `INSERT OR IGNORE ('main-agent', 'main', 'write')`. Inerte tant qu'aucune consultation — câblée en B7 (identité) + B8 (portée section). |
| 0043 `project_map_roles` | `notes` (2 colonnes) | F-171, `2.0.2` : `role_kind` / `role_status` **dérivées à l'écriture** du bloc de rôles d'une carte project-map, via l'analyseur `parse_link` de `gradatum-core` — **le même** que le validateur de schéma, source unique du format. Rendues interrogeables par `list_notes_filtered` (index) puis au contrat MCP (`vault_list` accepte les deux filtres). Rétro-remplissage par sous-commande d'administration **idempotente**, en **une seule transaction fail-closed** — un partiel rapporté comme succès y est structurellement impossible ; marche à blanc par transaction annulée. Déployé sur l'index de production le 2026-08-15 : 329 cartes typées, 0 non typable. ⚠️ **Pas de filtre sur la version** : « quelles cartes pour telle version ? » reste sans chemin propre. |

`ON DELETE CASCADE` on 0039 is deliberate: three code paths delete `notes` rows, and only
`delete_note_from_index` cascades manually. A RESTRICT FK would have broken
`write_note_derived_batch` and `delete_vault_from_index`.

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
- **v0.3.0 → v0.7.x**: `load_or_generate_jwt_key()` in `gradatum-server/src/jwt_key_boot.rs` — raw 32-byte Ed25519 seed loaded from `FileSecretsProvider`, the directory derived from the parent of the config key `jwt_private_key_path` when it sat under `storage.root`, `<storage.root>/config` otherwise
- **From v1.0.0**: `gradatum_auth::key_store::load_or_generate()` — same raw 32-byte Ed25519 seed, but the directory is derived by `gradatum_core::paths::config_dir(&storage.root)` on both the server and `gradatum-admin token issue`, and is no longer configurable. `jwt_key_boot.rs` and the config key `jwt_private_key_path` are removed. Seed atomically written chmod 600 on first boot, dir 0700. See *Path 2 implementation* below for the resulting file layout.
- **Deploy impact (C13)**: first deploy replaces ephemeral key → **one-time invalidation** of all live JWTs. Consumers must re-exchange API key for new JWT. Operator gate required. The v1.0.0 directory change carries the same impact for the narrow class of deployments described in CHANGELOG.md.

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

> **Critical**: Gradatum core is single-instance. HA/redundancy patterns are **external**, not built-in — the recommended ones are listed below. Operational setup lives in [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md).

Recommended patterns:

- **Database HA**: [Litestream](https://litestream.io/) → SFTP/S3/NFS continuous replication (RPO ~seconds)
- **Markdown HA**: rsync cron 5min OR Syncthing continuous OR lsyncd
- **Backup**: no built-in subcommand in `1.0.0` — snapshot `<storage.root>` (Markdown files,
  `index.db`, `db/`, `config/`) with the operator's own tooling, server stopped or via a
  filesystem/ZFS snapshot

Failover is manual (or scripted via systemd watchdog if user wants).

---

## Platform support

Gradatum targets **Linux exclusively** (x86_64, aarch64) as native platform, as of 2026-06-05;
Windows is supported **via Docker only** (no native binary); macOS is out of scope entirely. The
tiered Linux-primary/Windows-secondary/macOS-future model that used to govern this section was
retired along with the written-RFC process that defined it (see [`GOVERNANCE.md`](GOVERNANCE.md)
§ Structural change tracking).

[`docs/DEPLOYMENT.md` § Platform support](docs/DEPLOYMENT.md#platform-support) is the normative
statement — this section is a pointer, not a second copy, to avoid the two drifting apart.

---

## Auth flow

**3 auth paths** (progressive complexity):

| Path | Flow | Use case | Status |
|---|---|---|---|
| **Path 1** | OIDC verifier (JWT external issuer) | Enterprise SSO + Keycloak/OAuth2 | v0.5.0 (planned) |
| **Path 2** | API key (`POST /auth/exchange`) | Consumer-grade (agents, SDK, MCP) | v0.4.0+ ✅ |
| **Path 3** | CLI `gradatum-admin token issue` | Bootstrap + operator recovery | v1.0.0 ✅ |

**Path 2 implementation**:

- **API key storage**: `gradatum-acl-auth::ApiKeyStore` trait + `SqliteApiKeyStore` impl, keys hashed argon2id (OWASP 2023 compliant: m=19456 KiB, t=2, p=1)
- **Endpoint**: `POST /auth/exchange {api_key: "..."}` (hors JWT middleware, standalone route)
- **Response**: `ExchangeResponse { token (JWT EdDSA), ttl_secs, scopes, tenant_id, kid }`
- **JWT**: EdDSA Ed25519 (24h service TTL / 1h human TTL per scope)
- **Revocation**: `SqliteRevocationStore` is read on every request and fails closed. **Nothing writes to it in `1.0.0`**: `api-key revoke` marks the key row only, and the JWT verification path does not re-read the originating key's state, so a token issued before a revocation stays valid until `exp`. Cutting outstanding tokens requires rotating the signing seed (see SECURITY.md). Per-token revocation is planned for a `1.x` release.
- **TrustContext**: Mandatory `tenant_id` propagation via middleware
- **Grants**: flat scopes (`["admin"]`). Per-key **write**-scope enforcement exists but is gated behind `[multi_tenant] enabled` (default `false`): with it on, a write path requires the key to carry one of `write`, `admin`, `service` — see `WRITE_SCOPES` in `api_v1/tenant_guard.rs`, matched by exact string equality. **Read access is not governed by key scopes in either mode** — it is governed by vault grants (`require_read_grant`) and the locus ACL. Write-scope enforcement outside multi-tenant mode is deferred.

**Signing key**: both the server and `gradatum-admin token issue` sign with the 32-byte seed at `<storage.root>/config/jwt-signing-key.secret` (`kid = gradatum-v0`), the single source of truth. The directory is derived on both sides by `gradatum_core::paths::config_dir(&storage.root)` and is not configurable. The server creates it on first boot; the CLI only ever loads it. **This is the file to back up** — the former `config/jwt.private.pem` / `jwt.public.pem` pair is no longer read or generated, and backing it up protects nothing. Deleting the seed and restarting the server mints a new one and invalidates every outstanding token.

**ACL integration**: the authenticated identity (JWT `sub`, Studio user, or mTLS CN) is compared in clear against a consumer entry in the preset (e.g. `bearer.toml`) — no token hashing is performed — and that consumer's glob patterns govern read/write access per locus. With no policy file present the ACL is fail-closed (deny-all).

---

## Deployment

**Systemd pattern** (production-ready):

- **Service units**: `gradatum-server.service` (MemoryMax=512M) + `gradatum-worker.service` (MemoryMax=1G, MemorySwapMax=0)
- **User/group**: `gradatum` UID/GID 985 (static allocation, verified non-collision on the deployment host)
- **State directory**: `/var/lib/gradatum/{config,db,md,vault}/` (StateDirectory + LogDirectory systemd directives)
- **Startup order**: `gradatum-admin init --root /var/lib/gradatum` once (idempotent, includes `--preset flat|hierarchical|...`), then `systemctl start gradatum-server` → wait health OK → `systemctl start gradatum-worker`
- **Init script**: `scripts/install-gradatum-services.sh` (end-to-end setup: user check, build, binary install, state init, service creation, startup, acceptance test)

**Storage layout**: see the [Storage layout](#storage-layout) section above — it is the only
tree in this document. A second copy used to live here and had already drifted from the
first; two trees describing one layout guarantee that at least one of them is lying, so
this one is a pointer rather than a duplicate.

**Health checks**:

- `/health` endpoint returns `{"status":"ok"|"degraded", ...}`
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

`JsonlFileSink` (production): daily rotation on UTC date, files `audit.YYYY-MM-DD.jsonl` mode `0640`, immediate flush per event. Trait `AuditSink` is pluggable (noop for tests). Wired at boot on `<storage.root>/audit`.

**No retention is applied to these files.** The `[audit]` config block (`rotation`, `retention_days`, `strict_mode`) is defined in `gradatum-core::config` but not yet wired, and no GC sweeps the audit directory — files accumulate until an operator prunes them. This matters because a `vault_delete` writes the **full note body** into the audit record as a recovery tombstone (`api_v1/delete.rs`), so deleted content outlives the archive retention window there.

`content_hash_jcs()`: `sha256(JCS RFC 8785 canonical)` → `"sha256:<hex64>"`. Produces identical hashes for JSON objects with different key ordering.

---

## API surface topology

`gradatum-server` exposes **one TCP listener, one HTTP port** (default `19090`, see
[Guide E — Ports & configuration](docs/guides/E-ports-and-config.md)). Every surface — native
HTTP API, MCP, health probe, token issuance, the Studio UI — is multiplexed on that single port
by path-prefix routing (`axum::Router::nest()` / `.merge()`, `crates/gradatum-server/src/main.rs`):

```
http://<host>:19090
├── /api/v1/...   → native HTTP API (auth required)         — gradatum-cli, gradatum-sdk-rs, curl, custom integrations
├── /mcp          → native MCP server, StreamableHTTP (auth required) — any MCP-aware client that can attach a header
├── /health       → liveness/readiness probe (unauthenticated)
├── /auth/exchange → API key → JWT issuance (unauthenticated — this is the token issuer itself)
└── /ui/...       → gradatum-studio SPA (served by gradatum-server, own JWT-based login)
```

A separate listener, bound to loopback only by default (`127.0.0.1:19091`, security caveat C7,
[SECURITY.md § Hardening defaults](SECURITY.md#hardening-defaults)), exposes Prometheus
`/metrics`. It is not reachable through the main port.

This single-port, path-prefix topology was the routing decision of a design note
(`RFC-0003 §§3–4`) written before `gradatum-server` existed; the note itself is retired (see
[`GOVERNANCE.md`](GOVERNANCE.md) § Structural change tracking), but the decision it records is
still the router's actual shape, verified against `crates/gradatum-server/src/main.rs`. Two
prefixes that same note proposed, `/sse` (MCP-over-SSE, legacy transport) and a dedicated
`/admin` prefix, were never implemented — admin-scoped operations live under `/api/v1/...`,
gated by ACL scope rather than by a separate path.

MCP client integration used to run through a companion binary, `gradatum-mcp-stub`, translating
stdio MCP calls into HTTP requests against `gradatum-server`. That binary is **retired as of
`2.0.0`** — source kept in-tree (`publish = false`), no longer built or distributed. Every MCP
client now connects directly to `/mcp` above; see
[Guide D — MCP & Studio](docs/guides/D-mcp-and-studio.md) for current client setup.

---

## Endpoints

Server `:19090` exposes the following HTTP (REST) endpoints.

**Body limits** : `/mcp` capped at **512 KiB** (`RequestBodyLimitLayer`, applied at the service level — `DefaultBodyLimit` is ineffective on rmcp) ; `/internal/v1/persist/embedding` capped at **512 KiB** (`DefaultBodyLimit::max(EMBEDDING_BODY_LIMIT)`).

Methods and paths below are the ones registered on the router at `fb0742e5`
(`crates/gradatum-server/src/api_v1/mod.rs`, `internal/mod.rs`, `lib.rs`). A `—` in the
version column means the introduction version was not established by this pass — it is
**not** a claim that the endpoint is recent.

| Endpoint | Method | Description | Version |
|---|---|---|---|
| `/health` | GET | Service + dependency status (incl. `build_sha`) | 0.1.0-alpha |
| `/auth/exchange` | POST | API key → JWT EdDSA Ed25519 (TTL 24 h service / 1 h when `scope=human`) | v0.4.0 |
| `/metrics` | GET | Prometheus metrics — **separate listener**, `[server] metrics_bind` (default `127.0.0.1:19091`); non-loopback bind aborts boot | 0.1.0-alpha |
| `/ui/*` | GET | Studio SPA bundle (`ServeDir` + SPA fallback) | v0.4.6 |
| `/mcp` | POST | Native MCP server, StreamableHTTP (`rmcp`) | v0.6.4 |
| `/api/v1/vault_write` | POST | Async note ingestion (curator + embed pipeline) | 0.1.0-alpha |
| `/api/v1/vault_read` | POST | Read by ID or title (title lookup + redirect support) | v0.4.0 |
| `/api/v1/vault_search` | POST | Hybrid search (RRF + composite + optional rerank) | v0.3.0 |
| `/api/v1/vault_status` | GET | Vault stats (note count + total size) | 0.1.0-alpha |
| `/api/v1/vault_list` | POST | Paginated note listing (ULID cursor) | 0.1.0-alpha |
| `/api/v1/vault_authors` | GET | Distinct author facet | — |
| `/api/v1/vault_tags` | GET | Distinct tag facet | — |
| `/api/v1/vault_graph` | POST | Wikilink graph neighbourhood | — |
| `/api/v1/vault_links` | POST | Inbound/outbound links of a note | — |
| `/api/v1/vault_trace` | POST | Lineage multi-mode (ID / title / FTS query) | v0.3.0 |
| `/api/v1/vault_context` | POST | Context aggregation (token budget aware) | v0.3.0 |
| `/api/v1/vault_timeline` | POST | Chronological listing (body limit 4 KiB) | v0.5.2 |
| `/api/v1/vault_history` | POST | Note version history | v0.4.0 |
| `/api/v1/vault_history_get` | POST | Fetch one historical version | v0.4.0 |
| `/api/v1/vault_restore` | POST | Restore a historical version (CoW) | v0.4.0 |
| `/api/v1/vault_diff` | POST | Diff between two versions | v0.4.0 |
| `/api/v1/vault_downgrade` | POST | Soft downgrade (status + replaced_by field) | v0.2.0 |
| `/api/v1/vault_classify` | POST | Heuristic + LLM curator routing | 0.1.0-alpha |
| `/api/v1/vault_forget` | POST | Semantic forget — flags the note, does **not** delete it (two-step dry-run protocol, body limit 1 MiB) | v0.4.3 |
| `/api/v1/vault/forgotten` | GET | Paginated listing of forgotten notes | v0.4.3 |
| `/api/v1/vault/unforgot/{ulid}` | POST | Restore a forgotten note | v0.4.3 |
| `/api/v1/vault_archives_list` | POST | Read-only archive listing (no delete/restore/purge) | v0.8.0 |
| `/api/v1/code_scope` | POST | Code-map symbol scope (BM25-only, dedicated endpoint) | v0.5.2 |
| `/api/v1/notes/{id}` | PATCH | Partial note update | — |
| `/api/v1/notes/{id}/move` | POST | Move a note between loci (index-level) | v0.4.6 |
| `/api/v1/notes/by-status` | GET | Paginated notes listing by status bucket | v0.7.6 |
| `/api/v1/review` | GET | Pending-review queue | v0.4.6 |
| `/api/v1/dashboard` | GET | Aggregated dashboard counters | v0.4.6 |
| `/api/v1/project-map/export-features` | GET | JSON export of project-map feature cards | v0.6.4 |
| `/api/v1/jobs` | GET / POST | List jobs / create a job | v0.2.0 |
| `/api/v1/jobs/{id}/v2` | GET | Job status (ULID) | v0.2.0 |
| `/api/v1/jobs/{id}/cancel` | POST | Cancel a job | v0.2.0 |
| `/api/v1/jobs/{id}/events` | GET | Job event stream | v0.2.0 |
| `/api/v1/event-log` | POST | Gateway QaEvent ingestion (JWT + ACL, body limit 2 MiB) | v0.3.0 |
| `/api/v1/session-log/trace` | POST | Agent action tracing, append-only (body limit 4 KiB) | v0.5.2 |
| `/api/v1/lessons/recall` | GET | BM25 lesson recall by class (`rank`, `semantic` params) | v0.4.4 |
| `/api/v1/proactive_recall` | POST | Pull proactive or contextual recall surface | v0.7.6 |
| `/api/v1/proactive_recall/feedback` | POST | Acceptance feedback for surfaced notes | v0.7.6 |
| `/api/v1/system/scheduled` | GET | Scheduled task health (all tasks) | v0.7.6 |
| `/api/v1/system/metrics/catalog` | GET | Curated metrics series catalog | v0.7.6 |
| `/api/v1/system/metrics/timeseries` | GET | Metrics timeseries query with downsampling | v0.7.6 |
| `/api/v1/system/traces` | GET | Filtered session trace log | v0.7.6 |

### Internal namespace `/internal/v1/*` — loopback only, never public

A second router carries the operator/worker surface. It is bound to the internal listener
(loopback, dedicated admin token distinct from the worker token) and is **not** reachable
from the public API nor from MCP. Destructive archive and vault-lifecycle operations live
here exclusively:

```
persist/     curated · distill · embedding (512 KiB) · forget
reads/       title-lookup · id-lookup · note/{ulid}[/status|/trust|/embedding]
             notes/by-agent · notes/by-locus · notes/by-status · notes/garbage
             notes/count-unprocessed · forget/search · vaults/active
admin/       delete · archives/{list,purge,restore}
             vaults/{create,suspend,delete,purge}
```

---

## Workspace dependencies

All workspace dependencies but two are **exact-pinned** (`=x.y.z`) in the root `Cargo.toml`.
The exceptions are `stability = "0.2"` and `subtle = "2"` — the latter carries the
constant-time comparison primitives, so its range is worth knowing.
Versions below are read from that file at `6dfdb8f0` — see [DEPENDENCIES.md](DEPENDENCIES.md)
for the full graph.

- **Toolchain** : MSRV 1.91, Rust edition 2024, Cargo `resolver = "3"`.
- **HTTP stack** : `axum 0.8.9` + `axum-server 0.8.0` + `tower 0.5.2` + `tower-http 0.6.10`
  + `reqwest 0.13.3` (rustls) + `rustls 0.23.40` (aws_lc_rs provider installed at boot).
- **MCP** : `rmcp 1.6.0` + `schemars 1.0.4`.
- **Crypto / auth** : `sha2 0.11.0` + `governor 0.10.4` + `nix 0.31.2` + `argon2 0.5.3`
  + `ed25519-dalek 2.1.1` + `pkcs8 0.10.2` + `jsonwebtoken 9.3.1`.
  `jsonwebtoken` is held at 9.x on purpose: 10.x requires the `rust_crypto` feature
  (`sha2 ^0.10.7`), incompatible with the pinned `sha2 =0.11.0`.
- **TOML** : `toml 1.1.2` + `toml_edit 0.25.11` + `figment 0.10.19` (config loading).
- **Serde** : `serde 1.0.228` + `serde_json 1.0.149` + `serde_jcs 0.1.0`
  + `serde_norway 0.9.42` (YAML frontmatter; replaced `serde_yml`, itself archived and
  unsound — RUSTSEC-2025-0068/-0067) + `bincode 2.0.1`.
- **DB** : `rusqlite 0.40.2` (bundled, FTS5) + `sqlite-vec 0.1.9` (ANN, opt-in feature
  `sqlite-vec-ann`). `sqlx` is out of the resolved dependency graph entirely (removed
  2026-08-25) — `rusqlite` now backs the vault index, the job queue and sessions alike.
- **Job queue** : `apalis 1.0.0-rc.9` + `apalis-sql 1.0.0-rc.9` + `apalis-sqlite 1.0.0-rc.8`
  + `apalis-cron 1.0.0-rc.8` (rc.9 not published for the latter two).
- **Observability** : `prometheus 0.13.4` (worker) + `prometheus-client 0.22.3`
  (server, engine) + `tracing 0.1.44` / `tracing-subscriber 0.3.23`.
- **Reranker** : `ort 2.0.0-rc.9` + `tokenizers 0.21` (feature `onnx-reranker` opt-in).
- **Embeddings (CPU)** : `fastembed 4.6.0` + `ort-sys 2.0.0-rc.9` (feature `fastembed-cpu`).
- **Storage** : `opendal 0.58.1` (facade over `opendal-core` + `opendal-service-*`; feature `fs` by default, `s3`/`gcs`/`azure` opt-in).
- **Code ingest** : `tree-sitter 0.26.9` + language grammars (`rust`, `python`, `bash`,
  `typescript`), all optional.

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

## Published scripts

`scripts/` ships with the product; `scripts/internal/` does not. Membership is decided by
location, not by a hand-kept list: every file directly under `scripts/` is in the published
tree, and everything under `scripts/internal/` is operator tooling that stays in the internal
repository. There is no allow-list to keep in sync, and no wildcard promises a path that does
not exist yet.

The gate `scripts/ci-public-scripts-location.sh` enforces the rule in both directions: a
script invoked by a published surface must live directly under `scripts/`, and a script that
only ever runs from internal surfaces must live under `scripts/internal/`.

| Location | Contents | Published |
|---|---|---|
| `scripts/` | Install, start, fetch, published acceptance smokes, public CI gates. | yes |
| `scripts/internal/` | Internal registry publishing, historical data import, leak remediation, name reservation, unpublished smokes, internal CI gates. | no |

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

### Per-note usage salience (F-110 Phase 1, v1.0.0)

Per-note usage counters feeding future salience scoring — pure instrumentation, zero
client-visible behavior change (byte-identical responses proven by test).

```
Read paths (after response construction, success only, O(1) in-memory):
    vault_read           → kind=read
    vault_search         → kind=search-hit (per item) + search-hit-top3 (ranks 1-3)
    lessons/recall       → kind=search-hit
    proactive_recall     → kind=recall-surfaced (per surfaced note, post-ACL)
    proactive feedback   → kind=recall-accepted (per accepted_ulids, guard ⊆ surfaced unchanged)
        │
        ▼ NoteUsageAccumulators::record (std Mutex<HashMap>, never held across .await)
gradatum-server/main.rs — telemetry-flush loop (60 s)
    swap() accumulator → NoteUsageStore::flush_batch (UPSERT batch)
        count = count + delta · last_used_ms = MAX(...)
        errors → warn! only (best-effort, window lost, never fatal)
        │
        ▼
gradatum-index (migration 0029, STRICT)
    note_usage (tenant_id, note_id, kind) PK · count · last_used_ms
    INDEX idx_note_usage_last

Prometheus: gradatum_note_usage_total{kind} — CounterVec, bounded cardinality 5,
    incremented after successful flush only (consistency with read_usage pattern).
```

- Twin of the `read_usage_store.rs` pattern at per-note granularity.
- Merged to main `abce6c3` (2026-07-15); not yet deployed to production (separate GA gate).

---


### Salience factor, graduated forgetting, conditional distill cron — shipped in v1.0.0 (2026-07-16, all flag OFF)

Three dormant mechanisms delivered behind config flags (default OFF, byte-identical responses proven live):

- **F-110 Phase 2 — salience as 4th composite factor** (`gradatum-search::scoring`): `SalienceParams` mirrors the trust-decay pattern; when `[salience] enabled=true`, `vault_search` does ONE batch lookup on `note_usage` for the ≤50 RRF candidates and applies `× (1 + gamma·s/(s+k_norm))`. `ScoreBreakdown` gains optional `salience_*` fields (`skip_serializing_if`). Activation gate: G1/G2/G3 on ≥14 d of real data.
- **F-111 — graduated forgetting** (`gradatum-curator::audit` + `audit_job.rs`): `detect_irrelevant` conjunctive rule (live + age>90 d + zero usage in 30 d window via `MAX(last_used_ms)` + trust<0.6 + section ∉ `PROTECTED_DOWNGRADE` [9 sections incl. architecture]) feeds a new `irrelevant` report section; executor (`NoteDowngrader` over `Arc<dyn Index>::downgrade_note`, `[downgrade] enabled=false`, cap 50/run) is inert until the collection-window guard (`T0 = MIN(last_used_ms)`) is covered. Reversible only — never delete.
- **F-112 — conditional distill cron** (`gradatum-worker::schedules` + monitor): top-level `[distill_cron]` config (separate figment extraction — NOT under `[apalis]`), weekly tick measures pressure per locus (internal read `count-unprocessed`, early-exit at `pressure_min`), enqueues `Job::Distill` Semantic/Batch/Locus (`JobClass::System`/`Low`), fail-closed on dedup-read failure, cap per tick. Not registered when disabled.

---

## Multi-vault foundation (workspace `1.0.0`) — flag OFF by default

> Everything in this section sits behind `[multi_tenant] enabled` (**default `false`**).
> At OFF the server keeps the legacy single-vault `"main"` lock and responses are
> byte-identical to `0.x`. This is the isolation substrate shipped in 1.0.0; it is opt-in,
> not enabled for you.

### Two distinct dimensions: principal vs namespace

The single overloaded `tenant_id` string was split into two newtypes in `gradatum-core`
(`src/scope.rs`), because conflating them was the root of the whole cross-vault class:

```rust
pub struct TenantId(String);   // PRINCIPAL — who is calling (JWT sub / api-key owner)
pub struct VaultId(String);    // NAMESPACE — which vault the data lives in
```

A third type is a **witness**, not a value:

```rust
pub struct AclCheckedVaultId(VaultId);
```

Read and mutation-by-ULID paths take the witness instead of a raw `VaultId`. Rust cannot
prove across crate boundaries that an access check actually happened, so the guarantee is
explicitly **anti-forgetfulness, not absolute**: a request-supplied `vault_id` can no longer
reach a read *silently*, because the only ways to build the witness are two named,
greppable constructors — `attest_read_checked` (caller attests the target's Read ACL, plus
the per-vault grant when `multi_tenant.enabled = true`, was just evaluated `Allow`) and
`for_system_task` (non-HTTP context: periodic job, offline operator CLI, internal loopback
surface, where scope is guaranteed by the orchestrator). Auditing the whole surface is a
single grep over those constructor names.

DTOs follow the same split: `tenant_id: Option<TenantId>` (principal) and
`vault_id: Option<VaultId>` (namespace, `serde` default + skip). Both axes are optional on the
wire, but an omitted `tenant_id` carries no default at all: the principal is derived from the
credential by `effective_tenant`, never from a request field, and a context carrying no tenant
is refused `403`.

**`INV-P1-3`** — *the target of a write is always the principal's own vault*. This invariant
is enforced twice, on two different auth layers, deliberately **without** a shared helper:
`effective_write_vault` (public router, JWT + grant lookup) and `resolve_write_namespace`
(internal loopback listener, pure clamp). Merging them into one function was rejected: they
do not share a trust model, and a common code path would have made a loopback-only clamp
reachable from the public surface.

### Vault handle registry

```
AppState.vaults : Arc<VaultRegistry>
    RwLock<BTreeMap<VaultId, Arc<Vault>>>    (std RwLock — no .await under the guard)
    BTreeMap, not HashMap  → deterministic iteration order

    singleton(vault)         production wiring at flag OFF: exactly {main}
    add_vault(expected, v)   idempotent + fail-closed
    insert(expected, v)      fail-closed on identity mismatch
    resolve(vault_id)        → GradatumError::VaultNotFound if absent,
                               NEVER a silent fallback to the `main` singleton
```

`vault.vault_id()` (derived from the on-disk `config.toml`) is checked against the routing
key on every insertion: a silently inconsistent config cannot make a handle serve a
namespace other than the one it is routed under.

All registered handles share **one** `Arc<SqliteIndex>` (a single pool over `index.db`).
Isolation comes from the `vault_id` key dimension, not from separate databases.

Provisioning: `gradatum-admin vault create|suspend|soft-delete` reaches the internal admin
namespace, instantiates a real handle and registers it; at boot, `bootstrap_active_vaults()`
registers one handle per active vault (`list_active_vaults`) when the flag is ON, and
`{main}` alone when it is OFF.

### Tenant-scoped jobs

The job queue carries the tenant **served by the job**, derived from the job spec at enqueue
time (`gradatum_core::spec_tenant`) — never from the caller:

```
spec_tenant(&JobSpec) -> &str
    Curate/Embed/Validate → spec.tenant_id
    Ingest                → spec.vault
    Distill / Forget      → scope-derived
    Export / Migrate      → vault scope
    (exhaustive — no wildcard arm)

SqliteQueueStore   stamps tenant_id on enqueue (queue migration 011)
                   get/cancel/count/latest/list take Option<TenantId>
                       None    → no SQL clause  (byte-identical, flag OFF)
                       Some(t) → AND tenant_id = ?  (404, anti-disclosure)

create_job         403 if the spec's tenant ≠ the JWT principal (flag ON)
```

### Per-vault configuration overrides

`[per_vault.<vault_id>]` is a deliberately minimal layer (YAGNI): only `salience` and
`review_promote` are overridable. The map is empty by default.

The semantics have **three** states, not two. Writing an override that disables a feature
does *not* fall back to the global config — that is the whole point of the third state:

| TOML state | Effect for that vault |
|---|---|
| sub-table **absent** (`[per_vault.<id>.salience]` not written) | inherits the global config |
| sub-table present, `enabled = true` | **override** — the refined per-vault params apply |
| sub-table present, `enabled = false` | **disabled for this vault** — the feature is neutralised, and it does *not* revert to the (possibly active) global config |

Read "`None` means fall back to the global config" as applying to the *absence of the
sub-table* only. Conflating it with `enabled = false` is the footgun that was fixed in the
code: `enabled = false` used to silently re-enable the global, i.e. the exact inverse of
what the operator wrote.

Overrides are resolved **once at boot** into `AppState::salience_per_vault`
(`Arc<HashMap>`) — an entry exists for every vault carrying an override, the value encoding
the resolved state (`Some(params)` active, `None` disabled) — so the read path performs no
allocation. A vault with no override is simply absent from the map. Every per-vault
`salience` override is validated fail-loud at boot: an invalid one refuses the boot rather
than injecting corrupt params.

### Observability and build traceability

- Both binaries embed the build commit via `build.rs` (`cargo:rustc-env=BUILD_SHA`), exposed
  as `gradatum-server <semver> (build_sha <sha>)` in `--version` and as the `build_sha`
  field of `GET /health`. This is what makes "is the running binary the one I built?"
  answerable without guessing.
- `gradatum-engine` (F-120): the event-log sink now reads the HTTP status of every
  `POST /api/v1/event-log`. Any non-2xx (and any transport failure) increments
  `engine_event_log_errors_total{status_code}`; on `401` the sink re-exchanges its API key
  for a fresh JWT and retries the POST **once**. Before this, the JWT was exchanged once at
  boot (24 h TTL) and a `401` was an `Ok(response)` nobody inspected — the sink died
  silently. The JWT lives behind a `tokio::sync::RwLock<Zeroizing<String>>`, never held
  across an `.await`, never logged.

### Test surface

Cross-vault isolation is covered by a dedicated suite rather than by review discipline:
`crates/gradatum-index/tests/no_cross_vault_leak.rs` is a fuzzed formal gate wired as its own
CI job, alongside 14 targeted `cross_vault_*.rs` suites in `gradatum-index` and the
`isolation_*` / `handler_isolation_preflip` / `jobs_tenant_isolation` suites in
`gradatum-server`.

### Known open items

`[multi_tenant] enabled` defaults to `false` in code — a default gated by its own test
(`vaultgrant_c1::multi_tenant_flag_default_off`). The items below are the gaps that remain
**once an operator turns it on**; they do not apply to a single-vault deployment, where
`main` is the only vault and every scope resolves to it. Recorded at `fb0742e5`, they have
evolved as follows (this delta re-verified at `761f9625` on 2026-08-04):

- **Grant granularity** — *partly closed.* Migration `0040_grants_section_scope` added a
  nullable `section` column to `tenant_vault_grants`, and `VaultGrant` now carries a
  `section: Option<String>` (`None` = whole-vault, the historical semantics). What remains
  open is multiplicity, not granularity: the PK is still `(tenant_id, vault_id)`, so a given
  (tenant, vault) pair holds **at most one** grant — either whole-vault or scoped to a single
  section. Opening several distinct sections of the same vault to the same tenant would
  require rebuilding the primary key.
- **Provisioning reconciliation** — *still open.* `bootstrap_active_vaults()` registers handles for active
  vaults at boot, but there is no reconciliation pass for a vault present on disk yet absent
  from the registry (or the reverse). The boot-time GC that exists today
  (`gc_orphan_ann`) covers orphan ANN rows, not orphan vaults.
- **Agent-level grants** — *closed (lot B6→B9, 2026-08-04).* Migration `0042_agent_vault_grants`
  added the `agent_vault_grants` table (substrate; see migrations table above). The substrate was
  cabled in B7 (agent identity in middleware + boot reconciliation, `6367a2a0`), B8 built the
  infrastructure (`MissingReadGrant` + `require_agent_*` guards, `a52a6589`), and B9 wired the
  guards into `effective_write_vault` / `effective_read_vault` (`945f853f`). A1 `792d64bb` removed
  13 CLI `default_value="main"` so agent identity comes from the JWT only (holes 1+2 also fixed in
  `5cf6c9b9`: rotate guard + lying-key detection in api-key list). Re-baseline `761f9625`.

---

*This document is updated by Gradatum maintainers after each architectural change. Last update: 2026-08-04 — multi-tenant ACL agent-level batch (A1 CLI `default_value="main"` removal `792d64bb`, B6 `agent_vault_grants` table `a4d91227`, B7 middleware + boot reconciliation `6367a2a0`, B8 `MissingReadGrant` + `require_agent_*` infra `a52a6589`, B9 guards wired into `effective_write_vault`/`effective_read_vault` `945f853f`, holes 1+2 `5cf6c9b9`, migration 0042; `761f9625`). Previous: 2026-07-26 — 1.0.0 release pass (JWT signing-key SSOT `jwt-signing-key.secret` shared by server and CLI, queue path SSOT with `--db` validation, dead `curator.llm_review_*` keys removed, revocation and per-key scope caveats stated explicitly; 095bff0f). Previous: 2026-07-24 — multi-vault foundation of the 1.0.0 line (typed `TenantId`/`VaultId` split, `AclCheckedVaultId` witness, vault handle registry, index migrations 0030→0039, tenant-scoped jobs + queue migration 011, per-vault config overrides, `build_sha`, F-120 event-log JWT refresh — all behind `[multi_tenant] enabled = false`; fb0742e5). Previous: 2026-07-16 — salience/forgetting/distill-cron train complete, shipped in the 1.0.0 line (F-110 P2 salience factor, F-111 graduated forgetting, F-112 distill cron — all flag OFF; 2bab71f). Previous: 2026-07-15 — F-110 Phase 1 per-note usage salience (note_usage table, accumulator + 60 s flush, 5 kinds, Prometheus counter; abce6c3). Previous: 2026-07-11 — supervisor extra_args allowlist +--slot-prompt-similarity (a7a2044, prefix-cache slot routing); source comments neutralized for leak-scan (32f9069); no structural change. Previous: 2026-07-01 — v0.7.6: context assembly pipeline, reference mode + session window, proactive recall, agent identity injection via MCP, temporal search and decay, scheduled task health observability, curated metrics timeseries, Studio activity and notes browsing, distill validation gate.*
