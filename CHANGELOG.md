# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.6] — 2026-07-03

Upgrade from v0.6.4: one breaking server API change — `vault_context` is redesigned; its
response schema changes from `{ context, estimated_tokens, sources }` to `{ assembled_text,
included, budget_used, diagnostics, … }` and the default output is now assembled Markdown
(`mode: "raw"` retains the v0.6.4 dump text, inside the new schema). All other endpoints are
drop-in: new request fields are optional and omitting them preserves prior behavior. Operators
running `gradatum-engine` should review the breaking changes below before upgrading.

### Breaking changes (API)

- **`vault_context`: response schema replaced and default output changed.**
  - **Old response (v0.6.4)**: `{ context, estimated_tokens, sources }`.
  - **New response (v0.7.6)**: `{ assembled_text, included, budget_used, diagnostics,
    references, counts, cache_breakpoint_hint }`. `references` and `counts` are always
    present (`[]` / zeroed when `reference_mode` is off).
  - **Default output changed**: the request `mode` field defaults to `"assembled"` —
    `assembled_text` is a structured Markdown context block, not the raw FTS dump.
  - **Migration**: clients parsing the old shape must read `assembled_text` (and
    `included` instead of `sources`). To keep the v0.6.4 dump text itself, send
    `mode: "raw"` — `assembled_text` then reproduces the old `context` value
    byte-for-byte, inside the new schema.

### Breaking changes (operator)

- **`gradatum-engine`: `extra_args` is now validated against an allow-list**
  (`ALLOWED_EXTRA_FLAGS`). Flags that are managed by dedicated configuration fields are
  rejected — in particular `--n-gpu-layers` (and its aliases), which is controlled
  exclusively by the `gpu_layers` config field. An existing configuration such as
  `extra_args = ["--n-gpu-layers", "0"]` now fails at boot with
  `EngineError::BadRequest` from the `LlamaServerSupervisor`; migrate it to
  `gpu_layers = 0`.
- **`gradatum-engine`: new loopback-only `/metrics` listener on `port + 1` by default**,
  configurable via `metrics_port`. The listener always binds `127.0.0.1` regardless of
  `bind_addr`. When running multiple engine instances on contiguous ports (e.g. 11435
  and 11436), the `port + 1` default can collide with the neighbouring instance's main
  port — set `metrics_port` explicitly in that case.

### Added

#### Context assembly pipeline (`vault_context`)

`vault_context` is redesigned from a raw FTS dump into a full retrieval and assembly pipeline.

- **Retrieval**: Reciprocal Rank Fusion over BM25 (FTS5) and semantic embedding signals
  (`k=60`, configurable candidate cap).
- **Composite scoring**: `recency × PageRank × trust` applied after RRF fusion; reuses the
  `gradatum-search` scoring infrastructure.
- **Budget-aware selection**: notes are sorted by score and inlined until the `budget_tokens`
  limit; bodies are fetched lazily (only for retained notes).
- **Structured Markdown output**: per-note heading (`### title · section · date · score=X`),
  `---` separators, `[[ULID]]` source references.
- **Skill injection** (opt-in): when `inject_skills: true` and `skill_query` are set, top
  matching notes from the `skills/` section are appended to the assembled context (index-only
  lookup, no LLM call). Governed by `max_skills` and `skills_budget_fraction`.
- **New request fields** (all optional, fully backward-compatible): `budget_tokens`,
  `scoring` (`ScoringWeights`), `mode` (`Assembled` | `Raw`), `inject_skills`, `skill_query`.
- **New response fields**: `assembled_text`, `included` (`Vec<IncludedNote>` — `ulid`, `title`,
  `section`, `date`, `score`), `budget_used`, `diagnostics` (`candidates_considered`,
  `included_count`, `embed_fallback`, `skills_injected`).
- `mode=Raw` preserves the prior FTS-dump byte-for-byte (backward-compatibility fallback).
- **`ContextConfig`** TOML block (`[context]`): `default_budget_tokens`, `top_n_candidates`,
  `max_skills`, `skills_budget_fraction`, `embed_timeout_ms`.

#### Context efficiency — reference mode and session window

- **Reference mode** (`reference_mode: bool`, default `false`): notes are inlined up to the
  token budget (`budget_tokens` per request, `default_budget_tokens` in config); those beyond
  are returned as lightweight stubs `{ ulid, title, section, snippet }` up to the
  `stub_budget_tokens` config limit; stubs are dereferenceable via `vault_read(ulid)`.
- **Session window** (`session_id`): notes already sent inline in the current session are
  returned as stubs on repeat calls, never re-inlined. `mode=compact` re-ranks the freshest
  top-K notes inline and returns all prior-sent notes as stubs — useful for context compaction
  at session boundaries. Folded notes remain dereferenceable.
- **`cache_breakpoint_hint`**: boolean hint emitted when assembled context exceeds a configured
  threshold, signalling the consumer to insert a prompt-cache boundary.
- **New response field `references`**: `Vec<ReferenceStub>` (additive, default `[]`).

#### Proactive recall

- **Background refresh scheduler**: a `tokio::interval` task enqueues a `ProactiveRefresh`
  job every 900 s (configurable via `[proactive_recall] refresh_interval_secs`, floor 60 s).
  The job derives an implicit salience query from the K most recently written notes, runs
  cross-section retrieval over `lessons-learned`, `reasoning`, and `decisions`, applies
  composite scoring, and stores the top-N surface (default 8) in `proactive_surface`.
- **`POST /api/v1/proactive_recall`** — pull endpoint with two modes:
  - `proactive` (no `context` field) — reads the pre-computed surface (cheap path).
  - `contextual` (with `context` field) — on-demand RRF over the same sections.
  - Response: `{ recall_id, mode, items: [{ulid, title, section, snippet, score}] }`.
- **`POST /api/v1/proactive_recall/feedback`** — acceptance feedback; records which surfaced
  notes were used (`accepted_ulids ⊆ surfaced_ulids`, 400 otherwise); idempotent.
- **MCP tools**: `vault_proactive_recall`, `vault_proactive_recall_feedback` (bring MCP
  surface to **23 tools**).
- **Lessons recall enrichment**: `/api/v1/lessons/recall` gains two optional parameters —
  `rank` (`relevance` | `recency-boosted`) and `semantic` (`false` | `true`; degrades
  gracefully to BM25 when the embedding service is unavailable).

#### Agent identity via MCP

- **`identity` section** (13th canonical section): migration 0024 creates the section;
  migration 0025 backfills `title` for existing identity notes from their first H1.
- **Soul validator** (`validate_soul()`): checks structural sections (INVARIANTS / GATES /
  NARRATIVE); handles `extends:` resolution (bounded depth); accepts `scope` field.
- **MCP `initialize` identity injection**: on MCP `initialize`, the server injects the
  tenant's agent identity note from the `identity` section into the MCP `instructions` field.
  Access to `identity` from `vault_search` is fail-closed for non-privileged callers.
- **Write ACL for `identity`**: only the bearer whose `agent_id` matches the JWT `sub` may
  write their own identity note; `doc_kind` is forced to `Static`.
- **`write_check` drift detection**: `write_check::check_category_section()` detects
  category↔section drift on note ingestion (warn-only). Metric
  `gradatum_write_check_total{rule}` incremented on each detected drift.
- **Worker guard**: reclassification of an `identity` note is a no-op.

#### Temporal search and decay

- **`vault_search` temporal range filter**: new optional request fields `from_ms` and `to_ms`
  (epoch milliseconds). Applied on both the FTS and semantic paths via `LEFT JOIN
  temporal_index`. `anchor_ms` is now included in every `SearchHit`.
- **`vault_write` `occurred_at` field**: optional string (ISO 8601). Validated at write time;
  propagated through the curation pipeline to populate `anchor_ms` in `temporal_index`.
- **Recency factor uses `anchor_ms`**: the exponential-decay recency signal in composite
  scoring now uses the canonical `anchor_ms` from `temporal_index` (fallback: `created_at`
  when no temporal anchor is set). Applied consistently across `vault_search` and
  `vault_context`.

#### Review auto-promotion job

- **`review-promote` scheduled job**: notes left in `staging` or `pending-review` are
  automatically promoted to `live` after 14 days (`age_days`), on an hourly tick
  (`interval_secs`, floor 60 s) capped at 200 notes per tick (`max_per_tick`).
  **Enabled by default** — set `[review_promote] enabled = false` to opt out.

#### Scheduled task health observability

- **Migration 0026**: two new tables in `index.db`:
  - `scheduled_task_health` — one row per task: `task_name` (PK), `last_run_ms`,
    `last_outcome` (`ok`/`error`), `last_duration_ms`, `last_error`, `run_count`, `updated_at`.
  - `scheduled_task_error` — append-only errors table; indexed on `(task_name, occurred_ms)`;
    lazy 7-day purge on each error insert.
- **`record_task_run` helper** (`gradatum-index`): upserts `scheduled_task_health`, appends to
  `scheduled_task_error` on error; never panics.
- **Boot seeding**: all 8 scheduled task names are seeded with `last_run_ms: null` at startup
  so the System page shows all tasks immediately, before the first tick fires.
- **All recurring tasks instrumented**: each task body captures duration and outcome and calls
  `record_task_run`. Task behavior is unchanged — instrumentation is purely additive.
- **`GET /api/v1/system/scheduled`** (JWT auth): returns all registered tasks with
  `{ name, last_run_ms, last_outcome, last_duration_ms, last_error, run_count, errors_24h,
  interval_secs }`. `errors_24h` is a `COUNT` over `scheduled_task_error` in the last
  86 400 000 ms. `last_error` is sanitized before emission.
- **Studio System page**: new nav item; renders all tasks with per-task badges
  (ok / error / overdue), `errors_24h` highlighted in red when > 0, last run as relative
  time, duration, and last error message.
- **Studio Dashboard scheduler widget**: compact summary (task count, how many in error or
  overdue) linking to the System page.

#### Curated metrics timeseries

- **Migration 0027**: table `metric_sample` in `index.db`:
  ```sql
  metric_sample (series TEXT NOT NULL, ts_ms INTEGER NOT NULL, value REAL NOT NULL,
                 PRIMARY KEY (series, ts_ms)) WITHOUT ROWID;
  CREATE INDEX idx_metric_sample_ts ON metric_sample(ts_ms);
  ```
- **Curated collection**: `collect_curated_samples()` re-encodes the Prometheus registry and
  parses a static allowlist of ~60 series in 4 groups (read-path usage, context efficiency,
  server health, write pipeline). Counters → direct value; histograms → two separate series
  (`_sum` / `_count`); high-cardinality `http.*` labels are aggregated.
- **`metric-sample` scheduled task**: runs every 60 s; collects curated samples, batch-inserts
  into `metric_sample`, and runs a lazy purge of rows older than 14 days. Errors are logged
  at `warn` and do not interrupt the task.
- **`GET /api/v1/system/metrics/catalog`**: returns the full static curated series list
  `{ series: [{ key, group, kind, unit, instrumented }] }`. No database query.
- **`GET /api/v1/system/metrics/timeseries`**: query parameters — `series` (comma-separated,
  validated against the allowlist; 400 if unknown, > `MAX_SERIES=32`, or duplicated),
  `from_ms` / `to_ms` (inclusive bounds; 400 if `from >= to`), `max_points` (default 500,
  cap 2000). Server-side downsampling via `GROUP BY (ts_ms / bucket_ms)` when the span
  exceeds `max_points` raw points. Response: `{ from_ms, to_ms, bucket_secs, series: [{ key,
  points: [{ ts_ms, value }] }] }`.
- **Studio metrics charts**: `SystemPage` gains a metrics section below the task health grid.
  Range selector (1 h / 24 h / 7 d / 14 d, default 24 h), auto-refresh toggle (default on,
  60 s), and 4 collapsible groups of interactive uPlot charts. Series not yet instrumented
  are rendered grayed out rather than hidden.

#### Activity and notes browsing in Studio

- **`GET /api/v1/system/traces`** (JWT auth, read scope): paginated, filterable read of the
  `session_trace` table. Filters: `agent_id`, `session_id`, `action_type`, date range.
