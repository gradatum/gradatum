# SECURITY.md

> Vulnerability disclosure policy for Gradatum.

---

## Reporting a vulnerability

**Do not open a public issue for security vulnerabilities.**

Send a report to `security@gradatum.org` (PGP key fingerprint published on `gradatum.org/security` once the public release ships; until then, reports may be sent in plain text and will be triaged with the same urgency).

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

**Current supported version** : `v1.0.0` (latest stable). All `0.x` tags are end-of-life and
receive no backports.

| Version | Status | Security fixes |
|---|---|---|
| `1.x` LTS branch | Supported | Yes — for the LTS lifetime declared at `1.0` release. |
| `1.x` main | Supported | Yes — fixes ported from LTS. |
| `0.x` | End-of-life at `1.0` | No backports. |

The exact LTS lifetime (e.g. 18 or 24 months) is decided in the `1.0` release RFC.

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
  - **Revoking an API key does not invalidate JWTs already issued from it.** `gradatum-admin api-key revoke` marks the key as revoked and stops it being exchanged for new tokens. It does not write to the revocation store, and the JWT verification path does not re-read the state of the originating key. A token issued before the revocation therefore stays valid until its `exp` — up to **1 h** for `human` scope and **24 h** for service/machine scope. Server-side token revocation is **planned for a `1.x` release**.
    **Emergency procedure**: to cut every outstanding token immediately, rotate the Ed25519 signing seed — remove `<storage.root>/config/jwt-signing-key.secret` (with the default `storage.root`: `/var/lib/gradatum/config/jwt-signing-key.secret`) and restart `gradatum-server`, which generates a new one and logs a `WARN` naming the path. This invalidates *all* tokens for *all* keys, so every consumer must re-exchange its API key. There is no way to invalidate a single outstanding token in `1.0.0`.
  - **Logout is client-side only**: the studio logout clears localStorage (`localStorage.removeItem('gradatum_studio_jwt_persist')`). A JWT stolen before logout remains valid until it expires (≤ 1 h for studio sessions), for the reason above.
  - **Rate-limit and reverse-proxy**: `ratelimit.exempt_localhost = true` by default. Set to `false` if gradatum runs behind a reverse-proxy (otherwise the loopback bypass disables rate-limiting on `/auth/exchange`).
  - **HSTS**: not emitted by default. Recommended when the operator enables TLS.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full security design and caveats.

---

## Privacy posture

_As of v1.0.0. Review this section on each minor release._

Gradatum is designed for local-first, self-hosted deployments. The following data handling properties hold in v1.0.0:

