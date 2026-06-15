# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.2] - 2026-06-15

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

Studio-MVP milestone (F-37): a read-mostly operator UI over the vault plus the backend surfaces it consumes. Internal release (not published). No breaking changes; drop-in upgrade from v0.4.5. Public READMEs remain at v0.4.3.

### Added

- **gradatum Studio MVP** (F-37, direction "LEDGER"): 5 surfaces (React + TypeScript + Vite bundle) served by `ServeDir` under `/ui/*` without auth (LAN — the JS is public). Auth flow: the operator pastes an api-key → `POST /auth/exchange` → JWT stored in `sessionStorage` (never `localStorage`) → `Authorization: Bearer` on every `/api/v1/*` call. Hardened static serving: strict CSP, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Permissions-Policy: geolocation=(), microphone=(), camera=()`. SPA fallback (`ServeDir.fallback(ServeFile index.html)`): deep-links / refresh on client-side routes serve `index.html`; a missing bundle still returns a clean 404.
- **Opt-in score breakdown in `vault_search`** (S1.1): new request field `include_scores: bool` (default `false`, fully backward-compatible under `deny_unknown_fields`) enriches each `SearchHit` with a `ScoreBreakdown` object (`rrf_score`, `recency_factor`, `pagerank_factor`, `in_degree`, `trust_raw`, `trust_decayed`, `composite`, optional `bm25_rank` / `sem_rank`). Signals were already computed by the scoring pipeline and discarded — they are now exposed only when requested. No `rerank` column (NoopReranker by default). The legacy hardcoded `trust: 0.5` field is documented as deprecated. The MCP tool schema auto-derives the new field via schemars.
- **Review queue endpoint** (S1.2): new `GET /api/v1/review` (auth, paginated by ULID cursor) listing notes with `status IN ('pending-review', 'staging')`, with `provenance` (distinguishing `distilled` from curator) and a distinct legacy `staging` badge. `confidence` is not exposed (not persisted — honest copy).
- **Dashboard endpoint** (S1.3): new `GET /api/v1/dashboard` (behind auth; `/health` stays unauthenticated) aggregating, with no new table: `notes_by_status` (tolerant of out-of-enum legacy statuses), `forgotten_count`, `jobs_by_status` (`GROUP BY`, DLQ included), `queue_depth`, `wal_size_bytes` (`null` = "n/a", never a lying 0), and the last job summary. New trait methods `count_notes_by_status` (`DocumentStore`) and `count_jobs_by_status` (`QueueStore`, default empty + native `GROUP BY` override in `SqliteQueueStore`).
- **Move-to-locus endpoint** (S1.4): new `POST /api/v1/notes/{id}/move {locus}` performing an index-level `UPDATE notes.locus` (consistent with `vault_downgrade` / `patch_note_status`); the ULID is preserved (no redirect table). Strict `LocusId::parse` validation: non-empty, charset `[a-z0-9-/]`, ≤128 bytes, anti-traversal. Clean `400` / `404` / `422`. Physical `.md` relocation (and thus `vault_read` consistency) is intentionally deferred (backlog dette) — documented in the handler contract.

### Changed

- **Curator routes low-confidence notes to `PendingReview`** (S1.2): `CurateOutcome::Pending` now writes `NoteStatus::PendingReview` instead of `Staging` at the four worker sites (dispatch + apalis, create + reclassify), factored through a single source of truth `gradatum_curator::outcome_to_status` (`Admitted→Live`, `Pending→PendingReview`, `Rejected→None`) to close the parity-bug class. Semantically correct: `PendingReview` = awaiting judgement (feeds `/review`); `Staging` = optional human review. Validated by the curator golden-set F1 gate (orthogonal to the status flip — it measures section routing) plus a mapping parity test.
- **`/health` T12 wiring** (S1.3): the previously stubbed `sqlite_wal_size_bytes` and `queue_depth` are now real — WAL size read from `AppState.wal_path` (`<index.db>-wal`, set by `with_search_path`) and queue depth derived from `count_jobs_by_status` (`Pending`). `queue_oldest_age_secs` stays 0 (deferred — no dedicated `QueueStore` method).

### Fixed

- **Locus preserved on re-upsert** (P1-1): `upsert_note` now guards `locus` with `CASE WHEN notes.content_hash IS NOT excluded.content_hash THEN excluded.locus ELSE notes.locus END`. A re-upsert from a stale `.md` (unchanged content hash, as after an index-level `update_note_locus`) no longer clobbers a moved locus; a genuine content change still applies the frontmatter locus. Same spirit as the trust P1-1 fix (`CASE`/`IS NOT`), discriminant = `content_hash`.
- **Review queue tolerates a malformed id** (R7): `list_review_queue` skips a non-ULID id (data anomaly) with a `warn` instead of failing the whole page with a 500; valid rows keep being served.

### Tests

- Workspace tests pass, zero failures; `clippy --workspace --all-targets` clean.
- New coverage: opt-in score breakdown (rrf ranks, omitted/present, MCP schema), curator status-flip parity + worker observable flip, review queue E2E + non-ULID resilience, dashboard aggregate + health re-check, move-locus E2E (success/400/404/422) + `LocusId::parse` unit, locus preservation on re-upsert, studio router (security headers, SPA fallback, missing-bundle 404).

## [0.4.5] — 2026-06-11

Backend-foundations milestone: multi-backend-readiness for the index (testability + decoupling) without shipping an alternative backend. Internal release (not published). No breaking changes; drop-in upgrade from v0.4.4.

### Changed

- **Worker type-erased on `Arc<dyn Index>`** (W1): the worker now depends on the type-erased `Arc<dyn Index>` façade instead of the concrete `Arc<SqliteIndex>`, unifying the composition root with the server. Eight inherent `SqliteIndex` methods used by the worker outside the three storage traits were promoted into `IndexStore` with neutral default implementations (`M-FEATURES-ADDITIVE`): `set_note_trust`, `write_temporal_entry`, `delete_redirect_by_ulid`, `delete_note_from_index`, `list_garbage_older_than`, `get_note_status`, `get_note_section`, `is_note_forgotten`. An alternative backend does not have to implement them to compile; `SqliteIndex` overrides each by delegation. No rusqlite type is exposed in any promoted signature.

### Added

- **Backend-agnostic index parity suite** (W2): new test-only crate `index-parity-tests` locking the observable contract of the `Index` trait (`DocumentStore` + `IndexStore` + `VectorStore` façade). A `make_index() -> Arc<dyn Index>` factory selects the backend via the `GRADATUM_INDEX_BACKEND` env var (default `sqlite` in-memory) — adding a backend is one match arm + one CI matrix entry, zero duplicated tests. 24 tests across 7 invariant families: write→read round-trip + content hash, FTS + semantic cosine (descending order, downgraded exclusion), status state machine / decay, temporal_index idempotence, dynamic-trust preservation on re-upsert, lesson recall, forget lifecycle. New split CI job `index-backends` (matrix `[sqlite]`) on Forgejo + GitHub.

### Fixed

- **Purge tolerates an unreadable status**: `handle_purge` no longer aborts the whole batch when `get_note_status` fails to parse a candidate's status (e.g. an out-of-enum `'downgraded'` value appearing mid-loop). The offending note is counted ignored + logged (`warn`) and the batch continues purging the other Garbage notes, consistent with the per-note TOCTOU re-check intent.

### Tests

- Workspace tests pass, zero failures; `clippy --workspace --all-targets` clean.
- `index-parity-tests` runs against the sqlite backend via the factory; the `index-backends` CI matrix is extensible to alternative backends.

## [0.4.4] — 2026-06-11

Distillation milestone: semantic distillation jobs, trust-decay scoring, consumable event-log, and lesson recall. Internal release (not published). No breaking changes; drop-in upgrade from v0.4.3.

### Added

- **Semantic distillation** (F-22): new `Distill` job (`DistillSource`, mode `Semantic` only) that clusters non-processed notes of a scope by embedding cosine similarity (threshold 0.75, batch capped at 500) and writes one synthesis note per cluster in `pending-review` with `provenance: "distilled"` and `derived-from` links; source notes are marked `processed` / `derived-into` via copy-on-write-safe extra fields (no parasite versions). Dry-run is the default; the cron schedule is documented but never enabled by default; vault-wide scope is refused outside dry-run. New `TRUST_SCORES["distilled"] = 0.60`.
- **Trust-decay scoring** (F-17): `composite_score` gains an optional trust-decay multiplier applied at the RRF layer only (never BM25), with per-provenance half-lives configurable (default `distilled` 90 days; `human-decision` no decay). Global flag `trust_decay_enabled` (default **on**; can be disabled) makes scores bit-identical to v0.4.3 when disabled. Modifier order documented: forgotten (short-circuit) > downgraded > [RRF × recency × pagerank × trust_decay]. Gated behind a search golden-set non-regression check.
- **Consumable event-log** (F-19): engines emit a semantic `agent_id` and a `feature_id` derived from request type (`embed` vs `chat`); the event-log store gains transactional reader methods (`fetch_pending`, `mark_processed`) for the distillation pipeline.
- **Lesson recall** (F-60): new `GET /api/v1/lessons/recall?class=<x>&limit=<n>` endpoint — BM25-only (no LLM) over the `lessons-learned` section, filtered to a controlled vocabulary of 12 classes (`400` otherwise), excluding lessons tagged `codified` and forgotten notes; returns `{items:[{ulid, title, snippet, tags, anchor_ms}]}` with a sub-50ms target. Also exposed as the `vault_lessons_recall` MCP tool (19th tool).
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

> **Internal notes removed from public changelog** for clarity. This release implements storage hardening and temporal features planned in the v0.4.x roadmap.

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
- **Caveats** : history pruning policy (v0.4.2), trust decay scoring (v0.4.1).

### Internal

- Crate count: 28 crates.
- Migrations: 0010 (provenance, redirect, trust).
- Backlog: history retention policy (v0.4.2), trust decay scoring (v0.4.1), event log retention.

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

Enrichissement `title`/`section` des hits sémantique-only dans `vault_search`.

### Fixed

- **`vault_search` : `title = null`, `section = ""` pour les hits sémantique-only** :
  après la fusion RRF, les notes présentes uniquement dans le signal sémantique
  (absentes de `bm25_map`) conservaient `title = null` et `section = ""` dans la
  réponse finale. Une passe d'enrichissement batch (`get_titles_sections` — 1 seul
  `SELECT … WHERE id IN (…)`) récupère désormais `title` et `section` depuis la
  table `notes` pour tous les hits manquants, juste avant la construction des
  `SearchHit`. Les enrichissements BM25 existants ne sont pas écrasés.
  `snippet` reste `None` pour les hits sémantique-only : pas de match FTS5
  disponible pour générer un extrait localisé.

### Added

- **`IndexStore::get_titles_sections`** : nouveau helper batch sur le trait
  `gradatum-core::IndexStore` — `SELECT id, title, section FROM notes WHERE
  vault_id = ? AND id IN (…)` — utilisé par la passe d'enrichissement ci-dessus.
  Implémenté dans `gradatum-index::SqliteIndex::get_titles_sections` (délégation
  via `index_store_impl.rs`).

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

- **Storage trait decomposition**: monolithic `trait Index` decomposed into three granular traits in `gradatum-core` — `DocumentStore` (note CRUD), `IndexStore` (FTS5, scoring, wikilinks), `VectorStore` (embedding + ANN). `trait Index` façade with blanket impl preserves call site compatibility. `AppState.search` uses vtable dispatch (`Arc<dyn Index>`). Types `SearchHitRaw`, `AuthorRow`, `Lineage` made public.
- **Event-log sink**: dedicated SQLite table `event_log` (migrations 0006/0007) — append-only, outside notes/notes_fts. Endpoint `POST /api/v1/event-log` with timestamp/payload bounds, log-injection sanitization. `EventLogStore` with `insert_batch` / `purge` / `count`. Retention policy: 30-day TTL, 6-hour purge interval, 5M-row cap. Prometheus metric included.
- **gradatum-gateway crate**: autonomous LLM proxy service (`:8436`). Routes: `/v1/chat/completions` (+SSE), `/v1/embeddings`, `/v1/rerank` (ONNX cross-encoder), `/v1/models`, `/health`, `/metrics`. Replaces standalone LLM services.
- **Cost attribution**: `QaEvent` enriched with feature_id, model_used (fallback-aware), tokens_input/output, cost_usd. Streaming paths omit token counts.
- **Cognitive kind capture** (migration 0008): columns `c_kind` (CoALA categories: episodic / semantic / procedural / reflective) and `doc_kind` (Event / Static) added to `notes`. Derived deterministically from `section` via const functions in `gradatum-core`. Zero LLM runtime cost. `section` remains authoritative; `c_kind`/`doc_kind` are derived metadata. Scoring unchanged (doc_kind usage deferred).
- **Secrets DI (F-13)**: `SecretsProvider` trait + `SecretBytes` (crate `secrecy`, Drop-zeroize, Debug masked) + `EnvSecretsProvider` + `FileSecretsProvider` in `gradatum-core/src/secrets.rs`. File secrets provider refuses overly-permissive permissions at load time.

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
- Security review findings: all HIGH resolved (F1–F5 event-log endpoint + V1–V5+V8 JWT/secrets).

### Internal

- Crate count: 26 → 28 (added `gradatum-gateway` + `gradatum-db-sqlite`).
- Migrations: 0006 (event_log), 0007 (agent_id), 0008 (c_kind/doc_kind), 0010 (job routing fix).
- Design review: storage traits finalized, gateway architecture approved, multi-vault deferred to v1.0.
- Security review: all high-priority findings resolved.

## [0.2.0] — 2026-05-29

Apalis job infrastructure, Dead-Letter Queue, jobs introspection API with SSE, and Prometheus observability.

### Added

- **Apalis job infrastructure**: 22 Job variants (`JobKind` enum) covering curator and maintenance flows. `JobRecord` 5-block structure with forward-compatible fields. Custom `GradatumQueue` façade over Apalis `Backend`. `SqliteQueueStore` with atomic lease semantics. Framework-agnostic: future swap to Redis/RabbitMQ/Postgres needs only a new `QueueStore` impl.
- **Dead-Letter Queue + Monitor**: automatic DLQ routing for jobs exceeding max retries. Apalis Monitor for multi-worker coordination with timeout, retry, panic isolation, and load shedding layers. Graceful shutdown with 30s drain.
- **Jobs introspection API**: five HTTP endpoints for job lifecycle (enqueue, status, stream, cancel) + Prometheus metrics. Server-Sent Events for streaming. Idempotency-Key header support. `gradatum-admin jobs` CLI commands for inspection and control.
- **Prometheus exporter**: `:19091` pull endpoint, disabled by default (`metrics_enabled = true` in config to enable). Per-job-kind metrics.
- **`gradatum-db-sqlite` crate (new)**: isolates SQLite queue implementation — 15 methods, WAL mode, index on `(vault_id, job_kind, status)`.

### Fixed

- **E-12 — `SqliteQueueStore::get()` stale payload**: record lifecycle fields (`started_at`, `completed_at`, `duration_ms`) were desynchronised from authoritative SQL columns. Fix: sync from SQL in `get()` (commit `e739517`).
- **E-25 — `duration_ms` stub**: `JobResult.duration_ms` was hardcoded 0. Now measured via `std::time::Instant` injected in `record_to_task` and recovered in `GradatumAcknowledger::ack()`. Smoke LIVE confirmed `duration_ms = 990ms` (commit `0a6e51e`, merge `c5d9f98`).
- **E-23 — Apalis ack/complete wiring**: `apalis::Backend::ack`/`complete` now properly wired via `GradatumAcknowledger` (commit `63dae03`).
- **E-24 — `enable_tracing` panic**: `enable_tracing` re-enabled + `TaskId` injection in `record_to_task` resolves panic in Apalis rc.9 `make_span`.

### Tests

- Workspace: **886 PASS** (up from 826 alpha.15 baseline, +60 new).
- **0 FAILED** across workspace.
- E2E integration: write note → curator job enqueued → Monitor processes → metric exported → SSE subscribers notified.

### Internal

- Crate count: 25 → 26 (added `gradatum-db-sqlite`).
- Maintainer review: GO-CAVEATS (2026-05-26); design spec Round 2 APPROVED; Phase 4.2 post-impl audit APPROVED-WITH-CAVEATS (1 P0 + 3 P1 all resolved 4.2bis).
- Spec: `docs/specs/2026-05-29-v0-2-0-apalis-job-infra-spec.md` (26 écarts §11, all ratified).

## [0.1.0-alpha.15] — 2026-05-28

Polish Phase 2.x.5 — Tasks 17-24 (8 tasks, TDD, 826 PASS workspace, +22 vs alpha.14 baseline).

### Security

- **Task 19 — LIKE escape `title_lookup`** : wildcards SQLite `%` et `_` dans un titre sont
  désormais échappés via `escape_like_pattern` + `ESCAPE '\\'` SQLite — élimine les faux positifs
  LIKE dans `vault_read` / `vault_trace` / classify (Phase 3 TDD — commit `7f16a8c`).

### Performance

- **Task 20 — `vault_trace` seeds JoinSet** : résolution parallèle des seeds (`tokio::JoinSet`)
  dans `gradatum-server` — élimine le round-trip séquentiel N×seeds (Phase 2 TDD — commit
  `f814fc6`).
- **Task 22 — wikilinks `title_lookup` JoinSet** : résolution parallèle des wikilinks dans
  `gradatum-worker` via `tokio::JoinSet` — remplace la boucle `.await` séquentielle (Phase 2
  TDD — commit `6e3b31f`).
- **Task 23 — reranker tokenize-once** : `encode_batch` pré-tokenisation en une passe dans
  `gradatum-search` (WONTFIX baseline — commit `fb7b063`).

### Behavior

- **Task 18 — curator classify cascade** : `vault_classify` appelle désormais le curateur LLM
  en cascade (B3) et applique les caveats C1-C4 (normalisation catégorie, fallback sur erreur
  curateur, propagation status) (Phase 4 TDD — commit `f29fde7`).

### Data migration

- **Task 21 — backfill-titles** : sous-commande `gradatum-admin backfill-titles` ajoutée —
  renseigne la colonne `title` des notes NULL depuis le champ `body` (M8 alpha.10, 551/552 notes
  prod backfillées en production) (Phase 5 TDD — commit `18b4ea2`).

### Cleanup

- **Task 17 — retrait `X-Gradatum-Wait`** : header stub `X-Gradatum-Wait` et logique
  `sync_wait` supprimés de `gradatum-server` — comportement déjà async-only (Phase 1 TDD —
  commit `3dce54f`).
- **Task 24 — audit scoring.rs** : audit dead code `scoring.rs` — 0 dead code détecté, aucune
  modification nécessaire (Phase 1 — commit `fb2c80a`).

### Tests

- Workspace : **826 PASS** (+22 vs alpha.14 baseline, 0 régression). `cargo test --workspace`
  PASS. `cargo deny` GREEN. 0 clippy warnings.
- Smoke E2E `scripts/smoke-alpha-15.sh` : LIKE injection + rate limit WardenLayer validés.
- V5 (rate limiting `vault_search`) : couvert par WardenLayer existant (même middleware
  `write`/`curate`).

### Internal

- Backlog post-alpha.15 :
  - Security review V6-V9 (surface crypto) — dédiée.
  - Runner Docker CI pour build arm64 + docker buildx ghcr.io.

## [0.1.0-alpha.14] — 2026-05-28

Security hardening and CI release infrastructure.

### Security

- **JWT not-before validation** : explicitly enabled `validation.validate_nbf = true` in `crates/gradatum-auth/src/jwt.rs`. Default behavior in jsonwebtoken v9 skips this check, silently accepting future-dated tokens.

### Infrastructure

- **CI actions pinning** : pinned artifact actions to v3 for Forgejo Actions compatibility. Docker build job disabled pending docker-capable runner provisioning.

### Tests

- Cargo workspace : 0 régression. `cargo build --release` PASS local 40s. `cargo test -p gradatum-auth --release` : 13 PASS.

### Internal

- Re-application post-OSS-flip squash (commit historique `5ef3690` 2026-05-26 perdu post-squash Phase E OSS flip readiness, récupérable backup bare `~/tmp/gradatum-backup-pre-filter-repo-20260526_050215.git`).
- Backlog Phase 2.x.5 :
  - Runner Docker pour build arm64 + docker buildx ghcr.io
  - Anti-leak Pilier 1 résiduel — 6 patterns leaks dans tree HEAD (bloque ci.yml main leak-detector)

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

### Internal

- Design review completed. Backlog items for alpha.14:
  - Title lookup wildcard escape (SQL injection prevention).
  - vault_trace batch queries (performance optimization).
  - Title column backfill (551/552 notes).
  - Wikilink resolution parallelization.
- Scripts : install script renamed to `install-gradatum-services.sh` (commit `50fa520`) + new `install-gradatum-stub-mcp.sh` (commit `2f845d7`) + `mcp.json.sample` artefact runtime gitignored (commit `6a7580b`).

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

- 779 PASS / 0 clippy / 0 fmt / `cargo deny` GREEN après chaque merge (TNR strict par PR).

### Internal

- 779 PASS / 0 clippy / 0 fmt / `cargo deny` GREEN per PR.
- Crypto upgrade chain blocked on upstream stabilisation (`ed25519-dalek 3.x`, `jsonwebtoken 10`). Planned for future cycle.

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

### Internal

- Backlog: environment variable configuration for reranker model path (currently `NoopReranker` default).

## [0.1.0-alpha.11-patch.1] — 2026-05-10

Design foundations: SearchHit enrichment and error propagation.

### Added

- **SearchHit title enrichment** : `SearchHit.title` field populated from RrfHit, eliminating need for round-trip `vault_read` calls.
- **Inference error handling** : `GradatumError::Inference` variant for clean error propagation from embed/rerank layers.

### Coverage

- **RRF handler integration** : 4 new E2E tests for RRF fusion path, graceful degradation, and error handling.

### Cosmetic

- `chore(gradatum-index)` clippy fixes pre-existing dans `search_semantic.rs`.

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
- **`vault_downgrade` côté gradatum** : parité avec the legacy vault MCP, prérequis pour migration propre.
  - Schema : migration `0004_vault_downgrade.sql` ADD COLUMN `replaced_by TEXT REFERENCES notes(id)` + index partiel `idx_notes_status_downgrade WHERE status='downgraded'`.
  - DTO : `gradatum-dto::VaultDowngradeRequest/Response` + `NoteStatusPatch` + extension `VaultSearchRequest.include_downgraded` (default false).
  - Endpoints : POST `/api/v1/vault_downgrade` (parité MCP, sync 200) + PATCH `/api/v1/notes/:id` (flexibilité status, 204).
  - SQL helpers : `SqliteIndex::downgrade_note(id, reason, replaced_by)` + `patch_note_status(id, status?, reason?, replaced_by?)` (idempotent UPDATE, NotFound si rows=0).
  - SQL helper test : `SqliteIndex::seed_note(id, section, body)` pour tests E2E.
- **Filter search avec pénalité downgraded** :
  - `vault_search` exclut `status='downgraded'` par défaut (filtre WHERE).
  - `include_downgraded=true` → score BM25 multiplié par 10 en Rust (amplifie négativité → score plus mauvais en ordre ASC, sémantiquement équivalent à 10% de pertinence).
  - `Index::search_fts_scored` trait : ajout param `include_downgraded: bool` + retour `Vec<(NoteId, f64, String_status)>` pour traçabilité.
- **MCP tool `vault_downgrade`** : exposé via gradatum-mcp-stub (drop-in compatibility with legacy vault) — déjà câblé Tasks 5-6 (tool_def + dispatch + EXPECTED_TOOL_NAMES).
- **Migration tool `gradatum-admin downgrade-from-legacy-vault-trash`** : porte `.vault-trash/<date>/*.md` du legacy vault vers gradatum (idempotent + dry-run + --limit). Heuristique match : substr(body_text, 1, 200) chars UTF-8-safe.
- 25+ tests régression : 3 schema migration + 5 DTO + 5 SQL helpers + 4 endpoints E2E + 4 search filter + 2 MCP smoke + 3 migration tool.

### Changed
- **`Index::search_fts_scored` trait signature** : ajoute `include_downgraded: bool` en 4ème param. Retour étendu `Vec<(NoteId, f64, String_status)>`. Call sites adaptés (handler vault_search, tests fts5_search, MockIndex stub).
- **Naming `replaced_by`** : aligné DTO + SQL + handlers (pas `superseded_by` du plan initial — corrigé pour cohérence avec module pré-existant `gradatum-dto/src/vault_downgrade.rs` et consommateurs `gradatum-worker/src/dispatch.rs`).
- **POST `/api/v1/vault_downgrade` sync 200** : remplace l'ancien handler async 202 (write::vault_downgrade conservé en code mais désactivé).
- **2 tests parity ajustés** : `poll_job_status::vault_downgrade_returns_*` + `write_synthetic::test_19_downgrade_*` mis à jour pour refléter contrat sync 404 (anciennement 202 async queue).

### Internal
- Décision the maintainer 2026-05-09 : SOFT downgrade only (status='downgraded' + body conservé). DELETE = exception manuelle hors MVP. Search default exclut downgraded. Score BM25 pénalisé pour downgraded.
- Convention `:id` (axum 0.7.9) au lieu de `{id}` du plan initial pour PATCH /notes/:id.
- Tests : 639 → **668 PASS** workspace (+29).
- Spec : `docs/superpowers/specs/2026-05-09-vault-downgrade-design.md`.
- Plan : `docs/superpowers/plans/2026-05-09-vault-downgrade-plan.md`.

## [0.1.0-alpha.8-patch.1] — 2026-05-09

### Fixed
- **Template `default_server_toml` (init.rs)** : ajout section `[embed]` manquante. Bug découvert post-deploy alpha.8 smoke Phase 9 : 4 jobs embed_note (3 backfill + 1 chaînage post-curate) drainés `duration_ms=0` skip silencieux car `WorkerEmbedConfig::extract_inner("embed")` retournait défauts → `enabled=None` → embedder=None côté Dispatcher → `process_embed_note` early-return sans appel HTTP. Fix : section `[embed]` ajoutée au template avec defaults explicites (enabled=true, timeout_ms=5000).
- **Defaults model/dim corrigés** : `EmbedConfig::default()` et template `[embed]` corrigés de `bge-small-en-v1.5/384` → `bge-m3-Q8_0/1024`. Corrigé simultanément dans `config.rs` (`default_embed_model` + `default_embed_dim`) et template `init.rs`.
- Tests régression ajoutés : `merge_adds_embed_section_when_backup_lacks_it` (`tests/init_merge.rs`) + `embed_defaults_match_documented_values` (`config.rs`) + mise à jour `embed_section_defaults`.

### Internal
- Cause racine #1 : Backend Tasks 6 (`f6912d7`) + 8 (`f5cb510`) ont ajouté `EmbedConfig` struct à `ServerConfig` mais oublié template TOML init. Pattern à mémoriser : toute nouvelle section serde-serializable de `ServerConfig` doit être propagée au template `generate_server_toml_template` simultanément.
- Cause racine #2 : valeurs model/dim non vérifiées lors de la spec. Pattern à mémoriser : vérifier `curl <service>/v1/models` AVANT de hardcoder model/dim dans les defaults.

## [0.1.0-alpha.8] — 2026-05-09

### Added
- **`gradatum-warden` v0.0.1** : perimeter defense layer — IP filter CIDR + rate limit per-IP token bucket + loopback bypass. Public API: `WardenLayer`, `WardenConfig`, `WardenError`, `WardenDecision`. Features (audit, threat-intel, geoip, prometheus, hot-reload) deferred to future RFC.
- **V3 Rate limiting** sur `/api/v1/*` + `/auth/exchange` (exempt `/health`, `/metrics`). Default 60 req/min, burst 10, `exempt_localhost` configurable. Réponse 429 + `Retry-After`. Section `[ratelimit]` server.toml. Implémenté via `gradatum-warden::WardenLayer`.
- **V4 Auth bearer optionnel `/jobs/:id`** : flag `[auth].require_jwt_jobs_endpoint` (default `false` — invariant réseau privé). Si `true` → bearer JWT requis sur `GET /api/v1/jobs/:id`. Préparation expo réseau public futur.
- **Embeddings async pipeline** : worker post-curate enqueue automatique `kind="embed_note"` chaîné. Drain → HTTP POST vers l'endpoint embeddings configuré → INSERT `note_embeddings` (UPSERT BLOB f32 LE). Section `[embed]` server.toml (default endpoint `localhost:8431`, model `bge-small-en-v1.5`, dim 384, timeout 5s).
- **`gradatum-admin backfill-embeddings`** : sous-commande CLI scan notes sans embedding via LEFT JOIN + enqueue `embed_note` jobs idempotent. Args `--root` `--tenant` `--limit`.
- `SqliteIndex::insert_note_embedding` + `get_note_embedding` helpers (UPSERT BLOB f32 LE, validation `vector.len() == dim`).
- `AppState.embedder: Arc<dyn Embedder>` (default `Noop`) + builder `with_embedder`.
- Worker handler `process_embed_note` + `with_embedder` + `with_index` builders.
- `EmbedConfig` + `RateLimitConfig` dans `ServerConfig` (defaults serde).
- 19+ tests régression : 6 unit warden + 3 E2E warden Router + 3 ratelimit server + 4 auth-jobs + 3 embed_pipeline worker + 1 chained_jobs server + 2 backfill admin + 1 EmbedConfig + 2 ratelimit config + 2 auth-jobs config + 1 embed config.

### Changed
- **Bypass loopback fix critique** : régression bloquante évitée — clients loopback :19090 (local MCP stub, monitoring agent, smoke scripts) reçoivent désormais le body handler réel (pas `Body::empty`). Implémenté via `WardenService::call` early return `inner.call(req)`.
- `gradatum-embed` : feature `fastembed-cpu` confirmée hors `default`. HTTP backend par défaut. Pas de dépendance ORT requise.
- Pipeline ingestion : embed_note non bloquant (note déjà persistée vault+FTS5 avant enqueue chained, best-effort).
- 2 tests existants ajustés (`e2e_write.rs` + `v1-parity-tests/write_synthetic.rs`) : `depth==0` → `depth==1` cohérent avec chaînage embed_note post-curate.

### Removed
- Dep workspace `tower_governor 0.5` (introduite Tasks 1-2 puis retirée Task W1 — bug archi error_handler termine la chaîne avec body vide → incompatible bypass loopback).

### Internal
- Nouvelle crate workspace `gradatum-warden` (25 → 26 crates).
- Tests : 591 → **636 PASS** workspace (+45). 0 régression. Clippy 0 warning.
- 13 commits Phase 2.1.1 alpha.8.
- Trace décision : decision record `[DECISIONS][gradatum] Création crate gradatum-warden MVP v0.0.1 — 2026-05-09`.
- RFC périmètre complet warden (IP filter avancé, circuit breaker périmètre, threat intel, GeoIP, audit jsonl, prometheus, hot-reload SIGHUP) → différée future RFC-0004.

## [0.1.0-alpha.7-patch.6] — 2026-05-08

### Fixed
- **Lease leadership pas libérée à l'arrêt propre** : `gradatum-worker` recevait SIGTERM,
  loggait "arrêt propre", mais ne supprimait PAS sa row dans la table `worker_leadership`.
  Conséquence : tout stop+start rapide via `install-gradatum-services.sh` ou `systemctl restart`
  laissait le worker suivant en cycle retry ~60-75s (4 retries de 15s) avant takeover
  post-expiration TTL.

### Changed
- `LeaderElection::release()` ajouté dans `leader.rs` : DELETE conditionné
  `WHERE holder = ?` pour libérer la lease (race-safe — ne touche pas une lease prise
  par un autre worker entretemps).
- À l'arrêt propre (`main.rs`), `el.release().await` appelé après `renewal.abort()`.
  Best-effort : erreur loggée mais non fatale (TTL fallback via patch.5 `Restart=always`).
- Latence takeover stop+start : **~60-75s → <1s** (cycle complet de redéploiement).

### Added
- 4 tests d'intégration `crates/gradatum-worker/tests/leadership_cleanup.rs` :
  `release_removes_own_row`, `release_is_idempotent`,
  `release_only_self_not_other_holder`, `release_without_acquire_is_noop`.

### Notes
- Combiné avec patch.5 (`Restart=always RestartSec=15s`), garantit zéro intervention
  manuelle post-deploy.
- Le DELETE est best-effort : si DB locked ou pool closed à l'arrêt, lease expire
  naturellement via TTL (fallback patch.5).

## [0.1.0-alpha.7-patch.5] — 2026-05-08

### Fixed
- **gradatum-worker reste `inactive` après stop+start rapide** : `Restart=on-failure` dans `packaging/systemd/gradatum-worker.service` ne couvrait pas l'exit 0 légitime "pas leader" (worker concurrent tenant encore la lease). Sans relance auto, le service restait `inactive (dead)` jusqu'à intervention manuelle après expiration naturelle de la lease (~60s).
- Découvert post-deploy alpha.7-patch.4 : redéploiement via `install-gradatum-services.sh` stoppait le worker (sans cleanup lease), redémarrait immédiatement → nouveau worker voyait l'ancien holder valide → exit propre → service inactive.

### Changed
- `Restart=on-failure` → `Restart=always`, `RestartSec=5s` → `RestartSec=15s` dans le unit file. Systemd relance toujours, lease leadership expire naturellement (~60s), takeover automatique au prochain cycle (4 retries max avant succès).
- Commentaires de motivation ajoutés inline dans le unit file pour documenter la décision.

### Notes
- Ce patch est purement packaging (unit file). Le binaire `gradatum-worker` reste inchangé. Une amélioration future (patch séparé ou Phase 2.2) pourrait ajouter un cleanup lease explicit dans le worker sur SIGTERM (handler signal Rust) pour éliminer la latence de takeover.

## [0.1.0-alpha.7-patch.4] — 2026-05-08

### Fixed
- **CRITIQUE — bug merge structurel patch.2** : `walk_and_merge` dans `gradatum-admin/src/init.rs` itérait sur les keys du NEW template, supprimant ainsi les sections présentes UNIQUEMENT dans le backup user (sections d'extension comme `[curator]`, `[curator.llm]`). Découvert en LIVE après deploy alpha.7-patch.3 : gradatum-worker passait en `inactive (dead)` car la section `[curator]` du server.toml LIVE était wipée par le merge. Phase A manuelle (restore depuis backup) a contourné ; patch.4 corrige le code de fond.

### Changed
- **Sémantique merge inversée** : le BACKUP devient autoritaire pour TOUT contenu user (sections custom, sections d'extension, keys customisées). Le NEW template ne fait qu'augmenter avec :
  - Nouvelles keys/sections absentes du backup (ajoutées avec leurs valeurs défaut)
  - Valeurs défaut pour keys que le backup ne définit pas
- Comportement existant préservé : KEY_MIGRATIONS renames (`db_path` → `vault_index_path`) appliqués pré-walk sur une copie du backup pour cohérence (évite l'insertion de l'ancienne key dans le résultat).
- Suppression des helpers `lookup_item_mut` et `set_item` (remplacés par `set_item_or_insert` et `remove_path` dédiés aux KEY_MIGRATIONS pré-walk).
- Ajout compteur `user_added` dans les logs merge (sections/keys préservées comme extensions user).

### Added
- 2 tests régression : `merge_preserves_backup_only_sections_curator` (reproducer exact du bug LIVE) + `merge_adds_user_only_top_level_section` (extension custom user top-level).
- `set_item_or_insert` — helper insertion avec création du nœud intermédiaire si absent.
- `remove_path` — helper suppression d'une key par chemin dotted.

## [0.1.0-alpha.7-patch.3] — 2026-05-08

### Added
- **Backup atomique `bearer.toml`** — `gradatum-admin init --force` (et donc `install-gradatum-services.sh`) sauvegarde désormais `bearer.toml` en `.bak.<ISO-TS>` avant écrasement. Mitigation minimaliste cohérente avec `server.toml` (patch.2). Le merge consumer-aware n'est pas implémenté (out of scope ; `bearer.toml` LIVE = 0 customisation actuelle, risque concret nul).
- 2 tests régression : `materialize_preset_backups_existing_bearer_toml` + `materialize_preset_no_backup_on_fresh_install`.

### Notes
- Si une customisation manuelle de `bearer.toml` est introduite à l'avenir, le `--force` re-init la perd dans le fichier actif mais elle reste récupérable via le backup. Pour préservation automatique → patch futur consumer-aware (Phase 2.2+).

## [0.1.0-alpha.7-patch.2] — 2026-05-08

### Added
- **Merge structurel `server.toml`** — `gradatum-admin init --force` ne réécrit plus aveuglément. Pattern : backup atomique `.bak.<ISO-TS>` + merge dirigé par schéma du nouveau template. Préserve customisations user (`[curator.llm].base_url`, `api_key_env`, `timeout_ms`, `jwt_ttl_*`, etc.) ; ajoute nouvelles clés avec défauts ; écarte clés legacy absentes du nouveau schéma.
- **Table `KEY_MIGRATIONS`** explicite — gère renames cross-version (alpha.7 RT11 : `storage.db_path` → `storage.vault_index_path`).
- **3 tests régression merge** : `merge_preserves_user_curator_customizations`, `merge_drops_legacy_db_path_via_rename_migration`, `merge_keeps_new_keys_with_defaults`.

### Internal
- Dépendance workspace `toml_edit = "=0.22.27"` (préserve format + commentaires TOML via `DocumentMut`).
- `gradatum-admin` : ajout target `[lib]` pour exposition aux tests d'intégration merge sans passer par le binaire.
- `generate_server_toml_template` et `merge_user_config` exposées `pub` (réutilisation tests + future CLI `validate`).

## [0.1.0-alpha.7-patch.1] — 2026-05-08

### Fixed
- **Régression Task 16 G2** : `gradatum-admin init` template `server.toml` utilisait encore `db_path` legacy au lieu de `vault_index_path` canonical. Tout init frais ou re-init `--force` déclenchait le WARN deprecated au boot. Fix `init.rs:232` + test régression dans `init_clean.rs`.

## [0.1.0-alpha.7] — 2026-05-08

### Changed
- **RT11** : `[storage].db_path` renommé en `[storage].vault_index_path`. Backward-compat via `serde(alias)` — l'ancien nom continue de fonctionner avec un WARN au boot. Retrait définitif prévu alpha.7+1.

### Added
- `StorageConfig::legacy_alias_used()` — détection alias deprecated.
- `build_snippet` fonction `pub(crate)` (caveat C2 — déduplication test/prod).
- 3 tests régression UTF-8 ZWJ emoji boundary (caveat C1 POSTMORTEM).
- `EXPECTED_TOOL_NAMES` constante test mcp-stub (caveat C3 — count dynamique).

### Notes
- Caveat C4 (vérif UI mirrors secondary-mirror + GitHub) → action manuelle the maintainer post-deploy.

## [0.1.0-alpha.6] — 2026-05-08

### Fixed
- **RT5** : `GET /api/v1/jobs/<id>` retourne désormais le statut réel depuis `jobs_v2`. Avant : stub T3 P2.0b retournait toujours `"pending"`. Après : transition `pending` → `leased` → `done` observable. **Breaking comportemental** : id inexistant retourne `404 Not Found` (au lieu de `200 + pending`). Audit consumers requis (mcp-stub poll loop, external agent).
- **BM25 ranking** : `POST /api/v1/vault_search` utilise désormais `bm25(notes_fts)` natif FTS5 au lieu d'un score positionnel proxy. Score normalisé `[0..1]` via `1.0 / (1.0 + bm25.abs())`.
- **Information disclosure** (caveat V1 security review 2026-05-08) : `last_error` mappé vers codes opaques (`invalid_input` / `vault_error` / `storage_error` / `processing_error`) avant retour API. Empêche leak chemins FS, ULIDs reflétés, état interne via anyhow.

### Added
- `Queue::get(id) -> Option<JobInfo>` (trait async).
- `SqliteQueue::get` impl avec `SELECT ... FROM jobs_v2 WHERE id = ?`.
- `Index::get_note(tenant_id, note_id) -> Option<NoteRecord>` (trait async, P1 council résolu).
- `Index::search_fts_scored(...) -> Vec<(NoteId, f64)>` (BM25 réel).
- `SqliteIndex::search_fts_scored` impl avec `bm25(notes_fts)`.
- `JobInfo` struct (lecture meta job sans claim).
- `JobStatus::as_str` + `from_str` helpers.
- `sanitize_job_error` mapping codes opaques.
- `NoteRecord` déplacé dans `gradatum-core::index` (Option A — types portables L0).
- 11 tests régression (Queue::get unit, status helpers, sanitize, E2E poll_job_real, BM25 ordering, BM25 mapping).

### Internal
- Workspace clippy `-D warnings` GREEN (résolution warnings préexistants admin/server).
- 566/545 PASS workspace (+21 tests cumul Phase 2.1 G1).

## [v0.1.0-alpha.5] — 2026-05-07 (P2.0c-bis Auth Path 2)

### Added

- **Auth Path 2 — API key + /auth/exchange** (spec §2 core minimum viable)
  - `gradatum-acl-auth::ApiKeyStore` trait + `SqliteApiKeyStore` impl argon2id cost m=19456 KiB / t=2 / p=1 (T1)
  - Migration SQL `api_keys` table + index + integration init (T2)
  - CLI `gradatum-admin api-key {create,list,revoke,rotate}` commands (T3)
  - CLI `gradatum-admin token issue` Path 3 minimal scope (T4)
  - Endpoint `POST /auth/exchange {api_key}` 401 uniform hors middleware JWT (T5)
  - `SqliteRevocationStore` wired runtime + check on exchange (T6)
- **D3-complet** : Claims.tenant_id mandatory + TrustContext propagation + middleware layer (T7)
- **Tests integration** : 11 tests E2E + tenant propagation (T8)
  - `auth_e2e_full_flow.rs` 5 tests (create key → exchange → check TTL)
  - `auth_tenant_propagation.rs` 6 tests (TrustContext leak + middleware accept/reject)
- `scripts/smoke-alpha-5.sh` 9 étapes acceptance + bonus RAM check (T9)
- `ExchangeResponse` V2 §2.4 : 5 champs `token`, `ttl_secs`, `scopes`, `tenant_id`, `kid`
- `AppState::with_acl_preset_path()` wiring depuis `cfg.acl.preset_path` (E1 fix 2026-05-07)

### Changed

- `ExchangeResponse.expires_in` → `ttl_secs` (cohérence spec V2)
- `AuthConfig::default()` `revocation_db_path` + `api_keys_db_path` : absolus passim via config, au lieu de `None` auto-dérivé
- Migration `api_keys.sql` : retiré `PRAGMA journal_mode = WAL` (sqlx::migrate exécute en transaction implicite — SQLite refuse conflit). WAL config via `SqliteConnectOptions::journal_mode(Wal)` à la connexion, AVANT `sqlx::migrate!`.

### Fixed

- **Recovery post-NAS-freeze 2026-05-06** : 24 binaires `target/debug/deps/` corrompus via `zstd: stdout: I/O error` (fix escalier infra : VM 200 8 vCPU + 28 GB RAM + NFS timeo=600/retrans=5) → cleanup + rebuild.
- `AclEngine` not loading from `cfg.acl.preset_path` (E1 — auparavant hardcodé preset vide → tous vault_* 403 dès alpha.5 déploiement). Fix : `AppState::with_acl_preset_path()` reads TOML + inject.

### Tests

- **492 PASS / 0 FAIL / 9 ignored** (vs 446 baseline alpha.4 — net +46)
- `cargo test --workspace --no-fail-fast` vert sur main `3572183`
- 0 regression vs alpha.4 feature set

### Known limitations (deferred Phase 2.1)

- `JsonlFileSink` audit events auth câblés bout-en-bout — D6 retenu (writeable stubs alpha.5)
- Rate limiting `/auth/exchange` — D7 retenu pour Phase 2.1 shared rate limit
- Scopes granulaires — D1 retenu : flat scopes alpha.5 (`read`, `write`, `admin`)
- API key rotation auto-scheduled
- ACL filter list par tenant_id runtime (documented tests T8)
- Worker dispatch + Vault.read_note stubs → alpha.6 ou rc.1

### Security

- argon2id: `m=19456 KiB`, `t=2`, `p=1` (crate defaults, OWASP 2023 compliant)
- CSPRNG `OsRng` for secret generation (128 bits effective entropy per key)
- 401 uniform on `/auth/exchange` (no key enumeration leak)
- Constant-time argon2id verify (via `argon2` crate)
- API key displayed ONCE at creation (D8 UX, no re-display)
- Revocation store SQLite wired runtime (check on all exchange calls)
- C-A2 MINOR note : log distinguishability AlreadyRevoked (internal only) deferred Phase 2.1

### Recovery + Incident

- **2026-05-06 23:32** : filesystem unavailability (NFS soft timeout) triggered build artifact corruption — 24 `target/debug/deps/` files zstd-corrupted + 2 latent code bugs surfaced (WAL pragma in migration, AuthConfig defaults absolute) → fixed in `6f76a8e` + `3572183`.

### Spec

- **Spec source** : `docs/superpowers/specs/2026-05-06-p2.0c-bis-auth-path-2-design.md` (V2 ACCEPTED)
- **Conformité** : 16/16 checks (post maintainer re-audit GO 2026-05-07)
- **Tag** : `v0.1.0-alpha.5` annotated tag-object SHA `30f87cf3` → commit `d39685d` (docs release)
- **Commits** : `6f76a8e` (chunks 1+2+WAL), `aa5b2f6` (deps MAJ), `c8f5628` (chunk 3 T8), `3572183` (chunks 4+5+fixes), `d39685d` (docs release pointé par tag)

### Post-tag fixes (Drift #5, #6, install scriptée)

- **Drift #5** — `7be4cd2` : `queue_path` convention `<root>/queue.db` → `<root>/db/queue.sqlite` (align db/ folder layout)
- **Drift #6** — `a951b52` : `gradatum-admin init --preset` embed hiérarchique+flat via `include_str!` (idempotent install script)
- **Scripts/install** — `a951b52` : `scripts/install-gradatum-services.sh` (10 étapes) + `packaging/systemd/README.md` section "Installation via script" (SystemdUser mode) + bonus fix : `server_smoke.rs` regression TempDir creation (db/+vault/ subdirs auto-create)
- **Phase B acceptance** — 2026-05-07 17:00 UTC :
  - Deploy systemd (UID/GID 985 gradatum, MemoryMax server 512M, worker 1G MemorySwapMax=0)
  - Services gradatum-server :19090 + gradatum-worker actifs (verified via `systemctl status`)
  - Smoke acceptance `smoke-alpha-5.sh` 4 PASS / 5 WARN / 0 FAIL (auth Path 2 critères runtime validés)
  - 492 tests PASS / 0 FAIL / 9 ignored (0 regression vs alpha.4 baseline)

---

## [v0.1.0-alpha.4] — ~2026-05-15 (P2.0c Runtime Wiring) — tag not yet released

**Note** : Phase 2.0c alpha.4 planned but deferred post-freeze recovery. Alpha.5 Path 2 auth skipped ahead Phase 2.0c-bis (2026-05-07) as independent sub-phase per Phase 2.0 §2 spec option.

---

## [v0.1.0-alpha.3] — 2026-05-05 (P2.0b Write+Curator)

### Added

- `gradatum-queue` SqliteQueue + Queue trait + UPDATE...RETURNING atomic lease — T1 (`2ef6a54`)
- `gradatum-worker` leader election SQLite CAS + dispatcher loop + SIGTERM + GC stale leases — T2 (`4c668fc`)
- 3 MCP write handlers (`vault_write` / `vault_classify` / `vault_downgrade`) async 202 + jobs poll endpoint — T3 (`35d82c2`)
- `gradatum-curator` cascade 5 fonctions (novelty SHA-256+MinHash 128 / routing regex Bayesian 10 sections / tags TF-IDF top-5 / wikilinks Jaro-Winkler 0.88 / dedup cosine 0.95) — T4
- 5 LLM backends protocole-génériques (`HeuristicBackend` / `OpenAiCompatBackend` / `OllamaCompatBackend` / `AnthropicCompatBackend` (prompt caching ephemeral) / `GeminiCompatBackend`) — T5-T9 (`aa4a59f`)
- `CircuitBreaker<B>` wrapper backoff exp 30→60→120→300s + HalfOpen success_threshold=2 + 7 tests — T5-T9
- Audit log JSONL (`HttpAuditEvent` + `JsonlFileSink` rotation daily mode 0640 + content_hash JCS RFC 8785) — T10 (`b93fa4a`+`57450e2`)
- Bench curator F1 (`gradatum-bench` binary `curator_f1` + LLM_ENDPOINT/LLM_MODEL configurables env) — T11 (`f580b6f`→`8d38d45`→`9d9d0a8`→`9701f82`)
- OpenDAL feature gate per-backend opt-in (`fs` default + `s3`/`gcs`/`azure`/`all-cloud`) — T12 (`6a8e93f`)
- Systemd packaging (`gradatum-server.service` MemoryMax=512M / `gradatum-worker.service` MemoryMax=1G + **MemorySwapMax=0** caveat swap saturation / `sysusers.d/gradatum.conf` UID 990) — T13 (`1be7998`)
- TOML config curator (`[curator] backend = "heuristic"` default + `[curator.llm]` opt-in) + classifier-v1 prompt embedded via `include_str!` — T14 (`a90645f`)

### Fixed

- `gradatum-curator::routing` regex `\b SECTION \b` cassé sur préfixes `[SECTION]` — fix : 2 couches `PREFIX_PATTERNS` + `KEYWORD_PATTERNS` + 6 tests — T11 fix `9d9d0a8`
- `gradatum-bench::curator_f1` body Markdown brut polluait LLM léger — fix : `clean_body_for_llm()` strip headings/wikilinks/code/frontmatter — T11 fix `9d9d0a8`

### Bench results — exit P2.0b ✅ ALL PASS

Dataset gradatum-natif `gradatum-balanced-v1-final.jsonl` (147 notes / 10 sections gradatum, construit Phase Z 2026-05-05).

| Backend | F1 weighted | Threshold | Verdict |
|---|---|---|---|
| **heuristic** (offline default) | **0.7871** | ≥ 0.65 | ✅ PASS |
| **Qwen3-4B-Instruct-2507 Q4_K_M** (LLM tier production) | **0.7938** | ≥ 0.75 | ✅ PASS |
| Qwen3-0.6B-Extract (an external agent service, prompt extract non optimisé classification) | 0.4443 | indicatif | (note) |

Sections fortes heuristic : decisions 0.983 / debug 0.938 / retrospectives 0.844 / reasoning 0.791 / lessons-learned 1.000 / experiments 1.000 / feedback 1.000.

**Recommandation production LLM tier** :
- Tier minimum recommandé : `Qwen3-4B-Instruct-2507 Q4_K_M` (~2.5 GB binaire, ~4 GB VRAM, F1 0.7938 mesuré)
- Tier "qualité" : `claude-haiku-4-5` cloud, `gemini-1.5-pro`, `Qwen3.6-27B Thinking` local
- Tier minimal install : `heuristic` uniquement (zero LLM, F1 0.7871)

Le LLM tier reste **option config TOML opérateur** — par défaut `[curator] backend = "heuristic"` (zero LLM, offline first-class).

### Drop-in compatibility (legacy vault v1.6.2)

- L1 wire/protocol : ✅ MCP tool names + REST endpoints `/api/v1/vault_*` (10 read + 3 write)
- L2 DTO/shape : ✅ champs JSON identiques + `tenant_id` optionnel additif
- L3 auth/ACL : ✅ bearer JWT Ed25519 audience-scoped + ACL hierarchical deny-wins
- L4 datas accessibles : ❌ stubs vides — full content parity deferred to Phase 2.1 with `migrate-from-v0`
- L5+ search/curator semantics : may diverge intentionally (gradatum is a release, not a port)

### Design references

- design spec P2.0 — 2026-05-04 (+ addendum 2026-05-05 amendement R-A3)
- lessons-learned record `[LESSONS][gradatum] T11 bench — routing.rs regex \b cassé sur préfixes [SECTION] + Qwen body Markdown brut régresse — 2026-05-05`
- reasoning record `[REASONING][gradatum] Phase Z dataset gradatum-natif — pattern 4 phases — 2026-05-05`
- experiments record `[EXPERIMENTS][gradatum] Qwen3-4B-Instruct-2507 Q4_K_M F1w=0.7938 PASS T11 P2.0b — confirmé 2026-05-05`

---

## [v0.1.0-alpha.2] — 2026-05-05 (P2.0a Foundation + Read API)

### Added

- `gradatum-server` HTTP/MCP façade (Axum + figment + JSON tracing + SIGTERM 30s drain) — T1 (`5131ce2`)
- `ServerConfig::validate_bind_tls()` C3 fail-closed (5 cases) — T2 (`f2eedb8`)
- `gradatum-core::TrustContext` enum mandatory C10 — T3 (`4d10584`)
- `gradatum-auth::RevocationStore` trait + InMemory + SQLite + boot guard C2 — T4 (`1c6529c`)
- JWT Ed25519 with scope-based TTL (R-A1: 1h human / 24h service) — T5 (`08a02fd`)
- `gradatum-acl-policy::AclEngine` deny-wins B2/B3 (12 gold cases) — T6 (`dbb0958`)
- `gradatum-admin init` CLI (Q3 auto-gen Ed25519 keys + bearer + bearer.toml + server.toml R-A1 defaults) — T7 (`2a7ae07`)
- 10 MCP read endpoints (drop-in API parity legacy vault v1.6.2 — R-A2) — T8 (`d2d7779`)
- `gradatum-mcp-stub` stdio→HTTP bridge + real JWT middleware — T9 (`e84030a`)
- `/health` endpoint D1 (10 fields) — T10 (`cd083ae`)
- `/metrics:19091` sidechannel + cardinality cap C7 — T11 (`5989e19`)
- Shape parity tests (10 methods + smoke) — T12 (`5ca50f0`)
- Cross-platform support: Linux primary + Windows secondary tier via [RFC-0002](docs/RFC/RFC-0002-cross-platform-support.md) — Sprint X-1

### Drop-in compatibility (legacy vault v1.6.2)

- L1 wire/protocol : ✅ MCP tool names + REST endpoints `/api/v1/vault_*` identiques
- L2 DTO/shape : ✅ champs JSON identiques + `tenant_id` optionnel additif (default `main`)
- L3 auth/ACL : ✅ même bearer JWT format Ed25519 audience-scoped
- L4 datas accessibles : ❌ stubs T8 vides — full content parity deferred to Phase 2.1 with `migrate-from-v0`
- L5+ search/curator semantics : may diverge intentionally (gradatum is a release, not a port)

### Design references

- design spec P2.0 — 2026-05-04 (+ addendum 2026-05-05 amendement R-A3)
- reasoning record `[REASONING][gradatum] Search analytics — 4 options + préco D Phase 3 + subset A Phase 2.1 — 2026-05-05` (RFC-0004 deferred Phase 3)

---

## [0.1.0-alpha] - 2026-05-04

### Added

- **Phase 1 complete** — 14 livrables T01-T14 + L0-AUDIT.
- `gradatum-core` (L0) : types canoniques (Note + Frontmatter + NoteId ULID + ContentHash JCS RFC 8785 + NoteVersion + IntegritySignature + AuthorRef + Tag + Section + NoteStatus state machine 6 etats + ExtraFields lazy), traits (Index + AclPolicy + ACLFilter + Overridable Patch/Output + OverridePayload), AuditEvent typed enum + ExtraFields, schema_registry embedded `include_dir!` (4 schemas TOML), GradatumError taxonomy + VaultOnNfs (C11), VaultConfig runtime TOML (6 sub-sections : embed/curator/index/drift/audit/vault).
- `gradatum-markdown` (L1) : parser/writer Note <-> String round-trip + wikilink extractor regex.
- `gradatum-cache` (L1) : EffectiveNoteCache moka LRU avec checksum validation on hit (D-perf-2 / B22) — 0 stale risk concurrent.
- `gradatum-queue` (L1) : SQLite jobs avec UPDATE...RETURNING atomic claim + lease 5min + 4 PRAGMA C12 (WAL + synchronous=NORMAL + busy_timeout=5000 + foreign_keys=ON).
- `gradatum-chat` (L1) : trait Chat + 3 impls (Heuristic offline + HttpChat reqwest OpenAI-compat + Noop) + CircuitBreakerChat<C> decorator (3 consec failures -> 5min cooldown).
- `gradatum-embed` (L1) : trait Embedder + 3 impls (FastEmbedCpu fastembed bge-small-en-v1.5 384d feature-gated + HttpEmbedder reqwest + Noop) + FallbackEmbedder<P, F> decorator.
- `gradatum-index` (L1) : SqliteIndex impl Index trait (Q5DAG core trait), FTS5 unicode61, schema complet (notes + audit_trail + note_index + note_overrides generique B20 + file_checksums + history scaffold), drift Phase A 3 niveaux (size strict -> prefix-4KB -> full sha256).
- `gradatum-storage` (L2) : OpenDAL Storage trait + FileStorage backend + statfs NFS reject (caveat C11 BLOQUANT).
- `gradatum-vault` (L2) : registry + lifecycle (write_note compose md + persist + upsert index) + NoteMetadataOverride impl Overridable+OverridePayload + drift orchestration (delegue scan_phase_a) + effective_note cache.
- `gradatum-curator` (L2) : workflow heuristic gating + LLM review low-conf via Chat trait + 3 fallback strategies (PendingReviewFallback default / Reject strict / AdmitPendingReview soft) — D-perf-3 / B23.
- `v1-parity-tests` : 22 tests d'integration baseline D5 (vault_crud + curator_workflow + drift_e2e + cache_concurrency + index_search + audit_trail + markdown_roundtrip + persistence_reopen).
- `gradatum-bench` : 9 benches criterion actifs + 1 feature-gated + 2 scripts standalone (B1-B10/B2a/B2b/B5/B8a/B8b). B1 P0 mesure 5.23us @ 10KB JCS hash.
- Workspace : pinned `=X.Y.Z` deps (R11 + AM3 + spec §2.12), CHANGELOG.md, CONTRIBUTING.md task workflow, deny.toml graph rule.

### Caveats Phase 1 (P1/P2 reportes v0.2.0+)

- **B8** Ecart B8 (T04) : ExtraFields YAML inconnus silencieusement perdus (`serde_yaml` sans `#[serde(flatten)]`). Forward-compat partiel — fix Phase 2+ via evolution Frontmatter.
- **T08 Concern** : FastEmbedCpu feature-gated `fastembed-cpu` (off par defaut) — bug ort-sys 2.0.0-rc.12 via private registry (build script ureq v3 vs v2). API publique complete, impl gated. Activable via `cargo --features fastembed-cpu`. Fix upstream Phase 1+ via registry update.
- **L0 Audit** P1 fixes inline (Note::verify_integrity + ExtraFields JCS doc). 5 P2 reportes Phase 2+.
- **T11 stubs** : `Vault::read_note` et `update_status` retournent `NoteNotFound` Phase 1. Cache miss path stub. Enrichis Phase 2+.
- **T13** : 22 tests actifs + 3 ignored documentes. Port complet 260 tests predecesseur reporte Phase 2+ (D5 = >=30 jours real-world usage, pas test count).

### References

- Spec design : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` v3.2 (commit `c86554f`)
- Annexe perf : `docs/superpowers/specs/2026-05-04-phase1-perf-bench-results.md`
- Plan : `docs/superpowers/plans/2026-05-04-phase1-backend-plan.md`
- Progress tracker : `docs/superpowers/PROGRESS.md`
- Bench results : `docs/BENCH.md`
- v1 parity inventory : `docs/superpowers/notes/v1-test-inventory.md`

---

## Past versions

- `0.1.0-scaffold` (2026-05-01) — Phase 0 initial scaffolding.
- `0.1.0-phase0bis` (2026-05-03) — Phase 0bis re-structuring 17 -> 22 focused crates + RFC-0001 + CI enriched.