- **Studio Activity page**: table view of session trace records with expandable detail rows
  and auto-refresh. Accessible from the main navigation.
- **`GET /api/v1/notes/by-status`** (JWT auth, read scope): paginated listing of notes
  grouped by status (live, downgraded, pending-review, etc.) using keyset pagination.
- **Studio Notes page**: lists notes by status bucket, including archived (downgraded) notes;
  links to per-note detail.

#### Distill validation gate

Synthesized notes now pass through a deterministic scoring gate before being stored.

- **`Job::Validate` + `ValidateSpec`** (`gradatum-core`): new job variant carrying the
  synthesis note id, body, source texts, source trusts, and base trust. Bincode positional
  encoding preserved (new variant at position 8; all prior positions stable).
- **`quality_score` scorer** (`gradatum-worker/src/quality_score.rs`): pure, deterministic,
  zero I/O. Composite formula: `grounding × recency_sources × trust_sources × num_penalty ×
  entity_penalty`, clamped to `[0.0, 1.0]`:
  - `grounding` — cosine similarity between the synthesis embedding and the mean centroid of
    source embeddings.
  - `recency_sources` — exponential-decay weight on source `anchor_ms` values.
  - `trust_sources` — mean trust across sources.
  - `num_penalty` — numeric-coherence penalty: each number in the synthesis traceable to no
    source subtracts 0.15 (floor 0.5).
  - `entity_penalty` — orphan-entity penalty: each uppercase-initial token in the synthesis
    absent from all sources subtracts 0.10 (floor 0.5).
- **`handle_validate` worker**: disposition after scoring:
  - `score ≥ 0.75` → stored with `base_trust`; no extra tag.
  - `score < 0.75` → stored with `trust = base_trust × score`, `quality-low` tag appended.
    Sources are marked `processed` + `derived-into` (per-source failures are non-fatal).
  - Scoring errors fall back to score 1.0 (pass) — no synthesis is ever discarded due to an
    embedder failure.
- **`handle_distill` refactored**: enqueues `Job::Validate` instead of persisting directly;
  persist and source-marking are delegated to `handle_validate`.
- **`PersistDistillRequest.tags`**: tags supplied in a distill request are now propagated into
  the persisted note's frontmatter (previously dropped).
- **Validate worker**: `max_retries = 2` (persist is idempotent on the pre-allocated
  `note_id`; retries prevent permanent note loss on transient I/O failures).
  `ensure_main_tenant` cross-tenant guard applied at entry.

### Fixed

- **Phantom-write guard**: `vault_write` now returns `409 Conflict` for notes whose Markdown
  file is absent from storage (phantom notes). The response body distinguishes a phantom
  conflict from a `expected_sha256` mismatch.
- **`vault_read` for phantom notes**: returns `404 Not Found` (previously `500 Internal
  Server Error`) when a note's body file is missing from storage.
- **`vault_read` status**: returns the status stored in the index (authoritative) rather than
  the status extracted from a potentially stale Markdown frontmatter.
- **Temporal anchor preserved on note update**: the `vault_write` RMW path and the worker
  reclassify path now preserve an existing `anchor_ms` when updating a note that already has
  one. An anchor is overwritten only when the note body genuinely changes.
- **`vault_context` recency aligned with `vault_search`**: the context assembly pipeline now
  uses `anchor_ms` as the recency reference, in parity with `vault_search`. Previously
  `vault_context` used `created_at`, causing inconsistent recency ranking between the two
  surfaces.

### Security

- **Cross-agent identity isolation**: notes in the protected `identity` section are excluded
  from all search, list, timeline, by-status, review, trace, graph, and link surfaces for
  non-privileged callers, across the full HTTP API and MCP tool surface. Read and write are
  guarded per-handler: an agent may only read or write its own identity note (matched by JWT
  `sub`), and `vault_search` over `identity` is fail-closed for non-privileged callers
  (empty result set rather than an error, to avoid existence-oracle attacks). See
  `SECURITY.md` ("Agent identity security") for the full model and its limitations.
- **PII scrub**: maintainer-specific home-directory paths and personal identifiers removed
  from test fixtures and public source.

### Tests

3038 passed / 0 failed (`cargo nextest --workspace --release`); `clippy --workspace
--all-targets -- -D warnings` clean.

## [0.6.9] — 2026-06-26

Consolidates gateway and engine fixes shipped since 0.6.8 — no breaking API or
configuration changes.

### Fixed

- **Gateway keep-alive SSE on `/v1/messages`** — ping frames are now emitted across
  the entire prefill window (not only during headers), preventing upstream proxies
  and supervisors from closing the connection during long cold-start prefills on the
  local vision engine.
- **GBNF-bloat sanitisation in `translate_tools`** — `sanitize_schema()` now strips
  GBNF-incompatible JSON Schema constraints (`maximum`, `maxLength`, `pattern`,
  `minItems`, …) recursively at every nesting level, including inside
  `additionalProperties` and `prefixItems`. This allows the full Claude Code
  tool-set (~50 tools, 832 GBNF rules) to be forwarded to the b9780 engine without
  parser saturation, while preserving deterministic schema output for prompt-cache
  reuse.
- **`tool_choice=auto` enforcement on `/v1/messages`** — when the client sends a
  `tools` array without an explicit `tool_choice`, the gateway now injects
  `tool_choice: {type: "auto"}` before forwarding to the engine, preventing
  unexpected forced-tool behaviour.

## [0.6.8] — 2026-06-24

Consolidates versions 0.6.5–0.6.8 (not previously tagged publicly). Drop-in upgrade
from 0.6.4 — no breaking API or configuration changes.

### Added

- **Anthropic Messages API gateway** (`POST /v1/messages`) — the gateway now speaks
  the Anthropic protocol in addition to the OpenAI-compatible surface, enabling a
  fully local Claude Code experience backed by a local vision model. Includes
  `count_tokens` support and Anthropic-shaped JSON error envelopes.
- **Prompt-cache LCP enablement for the vision engine** — the engine supervisor
  allow-list accepts `--kv-unified`, unlocking llama.cpp's unified-KV prompt cache
  (b9780+). Multi-turn requests reuse the prior turn's KV via longest-common-prefix
  matching, collapsing per-turn prefill from O(full context) to O(new tokens).
- **Project-map integration**: feature backlog cards and roadmap data are now accessible
  on the project map.

### Fixed

- `gradatum-engine` `is_binary_allowed` accepts versioned `llama-server-<ver>`
  wrappers via a bounded suffix check (alphanumeric-only after a single dash),
  while the path-prefix allow-list remains the primary guard.
- Streaming `message_delta` now reports a non-zero `output_tokens` estimate.
- Engine config boot validation for `[messages]` aliases (only when configured).

## [0.6.4] — 2026-06-20

This release catches the public version number up with the real deployed version,
ending a historical gap where internal releases were not tagged publicly. v0.6.4
is a drop-in upgrade from v0.5.2 — no breaking API or configuration changes.

### Added

#### Native MCP server (`/mcp` — StreamableHTTP)

Gradatum now ships a first-party MCP server endpoint at `POST /mcp`, implemented
via `rmcp` (StreamableHTTP transport). It exposes **21 tools** covering the full
vault API surface (`vault_search`, `vault_write`, `vault_read`, `vault_timeline`,
`code_scope`, `vault_lessons_recall`, `vault_classify`, and more). `vault_classify`
returns a heuristic section classification for an existing note (offline, no LLM
inference) — a fast preview of where the curator would route the note.

Authentication is enforced on both `list_tools` and `call_tool`: any request
without a valid `Authorization: Bearer <api-key>` is rejected before tool
dispatch. The MCP schema for tools that take no parameters emits the
MCP-conformant `{"type":"object","properties":{}}` shape rather than an empty
object, preventing client-side schema validation errors.

The previous stdio MCP stub remains available for setups that require it; the
native endpoint is independent.

#### Queue DAG — job dependency chains (`await_jobs`)

The job queue now supports dependency chains: a job can declare a list of
predecessor job ULIDs in the `await_jobs` field. The worker will not promote a
waiting job to `Pending` until all its predecessors have reached `Done`. Two new
`QueueStore` methods underpin this:

- `find_awaiting` — scans for `Waiting` jobs whose dependencies are fully
  resolved, using `LIKE`-based matching to avoid collisions with partial ULID
  prefixes.
- `set_pending` — idempotent promotion from `Waiting` to `Pending`; re-running
  on an already-`Pending` job is a no-op.

Cascade promotion runs automatically in the worker's `complete` path (best-effort;
failures are logged and do not roll back the completed job). A recovery sweep
runs on each worker cycle to catch any `Waiting` jobs whose predecessors completed
before the cascade was in place.

#### Code index — multi-language support and reverse-dependency queries

**Multi-language parsing** (`LanguageParser` trait): the code index now supports
Bash, TypeScript, TSX (React JSX), and Python in addition to Rust. Each language
is backed by a dedicated tree-sitter grammar; the dispatch layer selects the
correct parser by file extension at ingest time. The `gradatum-admin code ingest`
and `code update` commands index all supported languages in a single pass.

**Reverse-dependency queries** (`include_callers` in `code_scope`): the
`POST /api/v1/code_scope` request now accepts an opt-in `include_callers: bool`
field (default `false`). When enabled, the response includes a `callers` list
— symbols in the index that call or reference the queried symbol. This field is
additive and fully backward-compatible: existing callers that omit it see no
change in response shape.

**Known limitation**: reverse-dependency detection for method calls of the form
`self.method()` has partial coverage (the callee is recorded as a terminal name
rather than a qualified `Type::method` form). Free-function and associated-function
calls are resolved correctly. This limitation is documented in-tree and will be
addressed in a future release.

#### project-map — 12th canonical section

A new `project-map` section provides a structured, graph-backed way to track work
units as vault notes. Each project-map note carries a mandatory typed-wikilink
schema: `[[project:…]]`, `[[status:…]]`, and `[[kind:…]]` are required;
`[[version:project/x.y.z]]` is optional. A write-time validator enforces the
schema when `section_hint="project-map"` is provided, rejecting notes that fail
cardinality or charset constraints.

The wikilink resolver routes typed links to synthetic graph nodes (not to
note ULIDs), keeping the project graph structurally separate from the memory
vault. A pull-based admin command, `gradatum-admin project-map render <project>`,
generates a `TODO.md` view from the work-status graph without using semantic
search.

A backfill command (`gradatum-admin backfill-changelog`) parses `CHANGELOG.md`
entries and writes them as project-map cards, enabling the graph to represent
historical releases.

### Changed

- **MCP schema SSOT**: the helper that builds `inputSchema` objects for MCP tool
  definitions is now a single source of truth in `gradatum-dto`, shared across the
  native server and any future transports. Previously four copies existed; a silent
  divergence in one of them was the root cause of client-side schema rejection
  bugs.
- **Engine path allowlist**: the local inference engine supervisor now accepts
  versioned install paths (e.g. `/opt/llama-server-0.0.0`) via a prefix allowlist
  (`/opt/llama-*`), in addition to the unversioned canonical path.
- **`Section::from_canonical_str` SSOT**: all section-name parsing is now routed
  through a single function, eliminating hardcoded string lists that had to be
  kept in sync manually.
- **Version number alignment**: the public release number now matches the
  internally deployed version. Earlier releases in the `v0.6.x` series were
  deployed internally but not tagged publicly; this release closes that gap.
  No API or behavior changes are implied by the version jump from `v0.5.2`.

### Fixed

- **Graceful shutdown race**: signal handlers (SIGTERM / SIGINT) are now installed
  before the server binds to its port. Previously, a signal delivered in the
  narrow window between handler registration and bind could leave the server
  unresponsive to shutdown requests.
- **`/health` `oldest_age_secs` always zero**: the health endpoint was reading
  from an empty table (`jobs_v2`) instead of the real queue (`gradatum_jobs`),
  reporting `oldest_age_secs: 0` unconditionally. It now reads from the correct
  table and filters to `Pending` jobs only, avoiding false `degraded` signals
  from other statuses.
