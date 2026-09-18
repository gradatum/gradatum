# SECURITY.md

> Vulnerability disclosure policy for Gradatum.

---

## Reporting a vulnerability

**Do not open a public issue for security vulnerabilities.**

Send a report to `security@gradatum.org`. No PGP key is published; reports may be sent in plain text and are triaged with the same urgency.

Include:

- A clear description of the issue and its impact.
- Steps to reproduce (proof-of-concept code or commands).
- The Gradatum version and commit hash you tested against.
- Your name / handle for credit (optional — anonymous reports are accepted).

---

## Response timeline

| Phase | Target |
|---|---|
| Acknowledgement | Within **72 hours** of receipt. |
| Triage + severity assessment | Within **7 days**. |
| Fix or mitigation | Within **30 days** for High / Critical, **90 days** for Medium / Low. |
| Public disclosure | Coordinated with reporter; default 90 days after triage or 14 days after fix release, whichever comes first. |

---

## Severity classification

We use [CVSS 3.1](https://www.first.org/cvss/v3.1/specification-document) scoring. Severity bands:

| CVSS score | Band |
|---|---|
| 9.0–10.0 | Critical |
| 7.0–8.9 | High |
| 4.0–6.9 | Medium |
| 0.1–3.9 | Low |

---

## Supported versions

**Current supported version**: `2.x` (from `2.0.0`). The `1.x` line is closed and receives no
final security fix — no `1.x` release follows `1.0.0`. All `0.x` tags are end-of-life and
receive no backports. `1.0.1` and `1.0.2` were never published; those numbers are skipped
rather than reused.

| Version | Status | Security fixes |
|---|---|---|
| `2.x` | Supported | Yes — ongoing. |
| `1.x` | Closed | None issued for this line. The fix below is reachable only by migrating to `2.x`. |
| `0.x` | End-of-life at `1.0` | No backports. |

**`1.0.0` is affected by a boot-log secret disclosure.** A secret misplaced in the
configuration file — an incorrect type, invalid syntax, or an unknown variant — was echoed
verbatim into the boot log by the configuration error path. The fix has been present since
`2.0.0` (published 2026-08-13); no `1.x` release carries it, and none will. Operators running
`1.0.0` must migrate to `2.x` to receive it — there is no in-place `1.x` patch to apply.

Migrating is not automatic: a dependent pinned to the `1` major (for example `gradatum =
"1.0"`) keeps resolving within `1.x` and will not pick up this fix on its own — raising the
pin to `2.x` is a deliberate action the dependent has to take. `1.0.0` itself is not pulled
from the registry, and will not be: yanking it would not remove the exposure for anyone still
resolving to it, would break existing lockfiles that reference it, and would not deliver the
fix. Migrating forward is the only route that does.

---

## Hardening defaults

By design, Gradatum applies these defaults at boot time:

- **Bind loopback only** (`127.0.0.1`) by default. Non-loopback binds require `[server.tls]` to be configured; the server refuses to boot on a non-loopback address without TLS (fail-closed).
- **Native TLS termination supported** (since v0.5.2) via `[server.tls] { cert_path, key_path }` (rustls 0.23, TLS 1.2+/1.3, `axum-server` `bind_rustls`). Certificate and key are loaded before bind: any load failure (missing or corrupt PEM, insufficient permissions) aborts startup rather than falling back to cleartext. The deployed default (`127.0.0.1:19090`, no `[server.tls]` block) remains loopback behind a reverse proxy.
- **JWT audience-scoped strict** (`aud=service-X` exact match), Ed25519, mandatory `kid` header, TTL 1 h for `human` scope and 24 h for service/machine scope.
- **Persistent revocation store** required. In-memory store is allowed in dev with a `WARN` log on every boot. The store is consulted on every request and fails closed; see the revocation caveat below for what does — and does not — write to it.
- **Gateway body logging is not implemented.** No prompt or response content is written to disk by the LLM gateway. Encryption at rest for logged payloads is planned for a future release. Header sanitisation is not currently enforced at the gateway layer.
- **OpenDAL backends gated by feature flags** — only the backends explicitly enabled are compiled in.

- **Studio admin UI auth**: JWT stored in **localStorage** (key `gradatum_studio_jwt_persist`, persists across page reloads; api-key is never stored). Short-lived: **1 h** (scope `human`). Service/machine tokens: 24 h.
- **Strict Content Security Policy (studio)**: `script-src 'self'`, `connect-src 'self'`, `style-src 'self'`, `frame-ancestors 'none'` — no `unsafe-inline` or `unsafe-eval` on scripts. Served by gradatum-server (`gradatum-server/src/studio.rs`).
- **Studio response headers**: `Content-Security-Policy` (above) + `X-Content-Type-Options: nosniff` + `Referrer-Policy: no-referrer` + **`X-Frame-Options: DENY`** (complements `frame-ancestors` for legacy browsers: IE11, Safari < 10).
- **Body limits (anti-DoS)**: `/mcp` and `/internal/v1/persist/embedding` capped at **512 KiB**.
- **`/auth/exchange` rate-limited**: WardenLayer (defaults: 60 req/min, burst 10) + argon2id on api-key verification.
- **Code-map read surface (`POST /api/v1/code_scope`) — privileged-only**: code_scope returns source-code symbols (function signatures, bodies when `include_body=true`) from indexed `code-*` vaults. These vaults carry **no `tenant` column** (migrations 0017/0018 — `code_vault` is provisioned per-project, not per-tenant). To prevent cross-tenant source-code disclosure, the handler enforces a **privileged-context gate** (`is_code_scope_privileged`): only `TrustContext::Studio` (admin UI), `TrustContext::Mtls` (service-internal), and `BearerToken` with `sub == "main-agent"` (orchestrator owner) are authorized. A regular tenant token → `403 Forbidden`, regardless of grant breadth. See `crates/gradatum-server/src/api_v1/code_scope.rs` for the implementation and the handler-level doc.
- **Known limitations (CAVEATS)**:
  - **TLS**: gradatum serves cleartext by default. **TLS (rustls) is required for any non-loopback exposure.** A cleartext deployment outside loopback exposes the JWT in transit. The operator is responsible for configuring `[server.tls]`.
  - **Revoking an API key does not invalidate JWTs already issued from it.** `gradatum-admin api-key revoke` marks the key as revoked and stops it being exchanged for new tokens. It does not write to the revocation store, and the JWT verification path does not re-read the state of the originating key. A token issued before the revocation therefore stays valid until its `exp` — up to **1 h** for `human` scope and **24 h** for service/machine scope. Server-side token revocation is **planned for a future release**.
    **Emergency procedure**: to cut every outstanding token immediately, rotate the Ed25519 signing seed — remove `<storage.root>/config/jwt-signing-key.secret` (with the default `storage.root`: `/var/lib/gradatum/config/jwt-signing-key.secret`) and restart `gradatum-server`, which generates a new one and logs a `WARN` naming the path. This invalidates *all* tokens for *all* keys, so every consumer must re-exchange its API key. There is no way to invalidate a single outstanding token in `2.x`.
  - **Logout is client-side only**: the studio logout clears localStorage (`localStorage.removeItem('gradatum_studio_jwt_persist')`). A JWT stolen before logout remains valid until it expires (≤ 1 h for studio sessions), for the reason above.
  - **Rate-limit and reverse-proxy**: `ratelimit.exempt_localhost = true` by default. Set to `false` if gradatum runs behind a reverse-proxy (otherwise the loopback bypass disables rate-limiting on `/auth/exchange`).
  - **HSTS**: not emitted by default. Recommended when the operator enables TLS.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full security design and caveats.

---

## Privacy posture

_As of v2.5.0. Review this section on each minor release._

Gradatum is designed for local-first, self-hosted deployments. The following data handling properties hold in v2.5.0:

- **Data at rest is not encrypted.** The SQLite index (`index.db`), note frontmatter, note body text, and version history snapshots (`.history/`) are stored in plaintext on the configured storage backend. Encryption at rest (e.g. LUKS locally, or server-side encryption on an object store) is the operator's responsibility.
- **The vault may now leave the local machine entirely.** Since v2.0.0, setting `[storage] service = "s3"` stores the vault — the notes themselves, the primary data — on an object storage provider rather than on local disk. The Markdown bodies are written there **in plaintext**; Gradatum applies no encryption of its own before writing. This is a fourth egress path, and the broadest of them: the LLM, curator and embedding paths send note content for processing, whereas this one *hosts* it. It is opt-in and off by default — the default backend is the local filesystem, and no note leaves the machine unless the operator configures it to. Operators enabling it are responsible for the provider's jurisdiction, access policy, and encryption settings. Note that the SQLite index, the job queue and the **HTTP audit log** remain local regardless of this setting. A separate mechanism also carries the name "audit": the opt-in audit/dedup pass (`[audit] enabled`, off by default) writes its reports through the same `[storage]` backend as the vault, so when both `service = "s3"` and `[audit] enabled = true` are set, those reports follow the vault to the object store instead of staying local. They contain note titles, identifiers, categories and sections — not note bodies.
- **`body_text` is indexed in `index.db`** (note content indexed for full-text search). This is in addition to the on-disk `.md` files.
- **`author` is derived from the authenticated credential, not client-supplied.** Since v2.0.0, a write sets `author` to the resolved agent identity (the credential's `sub`) at write time; a request that supplies its own `author` is rejected (`400`, identity is not self-declared), and a request with no identity resolved at all is rejected too (`401`) — a note can never end up attributed to nobody. Persisted in `index.db` and in note frontmatter.
- **`forgotten_by` is a free-form string field** persisted in `index.db` (column `forgotten_by`), in note frontmatter, and returned verbatim in `GET /api/v1/vault/forgotten` responses. It records the actor identifier that triggered a forget operation. No identity verification is performed; operators should treat this field as potentially containing PII (e.g. usernames, email addresses) and apply the same data-handling policy as for the `author` field.
- **`identity/person/*` person cards (v2.3.0) are the vault's dedicated place for data about people, and the only note kind carved out of the identity section's usual protection so it can be forgotten or hard-deleted like an ordinary note** — an agent soul (`identity/<sub>`) stays protected from both `forget` and hard delete unconditionally; a person card is not, by a path-based exception (the server resolves the note's own stored title, never a client-declared field, so the exception cannot be spoofed by mislabeling a soul as a person card or vice versa).
  - **Write and read both require the dedicated `identity_write` privilege** (the same scope described under *Agent identity security* below). The ordinary "an agent may read/write only its own identity note" self-service path can never apply here: `person/` is a reserved namespace that no authenticated principal may ever hold as its own credential subject (enforced at API-key creation), so a caller can never *be* the person a card describes.
  - **The schema is a strict field whitelist** (`display_name`, `pronouns`, `category`, `collaboration_prefs`, `working_style`, `context`, `timezone`, `locale`). Unbounded free-text fields (`notes`, `about`, `bio`, `description`, …) are refused outright, and any field whose *key* names a GDPR Article 9 special category (health, biometric, genetic, religious or political belief, sexual orientation, ethnic origin, trade-union membership, disability) is refused regardless of the card's `category`. **Detection is key-based only** — sensitive content typed inside the *value* of an otherwise whitelisted field (e.g. `context: recovering from surgery`) is not caught. This is a stated limitation of the current filter, not an oversight — see *Known limitations* below.
  - **Only two of the four structural categories are open to writes in this release**: `self` (the operator's own profile) and `private` (relatives/close peers, created by operator proxy). `public-figure` and `domain-actor` are refused until a dedicated review opens them.
  - **A person card is never injected into an agent's boot instructions.** The injection path resolves strictly by the caller's own credential subject, and — for the same reason writes and reads are restricted above — no credential can ever carry a `person/` subject, so a person card can never be the note selected there.
- **The `personal-classified` locus control changed shape in 2.4.0, and never protected what its name suggested before then.** Up to and including v2.3.1, a dedicated per-consumer flag denied any locus whose path *contained* the string `personal-classified` anywhere. But no product code path ever produced such a locus — a locus is always `{vault}/{section}`, and no section is named `personal-classified` — so the check never fired: the control was inert. The documentation nonetheless described it as an active protection. 2.4.0 removes the flag and expresses the same intent as a preset read/write pattern: a `!personal-classified` negation matched on a whole locus *segment*, not a substring, so `personal-classified-2026` and `my-personal-classified` are no longer treated as matches (a vault literally named `personal-classified` still is, via `!personal-classified/**`). This is a locus-level access control only. Hiding sensitive *content* that lives inside an otherwise-readable note is a separate, data-level filtering concern — not a property of the locus ACL, and not closed by it. **The underlying engine is deny-by-default on read patterns**: negations subtract from an allow set but never form one, so a consumer whose `read_patterns` hold only negations (or none at all) matches no locus and reads nothing — every migrated preset needs at least one explicit positive pattern per consumer that must read anything.
- **Note history retention is configurable.** The Copy-on-Write history store (`.history/<note-id>/`) applies a count cap (`[history] max_versions`, default 50) and an optional TTL (`[history] ttl_days`, default: no expiry). Both are read from `<vault_root>/.gradatum/config.toml` (with the default `storage.root`: `/var/lib/gradatum/vault/.gradatum/config.toml`). There is no `gradatum.toml` — setting these keys in any other file has no effect and silently leaves the defaults in place. A note's history is removed when the note is deleted.
- **LLM backend locality is not enforced.** The gateway does not enforce data locality. Routing to remote endpoints is possible depending on configuration. Configuring a cloud-hosted LLM backend (e.g. Anthropic, OpenAI) will cause note content to be sent to that provider. Operators are responsible for ensuring their backend configuration matches their data residency requirements. Setting `vision_capable = true` on a gateway alias additionally routes image content (base64-encoded) to that backend. **The curator pipeline is an additional egress path:** when `[curator.llm]` is configured with a non-heuristic backend, note body content is sent to that LLM endpoint (local or cloud, depending on the `backend` field) during classification. The *compiled fallback* is heuristic (offline; no network egress) — but that is not what a fresh install runs. `gradatum-admin init` (`crates/gradatum-admin/src/init.rs`, `generate_server_toml_template`) emits a `[curator]` section with `backend = "openai_compat"` and a `[curator.llm]` block with `base_url = "http://localhost:8000"`. Every install created through `init` — which includes the Docker path — therefore curates through that endpoint by default, sending note body content to it. Out of the box that endpoint is a loopback chat server (`llama-chat`, no internet egress), but it is an LLM egress path, not the offline heuristic mode. To keep classification offline, set `[curator] backend = "heuristic"` (or omit the `[curator.llm]` section) so the worker curates in pure heuristic mode and never calls the model.
- **CORS is disabled by default — which is not the same as rejecting cross-origin requests.** The gateway's `allowed_origins` defaults to an empty list; in that case **no CORS layer is mounted and no CORS response header is emitted**. The server does not inspect the `Origin` header and does not reject anything: what blocks a cross-origin *browser* caller is the browser's own same-origin policy refusing to hand the response to the page. A non-browser client (curl, an SDK, a script) is unaffected and reaches every endpoint normally — CORS is not an access control. Setting `allowed_origins = ["*"]` is accepted but discouraged in production environments with sensitive data.
- **No telemetry is collected by Gradatum.** The server does not transmit usage data, metrics, or note content to any external endpoint operated by the Gradatum project.
- **`GET /health` is unauthenticated by design and discloses operational metadata to any caller.** It returns `version`, `build_sha` (commit identifier), `tenant_count`, and `locus_count`, alongside `status`, `uptime_secs`, queue depth/age, and WAL size — no note content or PII, but enough to fingerprint a deployment from the network. See `crates/gradatum-server/src/health.rs`.
- **`event_log` table (`index.db`)** records LLM gateway calls. Fields persisted: `route`, `model_alias`, `provider`, `feature_id` (nullable), `status_code`, `latency_ms`, `outcome` (nullable), `agent_id` (nullable — client-supplied `X-Agent-Id` request header, not a server-verified identity). Retention: 30 days by default (configurable via `[event_log] retention_days`). No prompt or response content is stored.
- **`session_trace` table (`index.db`)** records session-log Tier 1 entries (agent action tracing). Fields persisted: `session_id`, `agent_id` (JWT `sub` — stable agent identifier, server-assigned), `tenant_id` (from JWT), `ts_ms`, `action_type`, `target` (nullable, ≤ 512 chars), `intent` (nullable, ≤ 200 chars), `outcome` (nullable), `marker` (nullable), `ref` (nullable). Retention: 90 days by default (configurable via `[session_trace] retention_days`).
- **`note_usage` table (`index.db`)** records per-note usage telemetry (salience signal, F-110): a row keyed `(tenant_id, note_id, kind)` accumulates `count` and `last_used_ms` for every read, search hit and recall event on a note, where `kind` is one of `read`, `search-hit`, `search-hit-top3`, `recall-surfaced`, `recall-accepted`. **No retention or purge is wired for this table** — unlike `event_log` and `session_trace` above, which both run a scheduled retention task, `note_usage` has none; the migration's own comment defers the question ("le GC des lignes orphelines est un non-problème Phase 1"). There is no foreign key to the note, so rows are not tied to a note's lifecycle: **`note_usage` outlives both `forget` and `delete`** — a forgotten or deleted note's usage history remains in `index.db` indefinitely.
- **HTTP audit log — active by default, with no retention.** The server wires `JsonlFileSink` at boot and writes `<storage.root>/audit/audit.YYYY-MM-DD.jsonl` (mode `0640`, daily rotation on UTC date, flushed per event) for every audited operation. Each record carries `ts`, `event`, `actor` (JWT `kid`/`sub`/`aud`/`jti`), `tenant_id`, `locus`, `note_id`, `content_hash`, `outcome`, and `request_id`. **A `vault_delete` additionally records the full note body** as a recovery tombstone (`section`, `title`, `body`, `deleted_by`), so deleted content persists in the audit log even after the archive retention window has destroyed the archive itself. **No rotation-based pruning or garbage collection is applied to these files**: the `[audit]` block (`rotation`, `retention_days`, `strict_mode`) — which lives in `<vault_root>/.gradatum/config.toml`, not in `server.toml` — is defined but not yet wired, so audit files accumulate indefinitely. Operators must prune `<storage.root>/audit` themselves and treat its contents as holding note bodies and actor identifiers. Automatic audit retention is planned for a future release.
- **`forget` is a relevance signal, not an erasure.** `vault_forget` sets `forgotten = true` (plus `forgotten_at` / `forgotten_by`) in the note frontmatter and index. It performs no physical deletion: the note body remains in plaintext in the `.md` file, in `index.db` (`body_text`), in `.history/`, in the embedding index, and in the job queue (`db/queue.sqlite`, `gradatum_jobs.payload`). No background job removes forgotten notes — the purge job only targets notes in `Garbage` status. For actual removal, use the operator delete path (`gradatum-admin delete`), which archives the note and destroys it after the archive retention window — while leaving the audit tombstone described above.
- **Delete is archival, not destruction.** An on-demand delete moves the note's `.md` and `.history/` under `<vault_root>/.archive/` — that is `<storage.root>/vault/.archive/`, not `<storage.root>/.archive/` — in a mirror layout, and records a row in `archive_index`. Archived content stays in plaintext on the local filesystem until the retention deadline (`60` days by default, configurable) is reached and the retention GC physically destroys it.
- **The embedding pipeline is a third egress path.** When `[embed] enabled = true` (the default), note text is POSTed to `[embed] endpoint` — `http://localhost:8436/v1/embeddings` by default, i.e. loopback, no internet egress out of the box. The endpoint accepts any OpenAI-compatible URL: pointing it at a hosted embedding API sends note content to that provider.
- **Note bodies are also persisted in the job queue.** `vault_write` and `validate` job specs carry the full Markdown body (`JobSpec.body`, `ValidateSpec.body`), serialised into `<storage.root>/db/queue.sqlite` — in the live `gradatum_jobs.payload` column, which is **JSON text, not an opaque blob** (the legacy `jobs.payload_json` column — `LegacyQueue`'s rusqlite-backed table, kept for backward compatibility — carries the same field; the older `jobs_v2` table that once duplicated it was dropped, `DROP TABLE IF EXISTS jobs_v2`, since nothing regenerates it and its payloads outlived every vault-side deletion). This copy is a second plaintext location for note content, on a file distinct from `index.db` and from the `.md` files. It is reached by neither `forget`, nor `delete`, nor the archive retention GC. **Completed jobs keep their payloads until the queue's own purge reaches them — and that purge is a schedule, not a write-time step.** The queue opens with `secure_delete=ON`, and `QueueStore::purge_terminal_payloads` rewrites the payload of every terminal row (`Done`, `DLQ`, `Cancelled`, `Conflict`) in bounded batches of 500, leaving the `TERMINAL_PAYLOAD_KEEP` (200) most recent terminal rows untouched: a body stays replayable for the last 200 terminal jobs and not after, the purged row keeping a `Job::PayloadPurged` sentinel and a `payload_purged` flag where its spec was. Two callers reach it: a scheduled tick on the process that owns the queue write connection (the worker), every `[queue] purge_interval_secs` (default 3 600 s, floored at 60 s) and only for rows older than `[queue] purge_after_secs` (default 86 400 s); and an operator command, `gradatum-admin queue purge [--dry-run] [--older-than <N>h|<N>d]`, whose dry-run reads its candidates with the very statement the purge selects them with. The purge is physical where it reaches — `secure_delete=ON` zeroises the freed space, whole freed overflow pages included, and a truncating write-ahead-log checkpoint drops the page images taken before the rewrite — but it does **not** run `VACUUM`; the upgrade guide places that at deployment. A separate GC deletes `DLQ` *rows* older than 30 days; that is row removal, not the scrubbing above, and the two do not substitute for one another. **A masked note's clear body survives in `queue.sqlite` until the purge runs**: the masking write rewrites the note and never the job that carried it, so the window is bounded by `purge_after_secs` (24 h by default) and is set by the queue's schedule, not by the switch to `mask`. Operators must include this file in the same data-handling policy as the vault itself.
- **Embeddings derive from note content.** Vectors computed from note bodies are persisted in `index.db` (`note_embeddings`) and in the ANN index. They are not directly readable, but embedding inversion is a published attack class and irreversibility is not a property to rely on: treat them as content-derived data, under the same policy as the note bodies. They outlive a `forget`.

---

## Agent identity security (`identity` section)

Gradatum ships a protected `identity` section (13th canonical section, introduced in v0.7.6) for storing per-agent soul notes — structured Markdown documents that describe an agent's behavioral invariants, guidelines, and policies.

### Access model

- **Enumeration is hidden**: `identity` notes are excluded from search, list, by-status, review, trace, graph, and link surfaces for non-privileged callers. The filter (`identity_section_hidden`) and the read/trace guards are applied per-handler across the full HTTP and MCP surface — there is no single gateway guard that could be bypassed.
- **The timeline surface is stricter: it excludes `identity` for *every* caller, privileged ones included.** `vault_timeline` filters in SQL on `Section::PROTECTED_FORGET`, which contains `Identity`; no privilege check is consulted, so no soul is reachable through the timeline at all. This is a blackout, not a per-caller filter.
- **Privileged caller (as of 2.0.0): the dedicated `identity_write` scope, not an agent name.** Any bearer whose key carries the `identity_write` scope (`IDENTITY_WRITE_SCOPE`, `crates/gradatum-acl-auth/src/api_key.rs`) — or a Studio session — bypasses the per-agent match on every identity surface that exposes souls at all, and may enumerate, read, and write **any** agent's identity note. This is deliberate: whichever credential provisions and repairs the souls of the agents it supervises is meant to hold a distinct, narrowly-scoped grant, not gain that power as a side effect of `admin`. **The scope is deliberately disjoint from `admin`, `write` and `service`**: a key declared "full vault" through `admin` does not silently inherit soul-write, and symmetrically a key bearing only `identity_write` grants no ordinary write access — it must be combined with a write scope to write anything besides a soul note. The consequence must be stated plainly — **the `identity` section isolates agents from each other, not from whichever credential holds `identity_write`**. Treat any credential carrying that scope with the same care as an admin credential: grant it narrowly, store it chmod-600, and rotate it on suspicion.
- **At the default `multi_tenant.enabled = false`, the tenant dimension is closed rather than merely unpartitioned.** No credential can carry a tenant other than `main`, and three independent guards enforce it: API-key creation rejects any other tenant outright, `/auth/exchange` refuses to mint a JWT for such a key, and the authentication middleware rejects any such context with `403` **before** it reaches a handler. The privileged bearer's authority therefore spans the only tenant that can exist in that mode.
- **A second trust tier, `TrustContext::Studio`, is defined but never constructed in `2.x`.** The authentication middleware produces only `BearerToken` or `Unauthenticated`; every `Studio` value in the tree is built inside a test. Several code paths still branch on it, which is why it is named here — but it grants no access, because nothing issues it.
- **`vault_search` with `section=identity`**: returns results only to the privileged caller. Everyone else receives an empty result set (fail-closed — not a 403, to avoid oracle attacks on identity note existence).
- **Read access**: a non-privileged caller may only read their own identity note. The target agent is parsed server-side from the note's own `identity/<agent>` title, never taken from request input, and compared to the JWT `sub`. Reading another agent's note is rejected with `403` and emits an `identity_read_denied_foreign_agent` audit record.
- **Write access**: a non-privileged caller may only write or update their own identity note, under the same server-side title-versus-`sub` comparison. Cross-agent writes are rejected with `403` and emit an `identity_write_denied_foreign_agent` audit record.
- **Title and section hint must agree**: a write whose title starts with `identity/` while `section_hint` is anything other than `identity` is rejected with `400` (audit record `rejected_400_identity_title_without_hint`) before the body is processed. This closes the direct bypass — reaching the `identity/` title space through a different section hint, where the ownership comparison would not run.
- **The ownership predicate has two implementations.** `vault_read_impl` and `vault_write_impl` each carry an inline copy; `enforce_identity_read_guard` and `enforce_identity_write_guard` carry the other, applied to the secondary surfaces (`vault_trace`, `vault_classify`, `vault_history`, `vault_history_get`, `vault_restore`, `vault_diff`, `vault_downgrade`, `patch_note`, `move_note_locus`). Two copies are two places that can diverge, and they already do: the extracted guards recognise `TrustContext::Studio`, the inline copy in `vault_write_impl` does not. The divergence is inert today only because `Studio` is never constructed. The two implementations are not unified in `2.x`.
- **`doc_kind` is not an access control.** No code path forces a `doc_kind` at write time. Identity notes end up classified `Static` through a section-to-`doc_kind` mapping applied at indexing; that is a classification of the temporal axis, and nothing about it is enforced on the write path.
- **Soul validation**: the note body must pass a structural schema check at write time, and a body that fails it is rejected at the write endpoint with `400`.
  - A **root** soul (no `extends:`) requires all three sections `INVARIANTS`, `GATES` and `NARRATIVE`, **and** a line inside `INVARIANTS` that literally begins with `INV-CANARY`. The match is anchored to the start of the line, so mentioning the token in prose or in a comment does not satisfy it. This requirement is easy to miss when authoring a first soul, and it is the most likely cause of an otherwise puzzling `400`.
  - A **child** soul — one whose first non-empty line after the H1 is `extends:` — requires only `NARRATIVE`. `INVARIANTS`, `GATES` and the `INV-CANARY` line are inherited from the parent and are not required of the child. `extends:` detection is bounded to that first line, so the directive appearing later in prose does not turn a root soul into a child.
  - In **both** cases the body must be byte-stable: a line starting with `updated_at:` or `version:` is rejected. An optional `scope` field on an `INVARIANTS` line is accepted and ignored.
- **Worker reclassification guard**: the background curator treats identity notes as immutable — reclassification to a different section is a no-op, preventing the curator from overriding the section on re-ingest.

### MCP identity injection

On MCP `initialize`, the server injects an identity note body into the `instructions` field of the `initialize` response.

**As of 2.0.0, the target soul is selected by the credential alone — no client input contributes to the selection.** The server reads the subject (`sub`) carried by the trust context — the JWT claim or API-key owner, a server-side value produced by argon2id verification or JWT signature verification — and loads `identity/<sub>`. The `X-Gradatum-Agent` request header that previously let a client declare which agent's soul to receive has been removed and is no longer read at all (`crates/gradatum-server/src/api_v1/mcp.rs`). An unauthenticated caller, or a trust tier that carries no subject (`Studio` / `Mtls`), resolves to **no soul** — the service never falls back to a default identity (R2, fail-closed).

If the note does not exist, the caller is unauthenticated, or the read fails for any other reason, `instructions` is simply absent (graceful degradation — no error, and no signal distinguishing those cases).

Both properties that used to be stated separately now coincide:

- The **content** of `instructions` cannot be forged by a client. It is read verbatim from the vault; no request field contributes to it.
- The **selection** of that content cannot be influenced by a client either — it is the credential's own subject, not a request parameter. An operator who reads the injected soul as proof of *which* agent is connected is relying on a property the server now actually provides, which was not true before 2.0.0.

### Scope

Identity section security is enforced at the application layer only — it does not provide cryptographic confidentiality at rest. Filesystem-level encryption (LUKS or equivalent) remains the operator's responsibility for protecting stored notes against direct file access.

---

## Known limitations

These are properties of `2.x` that an operator must account for before handling sensitive data. They are stated here because the product does not enforce them.

- **No general-purpose PII redaction is applied by default.** Gradatum does not mask or redact personal data in ordinary note content at write time unless an operator has both enabled the frontier and selected the destroying mode. Since 2.4.0 a write-time personal-data *detection* pass exists for the vault at large — the F-160 marking mechanism, configured under a `[privacy]` block — and its delivered default is `report_only`: it only **marks** a note (it adds the `personal-classified` tag and a non-authoritative report block) and moves, masks or removes nothing. The other mode, `mask`, **rewrites** every detected span in place with `[redacted]`, and **no copy of the replaced text is kept by the application**: a write that masks anything destroys that note's whole version history (`.history/<id>/`) instead of snapshotting the version it replaced. That is an overwrite through the ordinary write path — an OpenDAL write over the `.md`, an upsert over `index.db` — and **it is not a secure erase**. "Irreversible" describes what gradatum will ever show again, never what the storage still holds: the bytes of the replaced text remain in the free space of the filesystem and of the database file until they are reused, and remain readable to anyone with filesystem or forensic access to either — a copy-on-write dataset (a ZFS snapshot, a VM image) or a backup taken before the write keeps them for as long as that copy exists. An operator whose threat model includes recovering the medium must destroy those copies; the rewrite will not. Two things bound it before an operator ever reaches it: it loads only over an activation that can mask something (a `mask` whose activated categories all sit at the observation point is refused at launch rather than read as a running redaction that rewrites nothing), and on a fresh install `categories = []`, so nothing is scanned until an operator opts in. Nothing reaches material written **before** the switch — the snapshots older than the first masking write are removed only by `gradatum-admin privacy history-purge`, run once by the operator, as described under *Personal-data marking (F-160)* below. The `mask` mode rewrites the note and nothing else, and most of the places a body is persisted then receive the rewritten one rather than the original: `masked_body()` runs **before** the note is constructed, so the `.md` file and `index.db` (`body_text`, and the FTS5 index built over it) are written from the **already masked** body — neither of them holds the clear text of a masked span. The embedding index is not written on that path at all: it is refreshed asynchronously by the `Curate → Embed` cascade, so it is *eventually* consistent with the rewrite, and between the masking write and the embedding job a vector of the clear text is still what a semantic search can reach. What the rewrite does not reach, and what therefore survives it, is this list and no longer: **`.history/`**, for the snapshots older than the switch to `mask` — the residue `privacy history-purge` exists to destroy; **`.archive/`**, since an archived note is not rewritten; the **HTTP audit journal**, which carries bodies of its own; and **`queue.sqlite`** — see *Note bodies are also persisted in the job queue* above. **Since v2.3.0, one narrow, purpose-built exception exists**: `identity/person/*` person cards (see *Privacy posture* above) are the only note kind subject to a write-time content filter — a strict field whitelist plus a refusal of any field whose *key* names a GDPR Article 9 special category. That filter is key-based only: sensitive content typed inside the *value* of an otherwise whitelisted field is not caught, and it applies to person cards alone, not to the vault at large. General content-level *redaction* of the vault at large is therefore **delivered but off**: it is absent from a default deployment, and the one exception above is independent of it — the F-160 pass does not consult person cards, and the person-card filter does not apply to the vault at large. What the F-160 marking pass does and does not protect is stated in *Personal-data marking (F-160)* below. Operators handling regulated data must filter upstream of ingestion.
- **Data at rest is not encrypted**, and the application-layer guards (ACL, identity guards, section hiding) offer no protection against direct filesystem access. Filesystem-level encryption is the operator's responsibility.
- **Deletion is not immediate erasure.** `forget` erases nothing; `delete` archives for a retention window; the audit tombstone of a deleted note is never pruned. See *Privacy posture* above for the full picture.
- **Revoking an API key does not cut outstanding tokens.** See the revocation caveat under *Hardening defaults*.
- **Per-key write scope is not enforced outside multi-tenant mode.** With `multi_tenant.enabled = false` (the default), a key's scopes are recorded but not checked on the request path. With it on, a write requires the key to carry one of `write`, `admin`, `service`. **Read access is never governed by key scopes, in either mode, for the ordinary vault** — it is governed by vault grants and the locus ACL. The `identity` section is the one exception: reading another agent's soul note requires the `identity_write` scope in both tenancy modes (see *Agent identity security* above); a caller may always read their own soul regardless of scopes.

### Personal-data marking (F-160)

2.4.0 adds a write-time personal-data detection pass — "the marking pass". It is a defensive marker, not a redaction engine, and its guarantees are narrow. State them precisely before relying on it.

- **`report_only`, the default, deletes and moves no data.** In `report_only` the pass records its verdict — the `personal-classified` tag plus a report block in the note's `extra` — and touches nothing else. Every prior copy of the note body stays where it was, in plaintext: the `.md` file, `index.db` (`body_text`), the note's `notes.extra_json`, `.history/`, `.archive/`, the HTTP audit journal (including the full-body `vault_delete` recovery tombstone), the embedding index, `queue.sqlite` and its **14 unencrypted NFS archives**, and — under `[storage] service = "s3"` — the object store. Marking a note reaches back into none of these.
- **`mask` mode is delivered, off by default, and the refusal is on the activation — never on the mode.** In-place redaction of detected spans (`mode = "mask"`) rewrites each marked span to `[redacted]`. It loads only over an activation with at least one category whose threshold is strictly above zero: a `mask` whose activated categories all sit at the observation point would read as a running redaction while rewriting nothing, and a corpus where nothing matched cannot tell that deployment from a working one, so it is **refused at launch** rather than left for an operator to notice. A `report_only` over the same uncalibrated categories still loads, because observing is what that mode does. Do not read the presence of the `mask` value in the schema as a protection in force: the mode is inert until an operator selects it **and** calibrates an activation beneath it, and `categories = []` (a fresh install) selects neither.
- **A masking write destroys the note's whole version history — and reaches nothing older.** The write path removes **every** snapshot under `.history/<id>/`, not only the snapshot of the version it replaced: the versions before it hold the very text the mode exists to remove, in the one directory the rewrite does not touch, and a note whose history still describes what was removed is not masked. The destruction is recorded as a `WARN` line carrying the note identifier, the number of snapshots destroyed and the number that resisted removal (`snapshots_destroyed`, `snapshots_failed`) — the counts, never their text. Both counts are there because one does not describe the purge: a snapshot the filesystem kept is not a snapshot that was destroyed, and a report naming only the second would describe a history that was emptied when it was not. A snapshot that resists removal does **not** fail the write — the masking rewrite lands, and the surviving snapshot is named by the count rather than by an error, which is why the two figures are read together. What that does **not** reach is material written before the switch: snapshots taken while the deployment ran `report_only` carry the clear text of notes an operator has since decided to rewrite, and no later write touches them, because a masking write destroys only the history of the note it is rewriting. `gradatum-admin privacy history-purge --root <root> [--dry-run]` is the one-shot cleanup for that residue: it selects the notes whose privacy block records `mode = "mask"` — durable on-disk evidence, readable without the index or the database — removes their `.history/` snapshots, and reports notes, snapshots and bytes; `--dry-run` reports the same count and removes nothing. It never touches a note the block does not identify as masked, and a note whose `.md` it cannot read is counted separately rather than passed over in silence. **It is not an erasure**, and it does not finish the job on its own: the command removes markdown snapshots and nothing else. A pre-switch copy of the same body can also sit in `.archive/`, in the HTTP audit journal, and in `queue.sqlite` as the payload of the write job that carried it — none of which the command reaches — and the embedding index is refreshed asynchronously, so a vector of the clear text survives until the `Curate → Embed` cascade catches up.
- **Under `mask`, a detection pass that could not complete refuses the write.** An incomplete pass (`detector_unavailable`) returns an error and **no note is written**, because the write path cannot tell which passages it would have destroyed. Under `report_only` the long-standing fail-open behaviour is unchanged: the write lands and the note is marked unscanned, which is what lets the deferred sweep pay the debt later.
- **The false-negative rate is not guaranteed.** The delivered detector is a regex floor; it will miss personal data it was not written to recognise. The marking pass is **a line of defense, not a guarantee** — treat an unmarked note as unproven, never as cleared.
- **The `personal-classified` tag designates a note; it does not govern access.** The tag governs no access control, before or after F-297 — the ACL circuit-breaker evaluates on the **locus**, never on the note's tag. What F-160 closes is the **enumeration** by the API under ACL (D-14(b)); it protects neither **disk access**, nor an **authorized** consumer that reads a note already accessible to it. The enumeration guard covers the note-listing surfaces but **not the archive listing yet**: the archive registry carries no tag, so `vault_archives_list` cannot filter on `personal-classified` — this is inert while `categories = []` and becomes relevant only on activation, and is tracked for 2.5.0.
- **F-160 adds no in-process FFI edge (the "zero C" property as stated in D-4).** The marking pass and its regex detector are pure Rust; F-160 introduces no new C or C++ code reachable in-process. This is *not* a claim that the product is zero-C: it already embeds `rusqlite` (bundled SQLite, a C library) and already spawns `llama-server` (C++) as separate processes. F-160 simply adds none of its own.
- **What an operator should expect when the pass runs (release-facing consequences).** These follow the closure list above and are the part that surfaces to a user:
  - Writing the report block **changes the note's `ContentHash`** — the block lives in `extra`.
  - The day a category is **activated**, agent search **loses the notes the pass then marks** `personal-classified` (the read guard hides them from enumeration).
  - The `queue.sqlite` purge is logical, not physical: it does **not** run `VACUUM`, and it purges neither `retry.last_error` nor `retry.errors[].message`.
  - A fresh install ships **`categories = []`** — the pass scans nothing until an operator opts in.
  - A **metadata-only mutation still triggers a full personal-data pass** over the note body.
- **No detection sidecar is mounted: the shipped detectors are in-process.** The mounting rule has two branches and no third — `[privacy]` absent or `categories` empty mounts a no-op pass-through, a non-empty `categories` mounts the in-process regex floor. There is no sidecar process, no sidecar socket, and no `sidecar` field in `[privacy]`. When the sidecar lands, its unit is to run under a **dedicated user**, with `PrivateNetwork=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, `NoNewPrivileges=yes`, a bounded `MemoryMax`, and a Unix socket at mode `0600` under `/run/gradatum/`; it is `preload = true` and `idle_stop_s = 0` — **resident is the only supported mode** — and it declares its build fingerprints. Stated in full: those handshake fingerprints attest against a **configuration drift**, **never against an impostor**. Peer authentication is to be carried by `stat` on the socket plus `SO_PEERCRED` — **and that path is not delivered either**. Nothing in the shipped server authenticates a sidecar peer; the point is moot only because no sidecar is mounted. Do not read the fingerprint check, once it ships, as peer authentication.
- **The third door — posting a reserved tag through deserialization — stays shut at the API and open at the disk.** A `Tag`'s `#[serde(transparent)]` deserialization lets a tag be **read back**, never **posted**, through an HTTP surface: measured, no `gradatum-dto` DTO deserializes a `Tag` (`pub tags: Vec<String>` everywhere), no DTO carries a `Frontmatter`, no surface accepts a raw `.md`, and a body beginning with `---` lands in the body, not the frontmatter (`crates/gradatum-markdown/src/writer.rs`, `parser.rs`). **But anyone with filesystem access to the vault can write the tag directly** — that is outside the API threat model and already implies control of the host. Stated here rather than left silent.

### Registry-state disclosure via the initialization 503 (2.0.0, multi-tenant only)

From 2.0.0, an initialised multi-tenant server answers an **unauthenticated** request differently
depending on whether the key registry holds any active key: **503** with an actionable
"uninitialised registry" body when it is empty, **401** when it is populated but the credential is
rejected. The 503 is therefore an information disclosure: it reveals the installation's credential
state to a caller who has presented no credential, and it is **indistinguishable** from the 503 that
follows a `gradatum-admin api-key reset`. A local-network observer who can reach the endpoint can
thus infer that a registry wipe (fresh install or reset) has just happened.

**This trade-off is accepted for its diagnostic value.** The body names the bootstrap identity
(`main-agent`) and the exact `api-key create` command, turning a dead-end 401 into a recovery
instruction — the operator who lost every key learns, from an unauthenticated probe, precisely how to
recover. The disclosure is limited to a binary "any active key present?" and the recovery text.

**The guarantee that holds:** the 503 body never carries the registry's absolute disk path. It is
built by `registry_uninitialised` in `crates/gradatum-server/src/middleware.rs`, whose response
string contains only the refusal, the bootstrap identity, and the create command — no filesystem
location. This keeps the disclosure to the installation *state*, not its *layout*.

**Measured scope — the discrimination only operates when `multi_tenant.enabled = true`.** In the
default legacy mode, the two rejection points that can produce the 503 (both `reject_unauthenticated`
call sites in `auth_middleware`, on the api-key and JWT failure paths) are **not reached** for an
unauthenticated context: `tenant_is_authorized_legacy` returns `true` for
`TrustContext::Unauthenticated`, so the request passes through and the downstream handler answers a
plain **401** with no state signal. A **fresh installation is legacy** — `gradatum-admin init` does
not emit a `[multi_tenant]` section, and the default is off. The real audience of the 503 message is
therefore **recovery after a `reset` on a multi-tenant server**, not the first `init`; and it is also
only on such a server that the reset-vs-empty observation above is possible.

---

## Supply chain

- Two advisory gates run in CI, reading two different views of the dependency
  tree, and are kept deliberately distinct rather than merged into one:
  `cargo deny check` walks the **resolved** graph (what actually compiles);
  `cargo audit` reads `Cargo.lock` **flat** (every crate the lockfile records,
  including ones gated behind a feature nobody enables). Both run on every
  PR, on every push to `main`, and **daily**, blocking (`fail-on-finding`, no
  `continue-on-error`).
  - `deny.toml` carries **one** exception: RUSTSEC-2025-0141 (`bincode` v2,
    unmaintained). RUSTSEC-2026-0194 and RUSTSEC-2026-0195 (`quick-xml` DoS,
    reached via `opendal`) needed one until `opendal` was bumped
    0.51.0 → 0.58.1, which pulls `quick-xml` ≥ 0.41.0 and fixes both — they
    are no longer in `Cargo.lock` at all and carry no exemption.
    RUSTSEC-2023-0071 (`rsa` Marvin attack) is deliberately **not** in this
    file: `rsa` never enters the resolved graph, so the gate would fail on
    its own should that path ever open up.
  - `.cargo/audit.toml` carries **one** exception, scoped to `cargo audit`
    only: RUSTSEC-2023-0071 (`rsa`). `cargo audit` sees `rsa` in the flat
    lockfile regardless of feature selection, with no fix available upstream
    (`patched = []` since 2023-11); this exemption silences the tool that
    looks at the wrong scope for this advisory, not the underlying
    detection — `cargo deny` remains armed for it on the resolved graph.
  RUSTSEC-2025-0068 (`serde_yml` emitter unsound) was exempted until `1.0.0`;
  the exemption is gone because the cause is: the YAML backend is now
  `serde_norway`, and `serde_yml` together with its `libyml` backend
  (RUSTSEC-2025-0067, never covered by any exemption) both left the
  resolved graph.
- All dependencies are pinned with `=` for critical workspace deps.
- **A CycloneDX SBOM ships with tagged releases — on the GitHub release path only.** On a `refs/tags/v*` push, `.github/workflows/release.yml` emits one CycloneDX 1.5 JSON document per publishable crate, packages them as `gradatum-sbom-<TAG>.tar.gz`, covers that tarball in `SHA256SUMS`, attests its build provenance (SLSA, `actions/attest-build-provenance`), and attaches it to the GitHub Release. **`.forgejo/workflows/release.yml` generates no SBOM**: a release cut only there ships binaries and checksums without one. A separate nightly-cron job in `.forgejo/workflows/ci.yml` produces an unfiltered SBOM as a 90-day build artifact for internal dependency-drift scanning; it is not published and is not the release deliverable.
- Every artefact attached to a release is covered by a published `SHA256SUMS`.

---

## Acknowledgements

A `SECURITY-HALL-OF-FAME.md` will be published once the first vulnerability is resolved. Reporters are credited with their preferred handle (or kept anonymous on request).
