# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

Storage trait carve, event-log sink, gateway cost-attribution, cognitive kind capture, and secrets DI.

> **Breaking change (deploy)**: first deploy of v0.3.0 invalidates all existing JWTs once (JWT signing key was ephemeral pre-fix; now persisted). Consumers must re-exchange their API key for a new JWT after deploy.

### Added

- **Storage trait carve (Étapes 0.1 + 0.2a)**: monolithic `trait Index` decomposed into three granular traits in `gradatum-core` — `DocumentStore` (note CRUD), `IndexStore` (FTS5, scoring, wikilinks), `VectorStore` (embedding insert/get + ANN). Supertrait façade `trait Index: DocumentStore + IndexStore + VectorStore {}` with blanket impl preserves all existing call sites. `AppState.search` switched from `Arc<SqliteIndex>` (concrete) to `Arc<dyn Index>` (vtable dispatch). Types `SearchHitRaw`, `AuthorRow`, `Lineage` migrated to `gradatum-core` as public primitives.
- **Event-log sink (Tranche B1)**: dedicated SQLite table `event_log` (migrations 0006/0007) — append-only, outside `notes`/`notes_fts` → zero FTS5 pollution and zero semantic index bloat. Endpoint `POST /api/v1/event-log` (JWT auth + ACL) with full hardening (timestamp bounds 400, field bounds 422, `DefaultBodyLimit`, log-injection sanitize). `EventLogStore` with `insert_batch` / `purge` / `count`. Retention task (tokio interval): 30-day TTL / 6-hour purge / 5M-row cap. Prometheus gauge `gradatum_event_log_rows`. Migration 0007 adds `agent_id` discriminator column (header `X-Agent-Id`).
- **gradatum-gateway crate**: autonomous LLM proxy service on `:8436`. Routes: `/v1/chat/completions` (+ SSE), `/v1/embeddings`, `/v1/rerank` (F-08 cross-encoder bge-reranker-v2-m3 ONNX), `/v1/models`, `/health`, `/metrics`. Replaces standalone LLM gateway + embedding service post-migration.
- **Gateway cost-attribution (Tranche B1 gateway)**: `QaEvent` enriched with 5 fields — `feature_id`, `model_used` (fallback-aware: actual resolved model, not primary alias), `tokens_input`, `tokens_output`, `cost_usd`. Client response unchanged. Streaming paths yield `tokens_* = None`.
- **F-42 cognitive kind capture (migration 0008)**: columns `c_kind` (4 CoALA categories: `episodic` / `semantic` / `procedural` / `reflective`) and `doc_kind` (`Event` / `Static`) on `notes` table. Derived deterministically from `section` via `section_to_c_kind()` / `section_to_doc_kind()` const fns in `gradatum-core`. Zero LLM, $0 runtime cost. `section` (g-section) remains the authoritative column; `c_kind`/`doc_kind` are derived metadata. Scoring unchanged (doc_kind usage deferred v0.4.0 F-17).
- **Secrets DI (F-13)**: `SecretsProvider` trait + `SecretBytes` (crate `secrecy`, Drop-zeroize, Debug masked) + `EnvSecretsProvider` + `FileSecretsProvider` in `gradatum-core/src/secrets.rs`. File secrets provider refuses overly-permissive permissions at load time.

### Changed

- **Workspace**: 26 → 28 crates (added `gradatum-gateway` + `gradatum-db-sqlite` promoted).
- **`AppState.search`**: `Arc<SqliteIndex>` → `Arc<dyn Index>` — vtable dispatch, negligible overhead. Enables future multi-impl and mock injection without recompile.
- **Job dequeue filter**: `dequeue_by_kind` fix — jobs are now dequeued with strict `kind` filter. Prior to this fix a Curate job could be claimed by the wrong worker type → DLQ → note loss (migration 010).

### Fixed