- **Data at rest is not encrypted.** The SQLite index (`index.db`), note frontmatter, note body text, and version history snapshots (`.history/`) are stored in plaintext on the local filesystem. Filesystem-level encryption (e.g. LUKS) is the operator's responsibility.
- **`body_text` is indexed in `index.db`** (note content indexed for full-text search). This is in addition to the on-disk `.md` files.
- **`author` is a free-form string field** persisted in `index.db` and in note frontmatter. No identity verification is performed; operators should document their author identifier convention.
- **`forgotten_by` is a free-form string field** persisted in `index.db` (column `forgotten_by`), in note frontmatter, and returned verbatim in `GET /api/v1/vault/forgotten` responses. It records the actor identifier that triggered a forget operation. No identity verification is performed; operators should treat this field as potentially containing PII (e.g. usernames, email addresses) and apply the same data-handling policy as for the `author` field.
- **Note history retention is configurable.** The Copy-on-Write history store (`.history/<note-id>/`) applies a count cap (`[history] max_versions`, default 50) and an optional TTL (`[history] ttl_days`, default: no expiry). Both are read from `<vault_root>/.gradatum/config.toml` (with the default `storage.root`: `/var/lib/gradatum/vault/.gradatum/config.toml`). There is no `gradatum.toml` — setting these keys in any other file has no effect and silently leaves the defaults in place. A note's history is removed when the note is deleted.
- **LLM backend locality is not enforced.** The gateway does not enforce data locality. Routing to remote endpoints is possible depending on configuration. Configuring a cloud-hosted LLM backend (e.g. Anthropic, OpenAI) will cause note content to be sent to that provider. Operators are responsible for ensuring their backend configuration matches their data residency requirements. Setting `vision_capable = true` on a gateway alias additionally routes image content (base64-encoded) to that backend. **The curator pipeline is an additional egress path:** when `[curator.llm]` is configured with a non-heuristic backend, note body content is sent to that LLM endpoint (local or cloud, depending on the `backend` field) during classification. The default backend is heuristic (offline; no network egress).
- **CORS is disabled by default — which is not the same as rejecting cross-origin requests.** The gateway's `allowed_origins` defaults to an empty list; in that case **no CORS layer is mounted and no CORS response header is emitted**. The server does not inspect the `Origin` header and does not reject anything: what blocks a cross-origin *browser* caller is the browser's own same-origin policy refusing to hand the response to the page. A non-browser client (curl, an SDK, a script) is unaffected and reaches every endpoint normally — CORS is not an access control. Setting `allowed_origins = ["*"]` is accepted but discouraged in production environments with sensitive data.
- **No telemetry is collected by Gradatum.** The server does not transmit usage data, metrics, or note content to any external endpoint operated by the Gradatum project.
- **`event_log` table (`index.db`)** records LLM gateway calls. Fields persisted: `route`, `model_alias`, `provider`, `feature_id` (nullable), `status_code`, `latency_ms`, `outcome` (nullable), `agent_id` (nullable — client-supplied `X-Agent-Id` request header, not a server-verified identity). Retention: 30 days by default (configurable via `[event_log] retention_days`). No prompt or response content is stored.
- **`session_trace` table (`index.db`)** records session-log Tier 1 entries (agent action tracing). Fields persisted: `session_id`, `agent_id` (JWT `sub` — stable agent identifier, server-assigned), `tenant_id` (from JWT), `ts_ms`, `action_type`, `target` (nullable, ≤ 512 chars), `intent` (nullable, ≤ 200 chars), `outcome` (nullable), `marker` (nullable), `ref` (nullable). Retention: 90 days by default (configurable via `[session_trace] retention_days`).
- **HTTP audit log — active by default, with no retention.** The server wires `JsonlFileSink` at boot and writes `<storage.root>/audit/audit.YYYY-MM-DD.jsonl` (mode `0640`, daily rotation on UTC date, flushed per event) for every audited operation. Each record carries `ts`, `event`, `actor` (JWT `kid`/`sub`/`aud`/`jti`), `tenant_id`, `locus`, `note_id`, `content_hash`, `outcome`, and `request_id`. **A `vault_delete` additionally records the full note body** as a recovery tombstone (`section`, `title`, `body`, `deleted_by`), so deleted content persists in the audit log even after the archive retention window has destroyed the archive itself. **No rotation-based pruning or garbage collection is applied to these files**: the `[audit]` block (`rotation`, `retention_days`, `strict_mode`) — which lives in `<vault_root>/.gradatum/config.toml`, not in `server.toml` — is defined but not yet wired, so audit files accumulate indefinitely. Operators must prune `<storage.root>/audit` themselves and treat its contents as holding note bodies and actor identifiers. Automatic audit retention is planned for a `1.x` release.
- **`forget` is a relevance signal, not an erasure.** `vault_forget` sets `forgotten = true` (plus `forgotten_at` / `forgotten_by`) in the note frontmatter and index. It performs no physical deletion: the note body remains in plaintext in the `.md` file, in `index.db` (`body_text`), in `.history/`, in the embedding index, and in the job queue (`db/queue.sqlite`, `gradatum_jobs.payload`). No background job removes forgotten notes — the purge job only targets notes in `Garbage` status. For actual removal, use the operator delete path (`gradatum-admin delete`), which archives the note and destroys it after the archive retention window — while leaving the audit tombstone described above.
- **Delete is archival, not destruction.** An on-demand delete moves the note's `.md` and `.history/` under `<vault_root>/.archive/` — that is `<storage.root>/vault/.archive/`, not `<storage.root>/.archive/` — in a mirror layout, and records a row in `archive_index`. Archived content stays in plaintext on the local filesystem until the retention deadline (`60` days by default, configurable) is reached and the retention GC physically destroys it.
- **The embedding pipeline is a third egress path.** When `[embed] enabled = true` (the default), note text is POSTed to `[embed] endpoint` — `http://localhost:8436/v1/embeddings` by default, i.e. loopback, no internet egress out of the box. The endpoint accepts any OpenAI-compatible URL: pointing it at a hosted embedding API sends note content to that provider.
- **Note bodies are also persisted in the job queue.** `vault_write` and `validate` job specs carry the full Markdown body (`JobSpec.body`, `ValidateSpec.body`), serialised into `<storage.root>/db/queue.sqlite` — in the live `gradatum_jobs.payload` column, which is **JSON text, not an opaque blob** (the legacy `jobs_v2.payload` / `jobs.payload_json` columns carry the same field). This copy is a second plaintext location for note content, on a file distinct from `index.db` and from the `.md` files. It is reached by neither `forget`, nor `delete`, nor the archive retention GC. **Completed jobs are never purged**: the only queue GC deletes rows with `status = 'DLQ'` older than 30 days, so the bodies of every successfully processed write remain in `queue.sqlite` indefinitely. Operators must include this file in the same data-handling policy as the vault itself.
- **Embeddings derive from note content.** Vectors computed from note bodies are persisted in `index.db` (`note_embeddings`) and in the ANN index. They are not directly readable, but embedding inversion is a published attack class and irreversibility is not a property to rely on: treat them as content-derived data, under the same policy as the note bodies. They outlive a `forget`.