- **`/health` `build_sha` unknown**: the deployed server reported
  `build_sha: "unknown"` because the value was not injected at compile time.
  A `build.rs` script now captures the Git commit hash and embeds it via
  `VERGEN_GIT_SHA`; the field is populated on every build made from a Git
  repository.
- **`server_smoke` readiness check flakiness**: the startup readiness poll no
  longer uses a fixed sleep; it polls `GET /health` until it receives a `200`
  response (up to a bounded timeout), making the smoke check reliable regardless
  of machine load.
- **`note_links` edges never written**: a format mismatch between the wikilink
  writer (`[[section:ULID]]`) and the resolver (expecting only the ULID component)
  caused all wikilink edges to be silently dropped. New notes also produced zero
  edges. An internal `/internal/v1/id-lookup` endpoint now enables the resolver
  to accept the full typed-wikilink format. Graph queries (`vault_graph`,
  `vault_trace`) return correct results on all notes written after this fix;
  historical notes can be backfilled with `gradatum-admin backfill-note-links`.
- **`Section` parse inconsistency**: `project-map` notes written with
  `section_hint="project-map"` were silently reclassified to other sections
  because the curator's section list and the persistence layer's section list were
  separate hardcoded arrays that had diverged. Both sites now delegate to
  `Section::ALL` via `Section::from_canonical_str`.

### Security

- **`list_tools` authentication gate**: the MCP `list_tools` handler was
  unauthenticated, leaking the full tool catalogue to any network client that
  could reach the server port. It now requires a valid Bearer api-key, consistent
  with `call_tool`.
- **Body limit on `/mcp`** (anti-DoS): `POST /mcp` is now wrapped with a
  512 KiB `RequestBodyLimitLayer`. Requests exceeding this limit receive `413
  Payload Too Large` before the body is read. The `DefaultBodyLimit` middleware
  that applies to other routes does not cover `rmcp`-handled routes due to a
  tower service composition constraint; this layer closes that gap.
- **Three unauthenticated write endpoints closed**: `vault_downgrade`,
  `patch_note`, and `move_note_locus` were missing authentication and ACL checks
  on the loopback interface. Each now routes through an `_impl` handler that
  enforces ACL and tenant isolation, consistent with all other write endpoints.

### Tests

2337 passed / 0 failed / 10 skipped (`cargo nextest --workspace --release`); `clippy --workspace --all-targets -- -D warnings` clean.

## [0.5.2] — 2026-06-15

First public release since v0.4.3. No breaking changes; drop-in upgrade. Adds a static code index, a timeline API, action tracing, a proof-of-absence search signal, native TLS termination, and a suite of correctness and security fixes.

### Added

#### Code index (`gradatum-admin code ingest` / `gradatum-admin code update`)

A derived index of source code symbols, separate from the memory vault. Zero LLM cost — all ingestion is static analysis via tree-sitter.

- **`gradatum-admin code ingest`**: initial full ingest from a Git repository root; idempotent (repeated runs produce no duplicates).
- **`gradatum-admin code update`**: O(diff) incremental update driven by `git diff`; only changed files are re-ingested.
- **Drift detection**: the index tracks a per-file content hash. A stale entry is flagged before results are served so consumers always see fresh data or an explicit stale signal.
- **Ingest visibility** (`--visibility pub|all`): index public symbols only (default, unchanged) or all symbols including private.

#### `POST /api/v1/code_scope` — code search endpoint

Query the code index by vault identifier plus an optional symbol filter. Returns `DerivedSymbol` records (functions, structs, enums, traits, impls) with span and SHA-256 content hash.

- **`include_body`** / **`body_budget_tokens`** fields: retrieve the exact source span of a matching symbol on demand; path anti-traversal guard enforced unconditionally.
- **MCP tool `code_scope`**: thin proxy over the endpoint; schema auto-derived via schemars.

#### `POST /api/v1/vault_timeline` — chronological note listing

Paginated timeline of notes ordered by temporal anchor, with cursor-based pagination.

- **`as_of_ms`** / **`include_expired`** fields: query the vault as of a past point in time, or include notes whose `valid_until` has elapsed.
- **`valid_until` extraction**: the server extracts this field from note frontmatter and populates an internal temporal index used for as-of filtering.
- Protected sections excluded from all timeline results (0/49 leaks confirmed).
- **MCP tool `vault_timeline`**: thin proxy.

#### `POST /api/v1/session-log/trace` — agent action tracing

Fire-and-forget endpoint for recording agent actions. Append-only; no update or delete surface. `agent_id` is the JWT `sub` (server-assigned stable identifier, not free-form). Fields: `session_id`, `tenant_id`, `ts_ms`, `action_type`, `target`, `intent`, `outcome`, `marker`, `ref`. Retention: 90 days by default (configurable via `[session_trace] retention_days` in `gradatum.toml`).

#### `include_corpus_count` — proof-of-absence signal in `vault_search`

New optional request field (default `false`). When enabled, the response includes `corpus_match_count: Option<u64>` (full-corpus BM25/FTS5 match count, unbounded by the result limit K) and `corpus_count_capped: bool`. Distinguishes a genuine absence from a retrieval miss — useful in RAG pipelines where "nothing returned" is ambiguous.

- BM25/FTS5-only, unconditional: ANN semantic-only hits are not counted (invariant: `corpus_match_count >= count(results where !is_semantic_only)`).
- Opt-in count query (~2–5 ms); response is byte-for-byte identical when `include_corpus_count` is `false`.

#### Native TLS termination (`[server.tls]`)

The server can now terminate TLS directly, without a reverse proxy, via a new optional config block:

```toml
[server.tls]
cert_path = "/path/to/cert.pem"
key_path  = "/path/to/key.pem"
```

Backed by rustls 0.23 (TLS 1.2+/1.3 only) via `axum-server` `bind_rustls`. Boot is fail-closed: the certificate and key are loaded before the server binds; any load failure aborts startup rather than falling back to cleartext.

**Enforcement**: a non-loopback `bind` address without `[server.tls]` is refused at startup (fail-closed). The default deployment (`127.0.0.1:19090`, no `[server.tls]` block) is unchanged — loopback behind a reverse proxy requires no configuration change.

#### `vault_write` in-place update

`vault_write` now honors the `note_id` + `expected_sha256` fields for in-place updates:

- `note_id` present → update in-place; absent → fresh note; invalid → `400`.
- `expected_sha256` absent on an existing note → `409 Conflict` (prevents silent overwrite).
- `400`/`409` rejections are recorded in the audit trail.

### Changed

- **Studio session persistence**: the Studio now persists the session JWT in `localStorage` (key `gradatum_studio_jwt_persist`, 24h TTL) with a client-side expiry check at mount. No more api-key re-entry after reload. The `ak_` api-key itself is never persisted.
- **Job endpoints hardened** (`/api/v1/jobs`): all job routes now require a bearer JWT with ACL; the legacy `GET /api/v1/jobs/{id}` route is secured. `POST /api/v1/jobs` deserializes the real `JobKind`.
- **Gateway metrics cardinality**: `route` and `provider` Prometheus labels are bounded by an allowlist (unknown values map to `"other"`), preventing unbounded label growth from malformed or unexpected requests.
- **`vault_read` now returns `title`**: the `title` field is populated in `VaultReadResponse`, making read-modify-write (RMW) workflows reliable without a separate lookup.

### Fixed

- **Optimistic-lock `Conflict` not surfaced**: a `vault_write` update with a stale `expected_sha256` was correctly rejected (note never modified) but the job reported `Done` instead of `Conflict`, making the conflict silently invisible to the caller. Fixed by an anti-clobber guard in the job completion path; the `Conflict` status is now preserved through the ack cycle.
- **Code-ingest crash on multibyte source**: byte-slice truncation in the Rust parser is now char-safe (`char_indices`); no panic on source files with multibyte characters or emoji near the truncation boundary.
- **Code-ingest Unicode/space paths**: `git ls-files` and `git diff --name-status` now use `-z` + `core.quotepath=off` + `--no-renames` (NUL-split); paths with spaces or accented characters are ingested and purged correctly.
- **Code-ingest interrupted-ingest drift**: an atomicity marker is placed before any destructive mutation; an interrupted ingest forces a full rebuild on the next run instead of leaving silent drift in the index.
- **`vault_read` title always null**: `vault_read` previously returned `title: null` for all notes; the field is now populated from the note's stored title or extracted from the first Markdown H1.

### Security

- **Non-loopback without TLS refused**: a `bind` address outside loopback now requires `[server.tls]` to be configured; the server refuses to start if neither condition holds (fail-closed). See §Native TLS above.
- **Defense-in-depth against cross-tenant data access** (6-layer fix): two separate `tenant_id` fields (JWT claims vs. request body) were never reconciled, creating a latent path to cross-tenant reads. Fixed across 6 layers: `/auth/exchange` gate, central middleware, handler-level JWT derivation, cross-vault read clamp, worker job rejection, and api_key issuance guard. All six layers covered by tests; smoke-tested LIVE (403 on all cross-tenant paths, 200 on legitimate paths).
- **`code_scope` path anti-traversal**: the path guard is unconditional (not gated on request parameters); symlink traversal is also blocked (IB-5 asserted in tests, IB-7 symlink covered).
- **`vault_write` fail-open closed**: malformed `expected_sha256` returns `400` before reaching the `409` conflict check, closing a guard ordering issue that could have allowed a malformed hash to bypass the conflict check.
- **`vault_timeline` protected sections**: `PROTECTED_FORGET` sections are excluded from all timeline results; 0 leaks in 49 confirmed cases.

### Privacy

Two new at-rest data surfaces in `index.db`:

- **`event_log`**: LLM gateway call metadata (route, model alias, provider, latency, status code). No prompt or response content. Retention: 30 days by default (`[event_log] retention_days`).
- **`session_trace`**: agent action tracing entries. Fields: see `POST /api/v1/session-log/trace` above. Retention: 90 days by default (`[session_trace] retention_days`).
- **HTTP audit log** — planned v0.6.x. In v0.5.2 the server runs with `NoopAuditSink`: no audit files are written and there is **no `[audit]` configuration block**. The `HttpAuditEvent` data shape is defined in `gradatum_core::audit::http` but not wired to any sink.

### Tests

1925 passed / 0 failed (`cargo nextest --workspace --release`); `clippy --workspace --all-targets -- -D warnings` clean.

## [0.4.6] — 2026-06-11

Introduces a read-mostly operator UI over the vault, along with the backend API surfaces it
consumes. No breaking changes; drop-in upgrade from v0.4.5.

### Added