- **P0 — JWT signing key ephemeral (Tranche C)**: JWT Ed25519 signing key was regenerated on every server boot (`new_ephemeral()` at startup, `jwt_private_key_path` never loaded). Every restart invalidated all live JWTs. Key is now persisted: raw 32-byte Ed25519 seed, load-or-generate at boot via `FileSecretsProvider`, atomic write `O_CREAT` mode 0600, directory 0700. **One-time JWT invalidation occurs on first deploy of v0.3.0** — see breaking change note above.
- **Bug routing queue**: `dequeue_by_kind` missing `kind` predicate — a `Curate` job could be dequeued and processed by the wrong worker, routed to DLQ, causing note loss. Fixed with explicit `WHERE kind = ?` filter + migration 010.

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

- Crate count: 26 → 28 (`gradatum-gateway` + reclassified `gradatum-db-sqlite`).
- Migrations: 0006 (event_log table), 0007 (agent_id column), 0008 (c_kind/doc_kind columns), 010 (dequeue_by_kind fix).
- `gradatum-gateway.service :8436` replaces standalone LLM gateway + embedding service (REPLACE big-bang post-migration).
- Maintainer review: storage carve ratified (2026-06-01) + F-42 c-prime mapping ratified (2026-06-01) + multi-vault deferred (2026-06-02). Security review: all HIGH findings resolved.

## [0.2.0] — 2026-05-29

Apalis job infrastructure, Dead-Letter Queue, jobs introspection API with SSE, and Prometheus observability.

### Added

- **F-14 partial — Apalis job foundation**: 22 Job variants (`JobKind` enum) covering all curator and maintenance flows per v81 §6 spec. `JobRecord` 5-block structure: identity / intent / state / output / audit. All field types forward-compatible across v0.2.0 → v1.0 (Option wrapping, default impls, no breaking renames). Custom `GradatumQueue` facade over Apalis `Backend` trait; `SqliteQueueStore` impl (15 methods, atomic `UPDATE...RETURNING` lease). Framework-agnostic: future swap to Redis / RabbitMQ / Postgres requires only a new `QueueStore` impl.
- **F-15 — Dead-Letter Queue + Apalis Monitor**: automatic DLQ routing for jobs exceeding max retries (default 3). Apalis Monitor for multi-worker coordination. Layers: Timeout (per-kind) + Retry (exponential backoff) + CatchPanic (unwrap isolation) + LoadShed (queue saturation defense). Graceful shutdown: SIGTERM with 30s drain.
- **F-16 — Jobs introspection API + SSE + Idempotency**: five endpoints — `POST /api/v1/jobs` (enqueue, 202 + SSE link), `GET /api/v1/jobs/{job_id}` (status), `GET /api/v1/jobs/{job_id}/stream` (Server-Sent Events), `DELETE /api/v1/jobs/{job_id}` (cancel), `GET /metrics` (Prometheus scrape). Idempotency-Key header (UUID, max 256 chars, DOS-protected). `gradatum-admin jobs {list,show,cancel,retry}` CLI commands.
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

Security hardening (security review P1) + CI release.yml unblock — Council 1bis Archi v81 ACTÉ 2026-05-26.

### Security

- **Security review P1 jwt validate_nbf=true** : activation explicite `validation.validate_nbf = true` dans `crates/gradatum-auth/src/jwt.rs` ligne 258. `jsonwebtoken` v9 défaut `validate_nbf=false` aurait accepté silencieusement tout JWT avec claim `nbf` (not-before) dans le futur. SCOPE LOCK acté `safety-regression-guard` 2026-05-28.

### Infrastructure

- **CI release.yml unblock** : downgrade `actions/upload-artifact@v4` + `download-artifact@v4` → `@v3` (Forgejo Actions n'implémente pas l'API GHES v4). Job `build-docker` skip (`if: false`) jusqu'à provision runner Docker. Cible arm64 `continue-on-error: true` (cross-compile dépend Docker).

### Tests

- Cargo workspace : 0 régression. `cargo build --release` PASS local 40s. `cargo test -p gradatum-auth --release` : 13 PASS.