---

## Agent identity security (`identity` section)

Gradatum ships a protected `identity` section (13th canonical section, introduced in v0.7.6) for storing per-agent soul notes — structured Markdown documents that describe an agent's behavioral invariants, guidelines, and policies.

### Access model

- **Enumeration is hidden**: `identity` notes are excluded from search, list, by-status, review, trace, graph, and link surfaces for non-privileged callers. The filter (`identity_section_hidden`) and the read/trace guards are applied per-handler across the full HTTP and MCP surface — there is no single gateway guard that could be bypassed.
- **The timeline surface is stricter: it excludes `identity` for *every* caller, privileged ones included.** `vault_timeline` filters in SQL on `Section::PROTECTED_FORGET`, which contains `Identity`; no privilege check is consulted, so no soul is reachable through the timeline at all. This is a blackout, not a per-caller filter.
- **Privileged caller**: `1.0.0` has exactly **one** privileged identity — the bearer whose JWT `sub` equals `main-agent` (`SOUL_PRIVILEGED_WRITER`). It bypasses the per-agent match on every identity surface that exposes souls at all, and may enumerate, read, and write **any** agent's identity note. This is deliberate: the orchestrator provisions and repairs the souls of the agents it supervises. The consequence must be stated plainly — **the `identity` section isolates agents from each other, not from the orchestrator**. Any credential minted with `sub = main-agent` carries full read/write authority over every soul in the tenant, and the shipped `hierarchical.toml` ACL preset declares exactly such a consumer. Treat that credential with the same care as an admin credential: scope it narrowly, store it chmod-600, and rotate it on suspicion.
- **At the default `multi_tenant.enabled = false`, the tenant dimension is closed rather than merely unpartitioned.** No credential can carry a tenant other than `main`, and three independent guards enforce it: API-key creation rejects any other tenant outright, `/auth/exchange` refuses to mint a JWT for such a key, and the authentication middleware rejects any such context with `403` **before** it reaches a handler. The privileged bearer's authority therefore spans the only tenant that can exist in that mode.
- **A second trust tier, `TrustContext::Studio`, is defined but never constructed in `1.0.0`.** The authentication middleware produces only `BearerToken` or `Unauthenticated`; every `Studio` value in the tree is built inside a test. Several code paths still branch on it, which is why it is named here — but it grants no access, because nothing issues it.
- **`vault_search` with `section=identity`**: returns results only to the privileged caller. Everyone else receives an empty result set (fail-closed — not a 403, to avoid oracle attacks on identity note existence).
- **Read access**: a non-privileged caller may only read their own identity note. The target agent is parsed server-side from the note's own `identity/<agent>` title, never taken from request input, and compared to the JWT `sub`. Reading another agent's note is rejected with `403` and emits an `identity_read_denied_foreign_agent` audit record.
- **Write access**: a non-privileged caller may only write or update their own identity note, under the same server-side title-versus-`sub` comparison. Cross-agent writes are rejected with `403` and emit an `identity_write_denied_foreign_agent` audit record.
- **Title and section hint must agree**: a write whose title starts with `identity/` while `section_hint` is anything other than `identity` is rejected with `400` (audit record `rejected_400_identity_title_without_hint`) before the body is processed. This closes the direct bypass — reaching the `identity/` title space through a different section hint, where the ownership comparison would not run.
- **The ownership predicate has two implementations.** `vault_read_impl` and `vault_write_impl` each carry an inline copy; `enforce_identity_read_guard` and `enforce_identity_write_guard` carry the other, applied to the secondary surfaces (`vault_trace`, `vault_classify`, `vault_history`, `vault_history_get`, `vault_restore`, `vault_diff`, `vault_downgrade`, `patch_note`, `move_note_locus`). Two copies are two places that can diverge, and they already do: the extracted guards recognise `TrustContext::Studio`, the inline copy in `vault_write_impl` does not. The divergence is inert today only because `Studio` is never constructed. The two implementations are not unified in `1.0.0`.
- **`doc_kind` is not an access control.** No code path forces a `doc_kind` at write time. Identity notes end up classified `Static` through a section-to-`doc_kind` mapping applied at indexing; that is a classification of the temporal axis, and nothing about it is enforced on the write path.
- **Soul validation**: the note body must pass a structural schema check at write time, and a body that fails it is rejected at the write endpoint with `400`.
  - A **root** soul (no `extends:`) requires all three sections `INVARIANTS`, `GATES` and `NARRATIVE`, **and** a line inside `INVARIANTS` that literally begins with `INV-CANARY`. The match is anchored to the start of the line, so mentioning the token in prose or in a comment does not satisfy it. This requirement is easy to miss when authoring a first soul, and it is the most likely cause of an otherwise puzzling `400`.
  - A **child** soul — one whose first non-empty line after the H1 is `extends:` — requires only `NARRATIVE`. `INVARIANTS`, `GATES` and the `INV-CANARY` line are inherited from the parent and are not required of the child. `extends:` detection is bounded to that first line, so the directive appearing later in prose does not turn a root soul into a child.
  - In **both** cases the body must be byte-stable: a line starting with `updated_at:` or `version:` is rejected. An optional `scope` field on an `INVARIANTS` line is accepted and ignored.