- **gradatum Studio**: 5 surfaces (React + TypeScript + Vite bundle) served by `ServeDir` under `/ui/*` without auth (LAN — the JS is public). Auth flow: the operator pastes an api-key → `POST /auth/exchange` → JWT stored in `sessionStorage` (never `localStorage`) → `Authorization: Bearer` on every `/api/v1/*` call. Hardened static serving: strict CSP, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Permissions-Policy: geolocation=(), microphone=(), camera=()`. SPA fallback (`ServeDir.fallback(ServeFile index.html)`): deep-links / refresh on client-side routes serve `index.html`; a missing bundle still returns a clean 404.
- **Opt-in score breakdown in `vault_search`**: new request field `include_scores: bool` (default `false`, fully backward-compatible under `deny_unknown_fields`) enriches each `SearchHit` with a `ScoreBreakdown` object (`rrf_score`, `recency_factor`, `pagerank_factor`, `in_degree`, `trust_raw`, `trust_decayed`, `composite`, optional `bm25_rank` / `sem_rank`). Signals were already computed by the scoring pipeline and discarded — they are now exposed only when requested. No `rerank` column (NoopReranker by default). The legacy hardcoded `trust: 0.5` field is documented as deprecated. The MCP tool schema auto-derives the new field via schemars.
- **Review queue endpoint**: new `GET /api/v1/review` (auth, paginated by ULID cursor) listing notes with `status IN ('pending-review', 'staging')`, with `provenance` (distinguishing `distilled` from curator) and a distinct legacy `staging` badge. `confidence` is not exposed (not persisted — honest copy).
- **Dashboard endpoint**: new `GET /api/v1/dashboard` (behind auth; `/health` stays unauthenticated) aggregating, with no new table: `notes_by_status` (tolerant of out-of-enum legacy statuses), `forgotten_count`, `jobs_by_status` (`GROUP BY`, DLQ included), `queue_depth`, `wal_size_bytes` (`null` = "n/a", never a lying 0), and the last job summary. New trait methods `count_notes_by_status` (`DocumentStore`) and `count_jobs_by_status` (`QueueStore`, default empty + native `GROUP BY` override in `SqliteQueueStore`).
- **Move-to-locus endpoint**: new `POST /api/v1/notes/{id}/move {locus}` performing an index-level `UPDATE notes.locus` (consistent with `vault_downgrade` / `patch_note_status`); the ULID is preserved (no redirect table). Strict `LocusId::parse` validation: non-empty, charset `[a-z0-9-/]`, ≤128 bytes, anti-traversal. Clean `400` / `404` / `422`. Physical `.md` relocation is intentionally deferred and documented in the handler contract.

### Changed

- **Curator routes low-confidence notes to `PendingReview`**: `CurateOutcome::Pending` now writes `NoteStatus::PendingReview` instead of `Staging` at the four worker sites (dispatch + apalis, create + reclassify), factored through a single source of truth `gradatum_curator::outcome_to_status` (`Admitted→Live`, `Pending→PendingReview`, `Rejected→None`) to close the parity-bug class. Semantically correct: `PendingReview` = awaiting judgement (feeds `/review`); `Staging` = optional human review. Validated by the curator golden-set F1 gate (orthogonal to the status flip — it measures section routing) plus a mapping parity test.
- **`/health` live metric wiring**: the previously stubbed `sqlite_wal_size_bytes` and `queue_depth` are now real — WAL size read from `AppState.wal_path` (`<index.db>-wal`, set by `with_search_path`) and queue depth derived from `count_jobs_by_status` (`Pending`). `queue_oldest_age_secs` stays 0 (deferred — no dedicated `QueueStore` method).

### Fixed

- **Locus preserved on re-upsert**: `upsert_note` now guards `locus` with `CASE WHEN notes.content_hash IS NOT excluded.content_hash THEN excluded.locus ELSE notes.locus END`. A re-upsert from a stale `.md` (unchanged content hash, as after an index-level `update_note_locus`) no longer clobbers a moved locus; a genuine content change still applies the frontmatter locus. Discriminant = `content_hash`.
- **Review queue tolerates a malformed id**: `list_review_queue` skips a non-ULID id (data anomaly) with a `warn` instead of failing the whole page with a 500; valid rows keep being served.

### Tests

- Workspace tests pass, zero failures; `clippy --workspace --all-targets` clean.
- New coverage: opt-in score breakdown (rrf ranks, omitted/present, MCP schema), curator status-flip parity + worker observable flip, review queue E2E + non-ULID resilience, dashboard aggregate + health re-check, move-locus E2E (success/400/404/422) + `LocusId::parse` unit, locus preservation on re-upsert, studio router (security headers, SPA fallback, missing-bundle 404).

## [0.4.5] — 2026-06-11

Multi-backend-readiness for the index (testability + decoupling), without shipping an
alternative backend yet. No breaking changes; drop-in upgrade from v0.4.4.

### Changed

- **Worker type-erased on `Arc<dyn Index>`**: the worker now depends on the type-erased `Arc<dyn Index>` facade instead of the concrete `Arc<SqliteIndex>`, unifying the composition root with the server. Eight inherent `SqliteIndex` methods used by the worker outside the three storage traits were promoted into `IndexStore` with neutral default implementations: `set_note_trust`, `write_temporal_entry`, `delete_redirect_by_ulid`, `delete_note_from_index`, `list_garbage_older_than`, `get_note_status`, `get_note_section`, `is_note_forgotten`. An alternative backend does not have to implement them to compile; `SqliteIndex` overrides each by delegation. No rusqlite type is exposed in any promoted signature.

### Added

- **Backend-agnostic index parity suite**: new test-only crate `index-parity-tests` locking the observable contract of the `Index` trait (`DocumentStore` + `IndexStore` + `VectorStore` facade). A `make_index() -> Arc<dyn Index>` factory selects the backend via the `GRADATUM_INDEX_BACKEND` env var (default `sqlite` in-memory) — adding a backend is one match arm + one CI matrix entry, zero duplicated tests. 24 tests across 7 invariant families: write→read round-trip + content hash, FTS + semantic cosine (descending order, downgraded exclusion), status state machine / decay, temporal_index idempotence, dynamic-trust preservation on re-upsert, lesson recall, forget lifecycle. New split CI job `index-backends` (matrix `[sqlite]`) on Forgejo + GitHub.

### Fixed

- **Purge tolerates an unreadable status**: `handle_purge` no longer aborts the whole batch when `get_note_status` fails to parse a candidate's status (e.g. an out-of-enum `'downgraded'` value appearing mid-loop). The offending note is counted ignored + logged (`warn`) and the batch continues purging the other Garbage notes, consistent with the per-note TOCTOU re-check intent.

### Tests

- Workspace tests pass, zero failures; `clippy --workspace --all-targets` clean.
- `index-parity-tests` runs against the sqlite backend via the factory; the `index-backends` CI matrix is extensible to alternative backends.

## [0.4.4] — 2026-06-11

Adds semantic distillation jobs, trust-decay scoring, a consumable event-log, and lesson
recall. No breaking changes; drop-in upgrade from v0.4.3.

### Added

- **Semantic distillation**: new `Distill` job (`DistillSource`, mode `Semantic` only) that clusters non-processed notes of a scope by embedding cosine similarity (threshold 0.75, batch capped at 500) and writes one synthesis note per cluster in `pending-review` with `provenance: "distilled"` and `derived-from` links; source notes are marked `processed` / `derived-into` via copy-on-write-safe extra fields (no parasite versions). Dry-run is the default; the cron schedule is documented but never enabled by default; vault-wide scope is refused outside dry-run. New `TRUST_SCORES["distilled"] = 0.60`.
- **Trust-decay scoring**: `composite_score` gains an optional trust-decay multiplier applied at the RRF layer only (never BM25), with per-provenance half-lives configurable (default `distilled` 90 days; `human-decision` no decay). Global flag `trust_decay_enabled` (default **on**; can be disabled) makes scores bit-identical to v0.4.3 when disabled. Modifier order documented: forgotten (short-circuit) > downgraded > [RRF × recency × pagerank × trust_decay]. Gated behind a search golden-set non-regression check.
- **Consumable event-log**: engines emit a semantic `agent_id` and a `feature_id` derived from request type (`embed` vs `chat`); the event-log store gains transactional reader methods (`fetch_pending`, `mark_processed`) for the distillation pipeline.
- **Lesson recall**: new `GET /api/v1/lessons/recall?class=<x>&limit=<n>` endpoint — BM25-only (no LLM) over the `lessons-learned` section, filtered to a controlled vocabulary of 12 classes (`400` otherwise), excluding lessons tagged `codified` and forgotten notes; returns `{items:[{ulid, title, snippet, tags, anchor_ms}]}` with a sub-50ms target. Also exposed as the `vault_lessons_recall` MCP tool.
- **Migration 0014**: adds a nullable `outcome` column to `event_log` (additive, safe default).

### Fixed

- **Section filter on the semantic search path** in `vault_search`: when a `section` is requested, semantic-only hits from other sections no longer leak through the RRF fusion (they were previously filtered on the BM25 path only). The semantic hits are filtered by section before fusion; on a batch lookup failure the search degrades to BM25-only rather than risk a section leak.
- **`section` parameter forwarding** confirmed end-to-end through the MCP stub for `vault_search` (the leak was a server-side fusion issue, not a stub forwarding gap).

### Tests

- Workspace tests pass, zero failures; `clippy --all-targets` clean.
- Migration 0014 is covered by automated application and idempotence tests.

## [0.4.3] — 2026-06-10

Semantic forget, note lifecycle state machine, configurable history retention, search scoping, and multimodal content support. No breaking changes; drop-in upgrade from v0.4.2.

### Added

- **Semantic forget** (`vault_forget`): mark notes as forgotten so their search relevance decays progressively (half-life of one day) — notes are **not deleted**; physical removal remains a separate, explicit purge concern. Two-step protocol: `POST /api/v1/vault_forget` with `dry_run: true` (default) returns a preview listing the exact note ULIDs; execution requires `dry_run: false` plus `confirm_ulids` matching that preview exactly (any mismatch → `400` with an explicit error body). Scopes: `topic` (full-text query), `locus` (path prefix), `agent` (author). Protected sections (`agent-issues`, `council`) are always excluded and reported in the preview. Companion endpoints: `GET /api/v1/vault/forgotten` (paginated listing with a global total) and `POST /api/v1/vault/unforgot/{ulid}` (restore). Also available as the `vault_forget` MCP tool and the `gradatum-admin vault forget` CLI subcommand.
- **Note lifecycle state machine**: notes transition between six states (`draft`, `staging`, `pending-review`, `live`, `deprecated`, `garbage`) with validated transitions. `PATCH /api/v1/notes/{id}` with a `status` field returns `409 Conflict` on an invalid transition and `204 No Content` on success (idempotent when the target equals the current state). Each transition is recorded in the note's copy-on-write history.
- **Configurable history retention**: the per-note history cap is no longer hardcoded. New `[history]` config section with `max_versions` (default 50, minimum 1 enforced) and `ttl_days` (optional; unset means no age-based expiry). TTL pruning runs before the count cap, on the write path.
- **Purge job for garbage notes**: `Purge` job deletes notes in the `garbage` state older than a grace period (default 30 days, based on the last status change), including their history and redirect entries. Dry-run is the default and lists affected ULIDs without mutating anything. No purge schedule is enabled by default; activation is an explicit operator decision.
- **Search scoping**: `vault_search` accepts optional `locus` (path-prefix filter, LIKE metacharacters escaped) and `vault_id` (read-only cross-vault scoping). Both filters apply to the full-text and the semantic search paths. Omitting them preserves the previous behaviour exactly.
- **Multimodal content support in the gateway**: `POST /v1/chat/completions` accepts the OpenAI content-array format (text and `image_url` parts, base64 data URIs). Requests containing images are only routed to aliases declared `vision_capable = true` (otherwise `400`); when the vision provider is down, the request fails with `503` instead of silently falling back to a text-only model.
- **Classifier prompt v2**: the curator classification prompt now covers all 11 canonical sections (adding `council`) with refined disambiguation criteria, and an explicit caller-provided section hint is honored when valid.
- **Migration 0012**: adds `forgotten`, `forgotten_at`, `forgotten_by`, and `orphaned` columns to the notes index (additive, safe defaults).
- **Migration 0013**: creates the derived `temporal_index` table (per-note temporal anchor and document kind) with an automatic backfill. Foundation only — no query surface in this release.

### Changed

- **Explicit section hints are authoritative**: when a `vault_write` request provides a `section_hint` matching one of the 11 canonical sections, the note is classified to that section directly (the heuristic and LLM classifier are bypassed). Invalid hints are ignored with a warning and classification proceeds as before.
- **`UnforgotResponse.status`** is documented and returned as `"restored"`.

### Fixed

- **FTS5 query sanitization** in the forget `topic` scope: queries containing hyphens or dates (e.g. `2026-06-10`) no longer fail with an FTS5 syntax error; user-supplied terms are quoted as literals.
- **Double LIKE-escaping** of the `locus` filter: the prefix filter is now escaped exactly once, so prefixes containing `%`, `_`, or `\` match literally and correctly.
- **Forgotten-note decay applied to every search path** (scored, filtered, snippet, and semantic), with results re-sorted after the decay is applied. A note that is both forgotten and downgraded receives the forgotten decay only (no penalty stacking).
- **docs.rs build fix for `gradatum-curator`**: the classifier prompt was referenced via `include_str!` with a path that escaped the crate root, causing docs.rs builds of `gradatum-curator` to fail since v0.4.1. The prompt is now packaged inside the crate (`crates/gradatum-curator/prompts/`). Same fix applied to `gradatum-admin` presets and `gradatum-acl-policy` test fixture.

### Security

- `SECURITY.md` now declares the `forgotten_by` field (free-form actor identifier, stored in the index, the note frontmatter, and API responses — treat as potentially containing PII), the configurable history retention, and the fact that `vision_capable = true` routes base64 image content to the configured backend. See `SECURITY.md` for details.

### Tests

- Workspace: 1407 tests pass, zero failures; `clippy --all-targets` clean.
- Both migrations are covered by automated application and idempotence tests; operators are advised to keep the standard pre-migration backup enabled.

## [0.4.1] — 2026-06-06

Quality and reliability improvements across security, documentation, and correctness. No new features; drop-in upgrade from v0.4.0.

### Fixed

- **Unimplemented endpoints**: MCP endpoints not yet implemented now return `501 Not Implemented` with a descriptive message instead of silently enqueuing jobs that never complete.
- **Trait default panics**: default implementations of storage-trait methods now return a typed error instead of panicking, making partial backend implementations safe at runtime.
- **Token revocation**: API token revocation is now checked on every request, not only at issuance. Revoked tokens are rejected immediately.
- **Embedding endpoint defaults**: the default embedding endpoint URL was incorrect and caused connection failures on fresh deployments; corrected to match the documented value.
- **Queue transition atomicity**: job state transitions are now performed atomically, preventing duplicate processing under concurrent workers.
- **SQLite lock contention**: the SQLite write lock is released before vector computation begins, eliminating a potential stall under concurrent search and write workloads.

### Security

- **API key entropy**: API keys are now 256-bit (32 bytes), replacing the previous 128-bit keys.
- **Secret file permissions**: secret files are written atomically with `0600` permissions, removing a window where the file was briefly readable by other processes before chmod.
- **History retention bound**: note history is capped to a fixed maximum (50 versions per note) to prevent unbounded disk usage over time; older versions are pruned automatically.
- **Privacy posture documented**: `SECURITY.md` now describes what data is stored locally, what may leave the host, and the absence of at-rest encryption.
- **Incorrect encryption claim corrected**: documentation previously stated that gateway body logging was encrypted at rest; this claim was inaccurate and has been removed.

### Documentation

- **docs.rs coverage**: all public items across the workspace now carry accurate doc-comments. Internal implementation details, broken links, and references that were not meaningful to library users have been removed or corrected.

## [0.4.0] — 2026-06-06

Vault durable writes — note history, optimistic locking, stable wikilinks, write provenance.

### Added

- **Provenance Trust** : `provenance` field (String) in note frontmatter ; `trust` column in index.db (confidence score 0.0–1.0). Presets: human-decision 0.95, qa-event 0.75, agent-log 0.50, web-scraped 0.35. Stored for use in scoring; trust decay scoring planned for v0.4.1.
- **Stable Wikilinks** : `redirect_table` maps old titles to new ULIDs. `vault_read` resolves title-based lookups via `IndexStore::resolve_redirect()`. CLI support: `gradatum-admin vault rename --old-title <T> --new-ulid <U>`.
- **Optimistic Locking** : `vault_write` accepts optional `expected_sha256: Option<String>` parameter. Conflict detection on worker (non-blocking). Job result includes `JobStatus::Conflict`; client polls via `/jobs/{id}`. Backward-compatible: omitting `expected_sha256` means unconditional write.
- **Note History** : Copy-on-Write `.history/<ulid>/<timestamp>.md` on delta detection (excludes transient fields). New MCP endpoints: `vault_history(note_id)`, `vault_history_get(note_id, timestamp)`, `vault_restore(note_id, timestamp)`, `vault_diff(note_id, t1, t2)`. History retention policy planned for v0.4.2.
- **Migration 0010** : adds `provenance TEXT` and `trust REAL DEFAULT 0.5` columns to notes table; creates `redirect_table(source_title TEXT UNIQUE, target_ulid TEXT REFERENCES notes(id))`.
- **Migration safety** : pre-migration backup script included in systemd units (ExecStartPre hook). Archives vault, queue, and audit DBs to `.tar.zst` with 7-day retention. See docs/DEPLOYMENT.md for configuration.

### Changed

- **Storage traits** : `DocumentStore`, `IndexStore`, `VectorStore` traits finalized for multi-backend support. No breaking API changes; dispatch overhead negligible.

### Fixed

- **Provenance backfill** : migration 0010 sets `provenance='agent-log'` for all existing notes lacking provenance (idempotent).

### Tests

- **Workspace** : 1178 tests PASS, 0 clippy warnings, 0 regressions.
- **Known limitations**: history pruning policy and trust-decay scoring are deferred to later releases.

## [0.3.7] — 2026-06-05

Reliability fixes: search/read/write consistency and wikilink stability.

### Fixed
- **vault_write/worker** : fixed ULID mismatch between enqueued note ID and persisted note ID. `write_note_with_id()` ensures write-time ID == stored ID.
- **vault_read** : now accepts `<section>/<ulid>` format returned by `vault_search` (round-trip consistency). ULID and title lookups remain supported.

### Changed
- **vault_search** : score documentation clarified. Score is a composite RRF rank, not a [0–1] similarity value.

## [0.3.6] — 2026-06-05

Add per-crate README.md for crates.io documentation pages; metadata only, no code changes.

### Added

- `README.md` for all 26 publishable crates in the workspace (one file per crate,
  co-located with `Cargo.toml`). Each README reflects the actual v0.3.x implementation:
  role, API surface, feature flags, and usage example where applicable.

## [0.3.5] — 2026-06-03

Enriches `title` and `section` fields for semantic-only hits in `vault_search`.

### Fixed

- **`vault_search`: `title = null`, `section = ""` for semantic-only hits**: after RRF
  fusion, notes present only in the semantic signal (absent from `bm25_map`) retained
  `title = null` and `section = ""` in the final response. A batch enrichment pass
  (`get_titles_sections` — single `SELECT … WHERE id IN (…)`) now fetches `title` and
  `section` from the `notes` table for all missing hits, just before `SearchHit`
  construction. Existing BM25 enrichments are not overwritten. `snippet` remains `None`
  for semantic-only hits: no FTS5 match is available to generate a localized excerpt.

### Added

- **`IndexStore::get_titles_sections`**: new batch helper on the `gradatum-core::IndexStore`
  trait — `SELECT id, title, section FROM notes WHERE vault_id = ? AND id IN (…)` — used
  by the enrichment pass above. Implemented in
  `gradatum-index::SqliteIndex::get_titles_sections`.

## [0.3.4] — 2026-06-03

Fix `vault_search` returning `title: null` for all notes written before this
version.  The `notes.title` column (added in 0.3.0 / migration 0005) was never
populated at write time; migration 0009 backfills the existing corpus by
extracting the first Markdown H1.

### Fixed

- **`notes.title` always null at write-path**: `handle_curate` now calls
  `upsert_note_title` after every successful `vault.write_note`, resolving the
  title from the explicit `spec.title` field (API payload) or, as a fallback,
  from the first `# H1` line of the note body via `extract_h1_title`. The call
  is non-fatal: a failure is logged as `WARN` and does not roll back the write.