### Internal

- Re-application post-OSS-flip squash (commit historique `5ef3690` 2026-05-26 perdu post-squash Phase E OSS flip readiness, récupérable backup bare `~/tmp/gradatum-backup-pre-filter-repo-20260526_050215.git`).
- Backlog Phase 2.x.5 :
  - Runner Docker pour build arm64 + docker buildx ghcr.io
  - Anti-leak Pilier 1 résiduel — 6 patterns leaks dans tree HEAD (bloque ci.yml main leak-detector)

## [0.1.0-alpha.13] — 2026-05-10

Phase 2.x.4 Endpoints Completeness — Tasks 13/14/15/16 (maintainer review Round 2, consensus 4/4 100% post-fix in-spec rev2.1).

### Added

- **T13 B5 wikilinks post-curate** : `process_wikilinks_b5` dans `gradatum-worker` parse les `[[wikilinks]]` du body curaté et insère les arêtes dans `note_links` (`INSERT OR IGNORE`, non-fatal — un échec wikilink ne bloque pas la finalisation du job curate).
- **T14 B4 `title_lookup` intégré dans `vault_read`** : résolution `title → note_id` via `SqliteIndex::find_note_by_title(vault_id, title)` filtré sur `status='live'`. Fallback ULID parsing si lookup KO. Endpoint `vault_read` accepte désormais `{title}` ou `{id}` indifféremment.
- **T15 M4 `vault_trace` multi-mode** : query textuelle FTS5 (en plus de l'ancien mode ULID-only). Modes supportés : `{id: <ULID>}` (lineage parents+children directs), `{title: <str>}` (lookup B4 + lineage), `{query: <str>}` (FTS5 multi-match → top-N notes + lineage agrégé). Limit configurable.
- **T16 M5 `vault_context` budget tokens** : budget en tokens via heuristique `chars/3.0` char-safe (UTF-8). Agrégation top-10 notes via FTS5 + tri composite score + concaténation under-budget.
- **Step 0 council Round 2** : module `tests/helpers/mod.rs` partagé (worker + server) pour fixtures TestSqliteIndex + TestVaultEnv (fix A-rev2-1).

### Tests

- 779 → **796** PASS workspace (+17, cible spec rev2 +15 dépassée +2). 0 clippy default + `--features onnx-reranker`, 0 fmt diff, `cargo deny` GREEN.
- Smoke E2E `scripts/smoke-alpha-13.sh` (auth JWT exchange + `/health` + write→curate→read+trace+context).

### Internal

- Spec : `docs/specs/2026-05-10-phase-2x4-alpha-13-endpoints-completeness-spec.md` rev2.1 (council Round 1 → Round 2 → ACTÉ).
- Caveats backlog Phase 2.x.5 alpha.14 :
  - **C1** LIKE escape sur title_lookup (sécurité injection wildcard `%` `_`).
  - **C2** `vault_trace IN(...)` batch query au lieu de N×N round-trips.
  - **C7-bis** backfill colonne `title` 551/552 notes production (M8 alpha.10 introduit la colonne, backfill complet à finir).
  - **C9** wikilinks N×N parallèle `tokio::join_all` (actuellement séquentiel).
- Scripts : install script renamed to `install-gradatum-services.sh` (commit `50fa520`) + new `install-gradatum-stub-mcp.sh` (commit `2f845d7`) + `mcp.json.sample` artefact runtime gitignored (commit `6a7580b`).

## [0.1.0-alpha.12-bumps.1] — 2026-05-10

Plan bumps Phase 2.2 — supply chain hardening on `main` HEAD (maintainer review GO 2026-05-09 + adoption mode standard v1.1 — 5 PRs merge séquentiel).

### Changed

- **PR-1** `serde_yaml 0.9` → `serde_yml 0.0.12` (drop-in fork maintenu post-deprecation upstream `serde_yaml`).
- **PR-2** `rmcp 0.x → 1.x` + `schemars 1.x` (MCP protocol crate stabilisation).
- **PR-3** HTTP stack : `axum 0.8.9` + `tower-http 0.6.10` + `reqwest 0.13.3` (breaking changes adaptés routers + middlewares).
- **PR-5** Crypto + minor catch-up : `sha2 0.10 → 0.11` (breaking adapté `audit.rs` + `novelty.rs`) + `governor 0.8 → 0.10` + `nix 0.29 → 0.31` + 12 mineurs (`thiserror`, `anyhow`, `ulid`, `regex`, `once_cell`, `clap`, `unicode-segmentation`, `globset`, `tempfile`, `proptest`, `wiremock`, `futures`).
- **PR-6** TOML 1.x + MSRV : `toml 0.8 → 1.1.2` + `toml_edit 0.22 → 0.25.11` + MSRV workspace `1.75 → 1.85` + 5 corrections clippy `map_or → is_none_or` (activées avec MSRV 1.85) dans `frontmatter.rs`, `dedup.rs`, `novelty.rs`, `wikilinks.rs`, `audit_jsonl.rs`.

### Reported

- **PR-4** `rusqlite 0.32 → 0.39` **reporté Phase 2.3** : conflit `links="sqlite3"` avec `sqlx 0.8.6` (deux crates ne peuvent pas linker la même C lib statiquement). Attente `sqlx 0.9` stable.

### Tests

- 779 PASS / 0 clippy / 0 fmt / `cargo deny` GREEN après chaque merge (TNR strict par PR).

### Internal

- Trace : decision record `[DECISIONS][gradatum] PR-{1..6} plan bumps Phase 2.2 — 2026-05-10`.
- Bloqués chaîne crypto : `rand` / `pkcs8` / `ed25519` / `jsonwebtoken` (chaîne `rand_core 0.6` ↔ `ed25519-dalek 2.x` + `jsonwebtoken 10` nécessite `sha2 ^0.10`). Candidats PR-7 dès `ed25519-dalek 3.x` stable upstream.

## [0.1.0-alpha.12] — 2026-05-10

Phase 2.x.3 Multi-Facteur Scoring + Jina Cross-Encoder Reranker — Tasks 11/12/13/14 (maintainer review Round 2, consensus 4/4 100% post-fix in-spec rev2.1).

### Added

- **T11 multi-facteur scoring** :
  - `recency_factor = exp(-λ × days_since_created)` avec λ=0.01 (decay ~100j half-life).
  - `pagerank_factor.clamp(0.0, 1.0)` (normalisation indegree → [0,1]).
- **T12 backlinks queries** : `SqliteIndex::get_indegree(vault_id, note_id)` + `get_note_created_and_indegree(vault_id, note_id)` — isolation cross-vault stricte (pas de fuite indegree inter-vault).
- **T13 composite scoring** : `composite_score = rrf × (1 + α × recency_factor) × (1 + β × pagerank_factor)` avec α=0.2 et β=0.1 (pondérations conservatrices — tunable Phase 2.x.5).
- **T14 trait `Reranker` pluggable** :
  - `NoopReranker` (default, no-op identité).
  - `JinaOnnxReranker` feature-gated `onnx-reranker` (`ort 2.0.0-rc.9` + `tokenizers 0.21`) — cross-encoder Jina v2 base multilingual.
  - API : `async fn rerank(&self, query: &str, hits: Vec<RrfHit>) -> Result<Vec<RrfHit>>`.

### Fixed

- **API ort 2.0.0-rc.9** : la spec §11.5 référençait l'API `Tensor::from_array(shape, data)` — la réalité rc.9 utilise tuple `Tensor::from_array((shape, data))` + extraction via `try_extract_raw_tensor`. Corrigé in-spec rev2.1 + impl conforme.

### Tests

- 754 → **779** PASS workspace (+25, cible spec +23 dépassée +2). 0 clippy default + `--features onnx-reranker`, 0 fmt diff.

### Internal

- Spec : `docs/specs/2026-05-10-phase-2x3-alpha-12-multi-facteur-reranker-spec.md` rev2.1.
- Backlog : câblage `main.rs JinaOnnxReranker` conditionnel sur env var `RERANKER_ONNX_PATH` (Phase 2.x.4 amend backlog — actuellement `NoopReranker` câblé par défaut).

## [0.1.0-alpha.11-patch.1] — 2026-05-10

3 fondations résolution P0 BLOQUANT design spec v3 Round 1 spec alpha.12 (audit).

### Added

- **F2 DTO `SearchHit.title: Option<String>`** : ajout additif non-breaking dans `gradatum-dto::SearchHit` + propagation `RrfHit.title` → `SearchHit.title` dans handler `vault_search`. Permet aux clients de récupérer le titre H1 directement (sans round-trip `vault_read`).
- **F3 `GradatumError::Inference(String)` variant** : nouveau variant + `impl From<EmbedError>` côté `gradatum-core` (orphan rule respectée — `From` impl côté core, pas côté embed). Permet propagation propre erreurs embed/rerank dans la chaîne Result.

### Coverage

- **F1 RRF handler câblé** sur commit `eabc80d` (alpha.11 base) → 4 nouveaux tests E2E `vault_search_rrf_path.rs` (RRF fusion path + graceful degradation Noop embedder + embed error WARN+BM25 fallback + score monotonicité).

### Cosmetic

- `chore(gradatum-index)` clippy fixes pre-existing dans `search_semantic.rs`.

### Tests

- 740 → **754** PASS workspace (+14, 3 fondations + couverture E2E F1).
- 0 clippy, 0 fmt diff. RUSTSEC-2025-0068 `serde_yml` hors scope (deferred plan bumps PR-1 alpha.12-bumps.1).

## [0.1.0-alpha.10] — 2026-05-10

### Fixed
- **Bug1** : `vault_status.note_count` retourne désormais `COUNT(*) WHERE status='live'` (était stub `locus_count`).
- **Bug2** : `vault_status.total_size_bytes` retourne désormais `COALESCE(SUM(LENGTH(body_text)),0)` (était stub `tenant_count`).
- **B1** : `vault_search` prend en compte le paramètre `section` — filtre `AND n.section = ?` conditionnel via `search_fts_scored_filtered()`.

### Added
- **M6** : `vault_list` pagination réelle via `list_notes()` avec curseur ULID lexicographique — plus de stub T8 retournant des entrées vides.
- **M8** : Migration `0005_add_title_column` ajoutant la colonne `title TEXT` à `notes` + backfill H1 SQL + `extract_h1_title()` dans `gradatum-curator` + `upsert_note_title()` dans `SqliteIndex`.
- **M9** : Snippet FTS5 natif `snippet(notes_fts, 0, '»', '«', '...', 32)` dans `search_fts_with_snippet()` — localise le passage pertinent au lieu de tronquer la tête du body.

### Tests
- 698 → 720 PASS workspace (+22), 0 FAILED, 0 clippy warnings, 0 fmt diff, cargo deny GREEN.
- Nouveaux tests unitaires : `vault_status_tests` (7), `title_tests` (3), `section_filter_tests` (2), `snippet_fts_tests` (2), `vault_list_tests` (4), `extract_h1_title` (3) dans `gradatum-curator`.

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
- **`gradatum-warden` v0.0.1 (nouvelle crate workspace)** : couche L0 défense périmétrique — IP filter CIDR (`ipnet`) + rate limit per-IP token bucket (`governor 0.8`) + bypass loopback réel (`inner.call(req)` direct, body handler retourné). API publique stable : `WardenLayer`, `WardenConfig`, `WardenError`, `WardenDecision`. Aucune feature opt-in (audit/threat-intel/geoip/prometheus/hot-reload différés future RFC). Décision actée the maintainer leader override 2026-05-09.
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