- **Worker reclassification guard**: the background curator treats identity notes as immutable — reclassification to a different section is a no-op, preventing the curator from overriding the section on re-ingest.

### MCP identity injection

On MCP `initialize`, the server injects an identity note body into the `instructions` field of the `initialize` response.

**The target soul is selected by the client, not by the token.** The server reads the `X-Gradatum-Agent` request header and normalises it (lowercased, `[a-z0-9-]` only, at most 64 characters); absent or malformed values fall back to `main`. The JWT `sub` is then used for a single check: injection proceeds if `sub` equals `main-agent`, **or** if `sub` equals the requested agent. For a non-privileged bearer that check is real — requesting another agent's soul yields no injection at all, and does not fall back to the caller's own soul. For `main-agent` the check always passes, so **the header alone decides which soul that session receives**.

If the note does not exist, the caller is unauthenticated, or the check fails, `instructions` is simply absent (graceful degradation — no error, and no signal distinguishing those cases).

Two distinct properties, which the wording above deliberately separates:

- The **content** of `instructions` cannot be forged by a client. It is read verbatim from the vault; no request field contributes to it.
- The **selection** of that content can be influenced by a client, within the `sub` check above. An operator who reads the injected soul as proof of *which* agent is connected is relying on a property the server does not provide.

### Scope

Identity section security is enforced at the application layer only — it does not provide cryptographic confidentiality at rest. Filesystem-level encryption (LUKS or equivalent) remains the operator's responsibility for protecting stored notes against direct file access.

---

## Known limitations

These are properties of `1.0.0` that an operator must account for before handling sensitive data. They are stated here because the product does not enforce them.

- **No PII filtering is applied.** Gradatum does not scan, mask, or redact personal data in note content at write time. There is no write-time content filter of any kind — neither pattern-based nor model-based — and no `[privacy]` configuration block. Note bodies are persisted verbatim to the `.md` file, to `index.db` (`body_text`), to `.history/`, to `.archive/`, to the HTTP audit log, and to the embedding index. Content-level privacy filtering is **planned for a future release** and is **not present in 1.0.0**. Operators handling regulated data must filter upstream of ingestion.
- **Data at rest is not encrypted**, and the application-layer guards (ACL, identity guards, section hiding) offer no protection against direct filesystem access. Filesystem-level encryption is the operator's responsibility.
- **Deletion is not immediate erasure.** `forget` erases nothing; `delete` archives for a retention window; the audit tombstone of a deleted note is never pruned. See *Privacy posture* above for the full picture.
- **Revoking an API key does not cut outstanding tokens.** See the revocation caveat under *Hardening defaults*.
- **Per-key write scope is not enforced outside multi-tenant mode.** With `multi_tenant.enabled = false` (the default), a key's scopes are recorded but not checked on the request path. With it on, a write requires the key to carry one of `write`, `admin`, `service`. **Read access is never governed by key scopes, in either mode** — it is governed by vault grants and the locus ACL.

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