- **Migration 0009 — backfill `notes.title` for existing corpus**: applies an
  `UPDATE notes SET title = TRIM(SUBSTR(body_text, 3, …))` for rows where
  `title IS NULL OR title = ''` and `body_text LIKE '# %'`, extracting the H1
  header. Idempotent (guard on `title IS NULL OR title = ''`). Does not overwrite
  already-set titles.

### Known limitations

- Notes written before v0.3.4 whose body does not start with a Markdown H1 will
  retain `title = NULL` after the backfill (~895/911 notes in the reference
  deployment). These titles are not recoverable from the current schema; future
  writes for those notes will populate the column correctly going forward.
- The `classify` / `reclassify` worker path does not yet call
  `upsert_note_title` (annotated with `NOTE v0.3.4` in `dispatch.rs`). Notes
  updated exclusively through reclassification will have their title populated on
  the next normal curate cycle.

## [0.3.3] — 2026-06-02

Reliability patch: fix the multi-worker queue deadlock that starved one job
kind (the actual cause behind the worker stall; 0.3.1/0.3.2 fixed adjacent
issues but not this).

### Fixed

- **Multi-worker dequeue deadlock**: `dequeue`/`dequeue_by_kind` ran a
  `SELECT … FOR lease` then `UPDATE` inside a `BEGIN DEFERRED` transaction, so
  the read lock had to upgrade to a write lock. Under concurrent workers two
  dequeues deadlocked on the upgrade (`SQLITE_BUSY`), starving one kind (e.g.
  embeddings stayed `Pending` indefinitely while curation drained). The two
  dequeue sites now use `BEGIN IMMEDIATE`, acquiring the write lock up front so
  dequeues serialize without deadlock. Covered by a multi-kind concurrency
  regression test (10 curate + 30 embed → all drained in parallel).

## [0.3.2] — 2026-06-02

Reliability patch: fix the worker stopping after draining a batch (the actual
root cause of the intermittent stall that 0.3.1 did not fix).

### Fixed

- **Worker stops after batch drain**: the custom Apalis backend fetcher had an
  internal `loop {}` that never yielded to the worker on an empty queue, so
  under the concurrency gate a wakeup was lost when the queue drained — the
  worker stopped and the Monitor shut down, leaving new jobs unprocessed until a
  restart. The fetcher now follows the canonical Apalis pattern (one poll = one
  dequeue, yields `Ok(None)` on empty), so the worker keeps polling. Covered by
  a regression test that drives a real Monitor (drain → re-enqueue → processed).

## [0.3.1] — 2026-06-02

Reliability patch: eliminate an intermittent worker hang on `vault_write`.

### Fixed

- **Worker hang under SQLite contention**: job acks (`fail`/`complete`) returned
  `SQLITE_BUSY` immediately and failed silently, leaving jobs stuck `Running`
  until lease expiry, then re-dequeued and re-wedged. Added `busy_timeout(5s)` +
  WAL on all sqlx pools (queue/server/worker) so SQLite retries internally.
- **DLQ guard infinite loop**: `promote_retries` read the retry counter from a
  stale serialized blob (always 0) instead of the SQL `attempt_count`; the guard
  now reads SQL so jobs terminate to DLQ at max retries.
- **DLQ replay no-op**: `jobs dlq --replay` now resets `attempt_count` so
  replayed jobs get fresh retries.

### Changed

- `gradatum-worker.service` reads an optional `EnvironmentFile` for `RUST_LOG`
  (worker observability).

## [0.3.0] — 2026-06-02

Storage trait decomposition, event-log sink, gateway cost-attribution, cognitive kind capture, and secrets dependency injection.

> **Breaking change (deploy)**: JWT signing key is now persisted. First deploy of v0.3.0 invalidates all existing JWTs. Consumers must re-exchange API keys for new JWTs after deploy.

### Added

- **Storage trait decomposition**: monolithic `trait Index` decomposed into three granular traits in `gradatum-core` — `DocumentStore` (note CRUD), `IndexStore` (FTS5, scoring, wikilinks), `VectorStore` (embedding + ANN). `trait Index` facade with blanket impl preserves call site compatibility. `AppState.search` uses vtable dispatch (`Arc<dyn Index>`). Types `SearchHitRaw`, `AuthorRow`, `Lineage` made public.
- **Event-log sink**: dedicated SQLite table `event_log` (migrations 0006/0007) — append-only, outside notes/notes_fts. Endpoint `POST /api/v1/event-log` with timestamp/payload bounds, log-injection sanitization. `EventLogStore` with `insert_batch` / `purge` / `count`. Retention policy: 30-day TTL, 6-hour purge interval, 5M-row cap. Prometheus metric included.
- **gradatum-gateway crate**: autonomous LLM proxy service (`:8436`). Routes: `/v1/chat/completions` (+SSE), `/v1/embeddings`, `/v1/rerank` (ONNX cross-encoder), `/v1/models`, `/health`, `/metrics`. Replaces standalone LLM services.
- **Cost attribution**: `QaEvent` enriched with feature_id, model_used (fallback-aware), tokens_input/output, cost_usd. Streaming paths omit token counts.
- **Cognitive kind capture** (migration 0008): columns `c_kind` (CoALA categories: episodic / semantic / procedural / reflective) and `doc_kind` (Event / Static) added to `notes`. Derived deterministically from `section` via const functions in `gradatum-core`. Zero LLM runtime cost. `section` remains authoritative; `c_kind`/`doc_kind` are derived metadata. Scoring unchanged (doc_kind usage deferred).
- **Secrets dependency injection**: `SecretsProvider` trait + `SecretBytes` (crate `secrecy`, Drop-zeroize, Debug masked) + `EnvSecretsProvider` + `FileSecretsProvider` in `gradatum-core/src/secrets.rs`. File secrets provider refuses overly-permissive permissions at load time.

### Changed

- **Workspace**: 26 → 28 crates (added `gradatum-gateway` + `gradatum-db-sqlite` promoted).
- **AppState.search** : switched to vtable dispatch for Index trait (`Arc<dyn Index>`), enabling future multi-backend support without recompilation.
- **Job dequeue filter** : fixed `dequeue_by_kind` to enforce strict `kind` matching. Previously, a Curate job could be processed by the wrong worker type, causing note loss.

### Fixed

- **JWT signing key persistence** : key was ephemeral (regenerated per boot). Now persisted to disk via `FileSecretsProvider` (mode 0600). See breaking change note above.
- **Job dequeue routing** : fixed `WHERE kind = ?` filter to prevent job type mismatches.

### Security

- **Secrets hardening**: `FileSecretsProvider` enforces file mode 0600 and directory mode 0700 at `O_CREAT` (zero world-readable window). Seed zeroize on drop via `secrecy`. Path-traversal guard on secret file paths. Warning logged on permissive permissions.
- **Event-log endpoint hardening**: timestamp bounds (400 on out-of-range), field bounds (422 on oversized payloads), `DefaultBodyLimit`, log-injection sanitize on string fields.
- **Secrets DI**: eliminates inline secret literals; secret material flows exclusively through `SecretsProvider` trait implementations with memory-zeroing guarantees.

### Tests

- Workspace: **1088 PASS** (up from 886 v0.2.0 baseline, +202 new across 5 tranches).
- **0 FAILED** across 28-crate workspace.
- Golden search regression: **3/3 diff-zero** maintained across all tranches.
- `clippy --all-targets`: 0 warnings maintained.
- Security review findings: all HIGH severity findings resolved.

## [0.2.0] — 2026-05-29

Apalis job infrastructure, Dead-Letter Queue, jobs introspection API with SSE, and Prometheus observability.

### Added

- **Apalis job infrastructure**: 22 Job variants (`JobKind` enum) covering curator and maintenance flows. `JobRecord` 5-block structure with forward-compatible fields. Custom `GradatumQueue` facade over Apalis `Backend`. `SqliteQueueStore` with atomic lease semantics. Framework-agnostic: future swap to Redis/RabbitMQ/Postgres needs only a new `QueueStore` impl.
- **Dead-Letter Queue + Monitor**: automatic DLQ routing for jobs exceeding max retries. Apalis Monitor for multi-worker coordination with timeout, retry, panic isolation, and load shedding layers. Graceful shutdown with 30s drain.
- **Jobs introspection API**: five HTTP endpoints for job lifecycle (enqueue, status, stream, cancel) + Prometheus metrics. Server-Sent Events for streaming. Idempotency-Key header support. `gradatum-admin jobs` CLI commands for inspection and control.
- **Prometheus exporter**: `:19091` pull endpoint, disabled by default (`metrics_enabled = true` in config to enable). Per-job-kind metrics.
- **`gradatum-db-sqlite` crate (new)**: isolates SQLite queue implementation — 15 methods, WAL mode, index on `(vault_id, job_kind, status)`.

### Fixed

- **`SqliteQueueStore::get()` stale payload**: record lifecycle fields (`started_at`, `completed_at`, `duration_ms`) were desynchronised from authoritative SQL columns. Fixed by syncing from SQL in `get()`.
- **`duration_ms` stub**: `JobResult.duration_ms` was hardcoded 0. Now measured via `std::time::Instant` injected in `record_to_task` and recovered in `GradatumAcknowledger::ack()`.
- **Apalis ack/complete wiring**: `apalis::Backend::ack`/`complete` now properly wired via `GradatumAcknowledger`.
- **`enable_tracing` panic**: `enable_tracing` re-enabled; `TaskId` injection in `record_to_task` resolves a panic in `make_span`.

### Tests

- 886 PASS / 0 failed.
- E2E integration: write note → curator job enqueued → Monitor processes → metric exported → SSE subscribers notified.

## [0.1.0-alpha.15] — 2026-05-28

### Security

- **LIKE wildcard escaping in `title_lookup`**: SQL wildcards (`%` and `_`) in note titles
  are now escaped via `escape_like_pattern` + SQLite `ESCAPE '\\'`, eliminating false-positive
  LIKE matches in `vault_read`, `vault_trace`, and classify.

### Performance

- **`vault_trace` parallel seed resolution**: seed entries are now resolved concurrently via
  `tokio::JoinSet`, eliminating the sequential N×seed round-trip.
- **Wikilink `title_lookup` parallel resolution**: wikilink resolution in the worker now uses
  `tokio::JoinSet` instead of a sequential `.await` loop.
- **Reranker single-pass tokenization**: `encode_batch` pre-tokenizes in one pass.

### Changed

- **`vault_classify` LLM cascade**: `vault_classify` now invokes the LLM curator in cascade
  with category normalization, fallback on curator error, and status propagation.

### Added

- **`gradatum-admin backfill-titles`**: new CLI subcommand that populates the `title` column
  for notes where it is null, extracting the value from the note body.

### Removed

- **`X-Gradatum-Wait` header**: the stub `X-Gradatum-Wait` header and `sync_wait` logic have
  been removed from `gradatum-server` — the server is async-only.

### Tests

- 826 PASS / 0 regressions; `cargo deny` GREEN; 0 clippy warnings.
- LIKE injection prevention and rate limiting confirmed by integration tests.

## [0.1.0-alpha.14] — 2026-05-28

Security hardening and CI release infrastructure.

### Security

- **JWT not-before validation** : explicitly enabled `validation.validate_nbf = true` in `crates/gradatum-auth/src/jwt.rs`. Default behavior in jsonwebtoken v9 skips this check, silently accepting future-dated tokens.

### Infrastructure

- **CI actions pinning** : pinned artifact actions to v3 for Forgejo Actions compatibility. Docker build job disabled pending docker-capable runner provisioning.

### Tests

- 13 tests pass; `cargo build --release` passes; no regressions.

## [0.1.0-alpha.13] — 2026-05-10

Endpoints completeness: wikilinks, title lookup, vault trace, and context budget support.

### Added

- **Wikilinks post-curate** : `process_wikilinks_b5` parses `[[wikilinks]]` and inserts edges into `note_links`.
- **Title lookup in vault_read** : `vault_read` now accepts both ULID and title lookups via `find_note_by_title()`.
- **vault_trace multi-mode** : supports ULID lookup (lineage), title lookup, and full-text query (FTS5 multi-match + aggregated lineage).
- **vault_context token budget** : `vault_context` enforces token budget via heuristic `chars/3.0` (UTF-8 safe). Returns top-10 notes under budget.

### Tests

- 779 → 796 PASS workspace (+17). 0 clippy, 0 fmt, `cargo deny` GREEN.
- Smoke E2E: auth exchange, health check, write→curate→read+trace+context integration.

### Changed

- Install script renamed to `install-gradatum-services.sh`; `install-gradatum-stub-mcp.sh`
  added.

## [0.1.0-alpha.12-bumps.1] — 2026-05-10

Dependency upgrades: supply chain hardening (5 sequential PRs).

### Changed

- **serde_yml** : upgraded to maintained fork (0.0.12) post-deprecation of upstream `serde_yaml`.
- **MCP protocol** : upgraded `rmcp` 0.x → 1.x and `schemars` 1.x (stabilisation).
- **HTTP stack** : upgraded `axum`, `tower-http`, `reqwest` with adapter updates for breaking changes.
- **Cryptography** : upgraded `sha2` 0.10 → 0.11, `governor`, `nix`, and 12 minor dependencies.
- **TOML** : upgraded `toml` 1.x suite with MSRV bump to 1.85 and clippy fixes.

### Deferred

- **rusqlite upgrade** : deferred until `sqlx 0.9` stable (linking conflict with `sqlx 0.8.6`).

### Tests

- 779 PASS / 0 clippy / 0 fmt / `cargo deny` GREEN on each merge.

## [0.1.0-alpha.12] — 2026-05-10

Multi-factor scoring and cross-encoder reranker integration.

### Added

- **Multi-factor scoring** : recency and PageRank factors combined via composite scoring (`composite_score = rrf × (1 + α·recency) × (1 + β·pagerank)` with α=0.2, β=0.1).
- **Backlinks queries** : `get_indegree()` and `get_note_created_and_indegree()` for lineage scoring.
- **Reranker trait** : pluggable trait with `NoopReranker` (default) and `OnnxCrossEncoderReranker` (feature-gated `onnx-reranker`).

### Fixed

- **ONNX tensor API** : adapted to `ort 2.0.0-rc.9` API (tuple-based shape + extraction).

### Tests

- 754 → 779 PASS workspace (+25). 0 clippy, 0 fmt.

### Known limitations

- The reranker model path is not yet configurable via environment variable;
  `NoopReranker` is the default.

## [0.1.0-alpha.11-patch.1] — 2026-05-10

Design foundations: SearchHit enrichment and error propagation.

### Added

- **SearchHit title enrichment** : `SearchHit.title` field populated from RrfHit, eliminating need for round-trip `vault_read` calls.
- **Inference error handling** : `GradatumError::Inference` variant for clean error propagation from embed/rerank layers.

### Coverage

- **RRF handler integration** : 4 new E2E tests for RRF fusion path, graceful degradation, and error handling.

### Changed

- Resolved pre-existing clippy warnings in `search_semantic.rs`.

### Tests

- 740 → 754 PASS workspace (+14). 0 clippy, 0 fmt.

## [0.1.0-alpha.10] — 2026-05-10

Vault API completeness: status reporting, pagination, title tracking, and section filtering.

### Fixed
- **vault_status** : `note_count` now returns accurate `COUNT(*) WHERE status='live'`. `total_size_bytes` returns accurate byte sum.
- **vault_search** : section filtering now applied via conditional `WHERE n.section = ?`.

### Added
- **vault_list pagination** : cursor-based pagination via `list_notes()` with lexicographic ULID ordering.
- **Note titles** : migration `0005_add_title_column` adds `title` column. `extract_h1_title()` extracts from body. `upsert_note_title()` keeps in sync.
- **FTS5 snippets** : native FTS5 snippet extraction localizes relevant passages instead of truncating.

### Tests
- 698 → 720 PASS workspace (+22). Unit tests for status, title, section filtering, snippets, and pagination.

## [0.1.0-alpha.9] — 2026-05-09

### Added

- **`vault_downgrade` endpoint**: parity with the legacy vault MCP.
  - Migration `0004_vault_downgrade.sql`: adds `replaced_by TEXT REFERENCES notes(id)` column
    and a partial index `idx_notes_status_downgrade WHERE status='downgraded'`.
  - DTO: `VaultDowngradeRequest/Response`, `NoteStatusPatch`, and
    `VaultSearchRequest.include_downgraded` extension (default `false`).
  - Endpoints: `POST /api/v1/vault_downgrade` (synchronous 200) and `PATCH /api/v1/notes/:id`
    (status patch, 204).
  - SQL helpers: `SqliteIndex::downgrade_note(id, reason, replaced_by)` and
    `patch_note_status(id, status?, reason?, replaced_by?)` (idempotent UPDATE; 404 if not
    found).
- **Downgraded-note search filter**:
  - `vault_search` excludes `status='downgraded'` by default.
  - `include_downgraded=true` penalizes the BM25 score (approximately 10% relative relevance).
  - `Index::search_fts_scored` trait gains `include_downgraded: bool` parameter; return type
    extended to `Vec<(NoteId, f64, String_status)>`.
- **MCP tool `vault_downgrade`**: thin proxy for drop-in compatibility with the legacy vault.
- **`gradatum-admin downgrade-from-legacy-vault-trash`**: imports `.vault-trash/<date>/*.md`
  files from the legacy vault into gradatum (idempotent, `--dry-run`, `--limit`).

### Changed

- `vault_downgrade` endpoint changed from asynchronous (202) to synchronous (200).
- Field name `replaced_by` aligned across DTO, SQL, and handlers.

### Tests

- 668 PASS / 0 failed (+29).

## [0.1.0-alpha.8-patch.1] — 2026-05-09

### Fixed

- **Missing `[embed]` section in generated `server.toml`**: the `[embed]` configuration
  section was absent from the template, causing all embedding jobs to silently skip
  (`enabled=None` → embedder resolved to `None` → `process_embed_note` early-returned without
  an HTTP call). Added `[embed]` section with explicit defaults (`enabled=true`,
  `timeout_ms=5000`).
- **Embed model/dimension defaults corrected**: `EmbedConfig::default()` and the `[embed]`
  template updated from `bge-small-en-v1.5` / 384 dimensions to `bge-m3-Q8_0` / 1024
  dimensions.

### Tests

- Regression tests added: `merge_adds_embed_section_when_backup_lacks_it`,
  `embed_defaults_match_documented_values`.

## [0.1.0-alpha.8] — 2026-05-09

### Added

- **`gradatum-warden` crate**: perimeter defense layer — CIDR IP filter, per-IP token-bucket
  rate limiting, loopback bypass. Public API: `WardenLayer`, `WardenConfig`, `WardenError`,
  `WardenDecision`. Advanced features (audit, GeoIP, hot-reload) deferred to a future release.
- **Rate limiting** on `/api/v1/*` and `/auth/exchange` (exempt: `/health`, `/metrics`).
  Default: 60 req/min, burst 10, `exempt_localhost` configurable. Returns `429` +
  `Retry-After`. Config: `[ratelimit]` block in `server.toml`.
- **Optional auth on `GET /api/v1/jobs/:id`**: new `[auth].require_jwt_jobs_endpoint` flag
  (default `false`). When `true`, a Bearer JWT is required.
- **Asynchronous embedding pipeline**: after note curation, an `embed_note` job is
  automatically chained. The worker fetches embeddings from the configured HTTP endpoint and
  stores them in `note_embeddings` (UPSERT, f32 LE). Config: `[embed]` block in `server.toml`
  (default: `localhost:8431`, model `bge-small-en-v1.5`, 384 dimensions, 5 s timeout).
- **`gradatum-admin backfill-embeddings`**: CLI subcommand that scans notes without embeddings
  and enqueues `embed_note` jobs idempotently. Args: `--root`, `--tenant`, `--limit`.
- `SqliteIndex::insert_note_embedding` and `get_note_embedding` helpers (UPSERT, f32 LE,
  validates `vector.len() == dim`).
- `EmbedConfig` and `RateLimitConfig` added to `ServerConfig`.

### Changed

- **Loopback bypass fix**: loopback clients (`:19090`) now receive the real handler response
  instead of `Body::empty`. Fixed via `WardenService::call` early-return `inner.call(req)`.
- **`gradatum-embed`**: `fastembed-cpu` feature is not enabled by default; the HTTP backend is
  the default (no ORT dependency required).
- **Ingestion pipeline**: `embed_note` is non-blocking — the note is persisted to the vault
  and FTS5 index before the embedding job is chained (best-effort).

### Removed

- `tower_governor 0.5` dependency: its `error_handler` terminated the middleware chain with an
  empty body, incompatible with the loopback bypass.

### Tests

- 636 PASS / 0 failed (+45).

## [0.1.0-alpha.7-patch.6] — 2026-05-08

### Fixed

- **Worker leadership lease not released on clean shutdown**: `gradatum-worker` received
  SIGTERM and logged a clean shutdown but did not delete its row in the `worker_leadership`
  table. Consequence: a rapid stop+start left the next worker retrying ~60–75 s (4 × 15 s)
  before taking over after TTL expiry.

### Changed

- `LeaderElection::release()` added in `leader.rs`: issues a `DELETE WHERE holder = ?`
  (race-safe — does not touch a lease held by another worker). Called from `main.rs` after
  `renewal.abort()` on clean shutdown (best-effort; errors are logged, not fatal).
- Stop+start takeover latency: **~60–75 s → < 1 s**.

### Added

- 4 integration tests in `leadership_cleanup.rs`: `release_removes_own_row`,
  `release_is_idempotent`, `release_only_self_not_other_holder`,
  `release_without_acquire_is_noop`.

## [0.1.0-alpha.7-patch.5] — 2026-05-08

### Fixed

- **`gradatum-worker` stays `inactive` after rapid stop+start**: `Restart=on-failure` in
  `gradatum-worker.service` did not cover a legitimate exit 0 ("not leader") when another
  worker still held the lease. Without automatic restart, the service stayed `inactive (dead)`
  until the lease expired naturally (~60 s) and required manual intervention.

### Changed

- `Restart=on-failure` → `Restart=always`, `RestartSec=5s` → `RestartSec=15s` in the systemd
  unit file. Systemd now always restarts; the leadership lease expires naturally (~60 s) and
  takeover is automatic on the next cycle.
- Motivation comments added inline in the unit file.

## [0.1.0-alpha.7-patch.4] — 2026-05-08

### Fixed

- **Structural merge bug in `walk_and_merge`**: `gradatum-admin/src/init.rs` iterated over
  keys from the new template and discarded sections present only in the user backup (e.g.
  `[curator]`, `[curator.llm]`). On re-install, this wiped the live `[curator]` configuration,
  causing `gradatum-worker` to go `inactive (dead)`.

### Changed

- **Merge semantics inverted**: the backup is now authoritative for all user content (custom
  sections, extension sections, customized keys). The new template only augments with:
  - New keys/sections absent from the backup (added with their default values)
  - Default values for keys the backup does not define
- `KEY_MIGRATIONS` renames (`db_path` → `vault_index_path`) applied pre-walk on a copy of the
  backup to maintain consistency.
- Helpers `lookup_item_mut` and `set_item` replaced by `set_item_or_insert` and `remove_path`.
- Merge log now emits a `user_added` counter for sections/keys preserved from the backup.

### Added

- 2 regression tests: `merge_preserves_backup_only_sections_curator` (exact reproducer) and
  `merge_adds_user_only_top_level_section`.
- `set_item_or_insert` — helper that creates intermediate nodes when absent.
- `remove_path` — helper that deletes a key by dotted path.

## [0.1.0-alpha.7-patch.3] — 2026-05-08

### Added

- **Atomic `bearer.toml` backup**: `gradatum-admin init --force` (and
  `install-gradatum-services.sh`) now backs up `bearer.toml` to `.bak.<ISO-TS>` before
  overwriting. Consistent with the `server.toml` backup behaviour from patch.2.
- 2 regression tests: `materialize_preset_backups_existing_bearer_toml` and
  `materialize_preset_no_backup_on_fresh_install`.

### Known limitations

- Manual customisations to `bearer.toml` are overwritten in the active file on `--force`
  re-init but remain recoverable from the backup. Automatic merge support is deferred to a
  future release.

## [0.1.0-alpha.7-patch.2] — 2026-05-08

### Added

- **Schema-directed `server.toml` merge**: `gradatum-admin init --force` no longer blindly
  overwrites. Pattern: atomic backup `.bak.<ISO-TS>` + schema-directed merge. Preserves user
  customisations (`[curator.llm].base_url`, `api_key_env`, `timeout_ms`, `jwt_ttl_*`, etc.);
  adds new keys with defaults; drops legacy keys absent from the new schema.
- **Explicit `KEY_MIGRATIONS` table**: handles cross-version key renames
  (`storage.db_path` → `storage.vault_index_path`).
- 3 regression tests: `merge_preserves_user_curator_customizations`,
  `merge_drops_legacy_db_path_via_rename_migration`, `merge_keeps_new_keys_with_defaults`.
- `toml_edit = "=0.22.27"` workspace dependency (preserves TOML format and comments via
  `DocumentMut`).
- `gradatum-admin` gains a `[lib]` target for integration-test access without the binary.
- `generate_server_toml_template` and `merge_user_config` exposed as `pub`.

## [0.1.0-alpha.7-patch.1] — 2026-05-08

### Fixed

- **`gradatum-admin init` template still used legacy `db_path`**: the generated `server.toml`
  template referenced `db_path` instead of the canonical `vault_index_path`, triggering a
  deprecation WARN on every fresh or forced init. Fixed in `init.rs` with a regression test
  in `init_clean.rs`.

## [0.1.0-alpha.7] — 2026-05-08

### Changed

- **`[storage].db_path` renamed to `[storage].vault_index_path`**: backward-compatible via
  `serde(alias)` — the old name is still accepted but emits a WARN at boot. The alias will be
  removed in a future release.

### Added

- `StorageConfig::legacy_alias_used()` — detects use of the deprecated field name.
- `build_snippet` exposed as `pub(crate)` (deduplication between test and production paths).
- 3 regression tests for UTF-8 ZWJ emoji boundary handling.
- `EXPECTED_TOOL_NAMES` constant in MCP stub tests (dynamic tool count).

## [0.1.0-alpha.6] — 2026-05-08

### Fixed

- **`GET /api/v1/jobs/<id>` now returns real status**: previously always returned `"pending"`;
  now reflects actual transitions `pending` → `leased` → `done`. **Behavioural breaking
  change**: a non-existent id returns `404 Not Found` instead of `200 + pending`.
- **BM25 ranking**: `POST /api/v1/vault_search` now uses native FTS5 `bm25(notes_fts)` instead
  of a positional proxy score. Score normalised to `[0..1]` via `1.0 / (1.0 + bm25.abs())`.
- **Information disclosure**: `last_error` is now mapped to opaque codes
  (`invalid_input` / `vault_error` / `storage_error` / `processing_error`) before being
  returned in the API response, preventing leakage of filesystem paths, internal ULIDs, and
  anyhow error chains.

### Added

- `Queue::get(id) -> Option<JobInfo>` (async trait).
- `SqliteQueue::get` impl with `SELECT ... FROM jobs_v2 WHERE id = ?`.
- `Index::get_note(tenant_id, note_id) -> Option<NoteRecord>` (async trait).
- `Index::search_fts_scored(...) -> Vec<(NoteId, f64)>` (real BM25).
- `SqliteIndex::search_fts_scored` impl with `bm25(notes_fts)`.
- `JobInfo` struct (read job metadata without claiming).
- `JobStatus::as_str` and `from_str` helpers.
- `sanitize_job_error` mapping to opaque codes.
- `NoteRecord` moved to `gradatum-core::index` (portable type).
- 11 regression tests (Queue::get unit, status helpers, sanitize, E2E poll, BM25 ordering).

### Tests

- 566 PASS / 0 failed (+21).

## [0.1.0-alpha.5] — 2026-05-07

### Added

- **Auth via API key and `/auth/exchange`**:
  - `gradatum-acl-auth::ApiKeyStore` trait and `SqliteApiKeyStore` impl; argon2id hashing
    `m=19456 KiB / t=2 / p=1`
  - Migration SQL: `api_keys` table, index, and integrated init
  - CLI commands: `gradatum-admin api-key {create,list,revoke,rotate}` and
    `gradatum-admin token issue`
  - Endpoint `POST /auth/exchange {api_key}` with uniform 401 outside the JWT middleware
  - `SqliteRevocationStore` wired at runtime; checked on every exchange call
- **Mandatory `Claims.tenant_id`** with `TrustContext` propagation through the middleware
  layer
- **11 integration tests** (E2E auth flow + tenant propagation):
  - `auth_e2e_full_flow.rs`: 5 tests (create key → exchange → TTL check)
  - `auth_tenant_propagation.rs`: 6 tests (TrustContext leak + middleware accept/reject)
- `scripts/smoke-alpha-5.sh`: 9-step acceptance smoke + RAM check
- `ExchangeResponse` V2: 5 fields — `token`, `ttl_secs`, `scopes`, `tenant_id`, `kid`
- `AppState::with_acl_preset_path()` wired from `cfg.acl.preset_path`

### Changed

- `ExchangeResponse.expires_in` → `ttl_secs`
- `AuthConfig::default()` `revocation_db_path` and `api_keys_db_path`: absolute paths via
  config instead of auto-derived `None`
- Migration `api_keys.sql`: removed `PRAGMA journal_mode = WAL` (sqlx::migrate runs inside
  an implicit transaction — SQLite rejects the pragma there). WAL now configured via
  `SqliteConnectOptions::journal_mode(Wal)` at connection time, before migrations.
- `queue_path` convention: `<root>/queue.db` → `<root>/db/queue.sqlite` (aligns with the
  `db/` folder layout)
- `gradatum-admin init --preset`: embeds presets via `include_str!` (idempotent on
  re-install); install script `scripts/install-gradatum-services.sh` added

### Fixed

- **NFS build-artifact corruption**: 24 `target/debug/deps/` files corrupted by
  `zstd: stdout: I/O error` after a filesystem availability incident. Cleaned and rebuilt;
  two latent code bugs surfaced and fixed (WAL pragma in migration, absolute
  `AuthConfig` defaults).
- `AclEngine` not loading from `cfg.acl.preset_path` — previously hardcoded to an empty
  preset, causing all vault operations to return 403.

### Tests

- 492 PASS / 0 FAIL / 9 ignored.

### Known limitations

- `JsonlFileSink` audit events are wired with writeable stubs only — full end-to-end audit
  deferred.
- Rate limiting on `/auth/exchange` deferred.
- Granular scopes deferred: flat scopes only (`read`, `write`, `admin`).
- API key auto-rotation deferred.
- ACL filter by `tenant_id` at runtime deferred.
- Worker dispatch and `Vault.read_note` stubs deferred.

### Security

- argon2id: `m=19456 KiB`, `t=2`, `p=1` (OWASP 2023 compliant)
- `OsRng` for secret generation (128 bits effective entropy per key)
- Uniform 401 on `/auth/exchange` (no key enumeration)
- Constant-time argon2id verify (via `argon2` crate)
- API key displayed only once at creation, no re-display
- Revocation store wired at runtime; checked on every exchange call

---

## [0.1.0-alpha.4] — skipped

This version number was reserved but skipped; development proceeded directly to
v0.1.0-alpha.5.

---

## [0.1.0-alpha.3] — 2026-05-05

### Added

- **`gradatum-queue`**: `SqliteQueue` + `Queue` trait; `UPDATE...RETURNING` atomic lease claim.
- **`gradatum-worker`**: leader election via SQLite CAS, dispatcher loop, SIGTERM drain, GC of
  stale leases.
- **3 MCP write handlers** (`vault_write` / `vault_classify` / `vault_downgrade`) with async
  202 response + a job-status poll endpoint.
- **`gradatum-curator`** cascade pipeline — 5 functions:
  - Novelty detection (SHA-256 + MinHash 128)
  - Section routing (regex + Bayesian, 10 sections)
  - Tags (TF-IDF, top 5)
  - Wikilink extraction (Jaro-Winkler 0.88 threshold)
  - Deduplication (cosine 0.95 threshold)
- **5 LLM backends** (protocol-generic):
  `HeuristicBackend` / `OpenAiCompatBackend` / `OllamaCompatBackend` /
  `AnthropicCompatBackend` (ephemeral prompt caching) / `GeminiCompatBackend`
- **`CircuitBreaker<B>`** wrapper: exponential backoff 30→60→120→300 s, `HalfOpen`
  `success_threshold=2`; 7 tests.
- **JSONL audit log**: `HttpAuditEvent` + `JsonlFileSink` with daily rotation, mode 0640,
  content hash (JCS RFC 8785).
- **`gradatum-bench` binary `curator_f1`**: benchmarks curator F1 against a dataset; supports
  `LLM_ENDPOINT` / `LLM_MODEL` env vars.
- **OpenDAL feature gates**: `fs` default + `s3` / `gcs` / `azure` / `all-cloud` opt-in.
- **Systemd packaging**: `gradatum-server.service` (`MemoryMax=512M`) +
  `gradatum-worker.service` (`MemoryMax=1G`, `MemorySwapMax=0`) +
  `sysusers.d/gradatum.conf` (UID 990).
- **TOML curator config**: `[curator] backend = "heuristic"` default + `[curator.llm]`
  opt-in; classifier prompt embedded via `include_str!`.

### Fixed

- `gradatum-curator::routing` regex `\b SECTION \b` broken on `[SECTION]` prefixes — fixed
  with two-layer `PREFIX_PATTERNS` + `KEYWORD_PATTERNS`; 6 tests added.
- `gradatum-bench::curator_f1` raw Markdown body degraded lightweight LLM accuracy — fixed
  with `clean_body_for_llm()` that strips headings, wikilinks, code blocks, and frontmatter.

### Bench results — ALL PASS

Dataset: `gradatum-balanced-v1-final.jsonl` (147 notes / 10 sections).

| Backend | F1 weighted | Threshold | Verdict |
|---|---|---|---|
| **heuristic** (offline default) | **0.7871** | ≥ 0.65 | PASS |
| **Qwen3-4B-Instruct-2507 Q4_K_M** (recommended LLM tier) | **0.7938** | ≥ 0.75 | PASS |
| Qwen3-0.6B-Extract (indicative, unoptimised prompt) | 0.4443 | — | note |

Strong sections (heuristic): decisions 0.983 / lessons-learned 1.000 / experiments 1.000 /
feedback 1.000.

The LLM tier is an operator TOML option — default is `[curator] backend = "heuristic"`
(zero LLM, offline-first). Minimum recommended LLM tier: `Qwen3-4B-Instruct-2507 Q4_K_M`
(~2.5 GB binary, ~4 GB VRAM, F1 0.7938 measured).

### Drop-in compatibility (legacy vault v1.6.2)

- Wire/protocol: MCP tool names + REST endpoints `/api/v1/vault_*` (10 read + 3 write) — compatible
- DTO/shape: identical JSON fields + optional additive `tenant_id` — compatible
- Auth/ACL: same Ed25519 bearer JWT format, audience-scoped, deny-wins ACL — compatible
- Data content: empty stubs — full parity deferred to a future release
- Search/curator semantics: may diverge intentionally (gradatum is a new release, not a port)

---

## [0.1.0-alpha.2] — 2026-05-05

### Added

- **`gradatum-server`**: HTTP/MCP facade (Axum + figment + JSON tracing + 30 s SIGTERM drain)
- **`ServerConfig::validate_bind_tls()`**: fail-closed TLS configuration validation (5 cases)
- **`gradatum-core::TrustContext`**: mandatory enum propagated through all API handlers
- **`gradatum-auth::RevocationStore`** trait + `InMemory` + SQLite implementations + boot guard
- **JWT Ed25519** with scope-based TTL (1 h human / 24 h service)
- **`gradatum-acl-policy::AclEngine`**: deny-wins ACL (12 gold cases)
- **`gradatum-admin init`** CLI: auto-generates Ed25519 keys, bearer token, `bearer.toml`,
  and `server.toml` with defaults
- **10 MCP read endpoints**: drop-in API parity with legacy vault v1.6.2
- **`gradatum-mcp-stub`**: stdio → HTTP bridge + real JWT middleware
- **`/health`** endpoint (10 fields)
- **`/metrics:19091`** sidechannel with cardinality cap
- Shape parity tests (10 methods + smoke)
- Cross-platform support: Linux primary, Windows secondary tier
  ([RFC-0002](docs/RFC/RFC-0002-cross-platform-support.md))

### Drop-in compatibility (legacy vault v1.6.2)

- Wire/protocol: MCP tool names + REST endpoints `/api/v1/vault_*` — compatible
- DTO/shape: identical JSON fields + optional additive `tenant_id` (default `main`) — compatible
- Auth/ACL: same Ed25519 bearer JWT format, audience-scoped — compatible
- Data content: empty stubs — full parity deferred to a future release
- Search/curator semantics: may diverge intentionally (gradatum is a new release, not a port)

---

## [0.1.0-alpha] — 2026-05-04

Initial alpha release. Establishes the workspace foundation and all Layer 0/1/2 crates.

### Added

- **`gradatum-core`**: canonical types (`Note`, `Frontmatter`, `NoteId` ULID,
  `ContentHash` JCS RFC 8785, `NoteVersion`, `IntegritySignature`, `AuthorRef`, `Tag`,
  `Section`, `NoteStatus` 6-state machine, lazy `ExtraFields`); traits (`Index`, `AclPolicy`,
  `ACLFilter`, `Overridable`, `OverridePayload`); `AuditEvent` typed enum; embedded schema
  registry (4 TOML schemas via `include_dir!`); `GradatumError` taxonomy; `VaultConfig`
  runtime TOML (6 sub-sections: embed / curator / index / drift / audit / vault).
- **`gradatum-markdown`**: parser/writer for `Note` ↔ `String` round-trip + wikilink
  extractor regex.
- **`gradatum-cache`**: `EffectiveNoteCache` (moka LRU) with checksum validation on hit —
  zero stale-read risk under concurrency.
- **`gradatum-queue`**: SQLite job queue with `UPDATE...RETURNING` atomic claim, 5-minute
  lease, and 4 SQLite PRAGMAs (WAL, `synchronous=NORMAL`, `busy_timeout=5000`,
  `foreign_keys=ON`).
- **`gradatum-chat`**: `Chat` trait + 3 impls (`HeuristicBackend` offline, `HttpChat`
  OpenAI-compat, `Noop`) + `CircuitBreakerChat<C>` decorator (3 consecutive failures →
  5-minute cooldown).
- **`gradatum-embed`**: `Embedder` trait + 3 impls (`FastEmbedCpu` feature-gated,
  `HttpEmbedder`, `Noop`) + `FallbackEmbedder<P, F>` decorator.
- **`gradatum-index`**: `SqliteIndex` implementing the `Index` trait; FTS5 unicode61;
  complete schema (notes, audit_trail, note_index, generic note_overrides, file_checksums,
  history scaffold); three-level drift detection (size → prefix 4 KB → full SHA-256).
- **`gradatum-storage`**: `Storage` trait (OpenDAL) + `FileStorage` backend + NFS reject
  via `statfs`.
- **`gradatum-vault`**: registry + lifecycle (`write_note`: compose → persist → upsert
  index); `NoteMetadataOverride`; drift orchestration; `effective_note` cache.
- **`gradatum-curator`**: heuristic gating workflow + LLM review for low-confidence notes
  via `Chat` trait; 3 fallback strategies (`PendingReviewFallback` default / `Reject` /
  `AdmitPendingReview`).
- **`v1-parity-tests`**: 22 integration baseline tests (vault_crud, curator_workflow,
  drift_e2e, cache_concurrency, index_search, audit_trail, markdown_roundtrip,
  persistence_reopen).
- **`gradatum-bench`**: 9 active Criterion benches + 1 feature-gated + 2 standalone
  scripts. JCS hash baseline: 5.23 µs @ 10 KB.
- Workspace: pinned `=X.Y.Z` deps, `CHANGELOG.md`, `CONTRIBUTING.md`, `deny.toml` graph
  rule.

### Known limitations

- Unknown YAML keys in `ExtraFields` are silently dropped (`serde_yaml` without
  `#[serde(flatten)]`); deferred to a future release.
- `FastEmbedCpu` feature-gated (`fastembed-cpu`, off by default) due to an upstream
  `ort-sys` build script issue. Activatable via `cargo --features fastembed-cpu`.
- `Vault::read_note` and `update_status` return `NoteNotFound` (stubs); deferred to a
  future release.

---

## Past versions

- `0.1.0-scaffold` (2026-05-01) — initial workspace scaffolding.
- `0.1.0-phase0bis` (2026-05-03) — Phase 0bis re-structuring 17 -> 22 focused crates + RFC-0001 + CI enriched.
