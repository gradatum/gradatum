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

While Gradatum is in `0.x` (Alpha / Beta), only the latest tag receives security fixes.

**Current supported version** : `v0.7.6` (latest stable). Prior `0.x` tags are end-of-life.

After `1.0`:

| Version | Status | Security fixes |
|---|---|---|
| `1.x` LTS branch | Supported | Yes — for the LTS lifetime declared at `1.0` release. |
| `1.x` main | Supported | Yes — fixes ported from LTS. |
| `0.x` | End-of-life at `1.0` | No backports. |

The exact LTS lifetime (e.g. 18 or 24 months) is decided in the `1.0` release RFC.

---

## Hardening defaults

By design, Gradatum applies these defaults at boot time:

- **Bind loopback only** (`127.0.0.1`) by default. Non-loopback binds require `[server.tls]` to be configured; the server refuses to boot on a non-loopback address without TLS (fail-closed, caveat C3).
- **Native TLS termination supported** (since v0.5.2) via `[server.tls] { cert_path, key_path }` (rustls 0.23, TLS 1.2+/1.3, `axum-server` `bind_rustls`). Certificate and key are loaded before bind: any load failure (missing or corrupt PEM, insufficient permissions) aborts startup rather than falling back to cleartext. The deployed default (`127.0.0.1:19090`, no `[server.tls]` block) remains loopback behind a reverse proxy.
- **JWT audience-scoped strict** (`aud=service-X` exact match), Ed25519, mandatory `kid` header, TTL 1 h (caveat C1, decision D6).
- **Persistent revocation store** required (caveat C2). In-memory store is allowed in dev with a `WARN` log on every boot.
- **Gateway body logging is not implemented.** No prompt or response content is written to disk by the LLM gateway. Encryption at rest for logged payloads is planned for a future release. Header sanitisation is not currently enforced at the gateway layer.
- **OpenDAL backends gated by feature flags** — only the backends explicitly enabled are compiled in (caveat C12).

- **Studio admin UI auth**: JWT stored in **localStorage** (key `gradatum_studio_jwt_persist`, persists across page reloads; api-key is never stored). Short-lived: **1 h** (scope `human`). Service/machine tokens: 24 h.
- **Strict Content Security Policy (studio)**: `script-src 'self'`, `connect-src 'self'`, `style-src 'self'`, `frame-ancestors 'none'` — no `unsafe-inline` or `unsafe-eval` on scripts. Served by gradatum-server (`gradatum-server/src/studio.rs`).
- **Studio response headers**: `Content-Security-Policy` (above) + `X-Content-Type-Options: nosniff` + `Referrer-Policy: no-referrer` + **`X-Frame-Options: DENY`** (complements `frame-ancestors` for legacy browsers: IE11, Safari < 10).
- **Body limits (anti-DoS)**: `/mcp` and `/internal/v1/persist/embedding` capped at **512 KiB**.
- **`/auth/exchange` rate-limited**: WardenLayer (defaults: 60 req/min, burst 10) + argon2id on api-key verification.
- **Known limitations (CAVEATS)**:
  - **TLS**: gradatum serves cleartext by default. **TLS (rustls) is required for any non-loopback exposure.** A cleartext deployment outside loopback exposes the JWT in transit. The operator is responsible for configuring `[server.tls]`.
  - **Logout is client-side only**: the studio logout clears localStorage (`localStorage.removeItem('gradatum_studio_jwt_persist')`). A JWT stolen before logout remains valid until it expires (≤ 1 h for studio sessions). Server-side revocation on logout is deferred (not yet implemented in v0.7.6).
  - **Rate-limit and reverse-proxy**: `ratelimit.exempt_localhost = true` by default. Set to `false` if gradatum runs behind a reverse-proxy (otherwise the loopback bypass disables rate-limiting on `/auth/exchange`).
  - **HSTS**: not emitted by default. Recommended when the operator enables TLS.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full security design and caveats.

---

## Privacy posture

_As of v0.7.6. Review this section on each minor release._

Gradatum is designed for local-first, self-hosted deployments. The following data handling properties hold in v0.7.6:

- **Data at rest is not encrypted.** The SQLite index (`index.db`), note frontmatter, note body text, and version history snapshots (`.history/`) are stored in plaintext on the local filesystem. Filesystem-level encryption (e.g. LUKS) is the operator's responsibility.
- **`body_text` is indexed in `index.db`** (note content indexed for full-text search). This is in addition to the on-disk `.md` files.
- **`author` is a free-form string field** persisted in `index.db` and in note frontmatter. No identity verification is performed; operators should document their author identifier convention.
- **`forgotten_by` is a free-form string field** persisted in `index.db` (column `forgotten_by`), in note frontmatter, and returned verbatim in `GET /api/v1/vault/forgotten` responses. It records the actor identifier that triggered a forget operation. No identity verification is performed; operators should treat this field as potentially containing PII (e.g. usernames, email addresses) and apply the same data-handling policy as for the `author` field.
- **Note history retention is configurable.** The Copy-on-Write history store (`.history/<note-id>/`) applies a count cap (`[history] max_versions`, default 50) and an optional TTL (`[history] ttl_days`, default: no expiry). Both are set in `gradatum.toml`. A note's history is removed when the note is deleted.
- **LLM backend locality is not enforced.** The gateway does not enforce data locality. Routing to remote endpoints is possible depending on configuration. Configuring a cloud-hosted LLM backend (e.g. Anthropic, OpenAI) will cause note content to be sent to that provider. Operators are responsible for ensuring their backend configuration matches their data residency requirements. Setting `vision_capable = true` on a gateway alias additionally routes image content (base64-encoded) to that backend. **The curator pipeline is an additional egress path:** when `[curator.llm]` is configured with a non-heuristic backend, note body content is sent to that LLM endpoint (local or cloud, depending on the `backend` field) during classification. The default backend is heuristic (offline; no network egress).
- **CORS is disabled by default.** The gateway's `allowed_origins` defaults to an empty list (all cross-origin requests rejected). Setting `allowed_origins = ["*"]` is accepted but discouraged in production environments with sensitive data.
- **No telemetry is collected by Gradatum.** The server does not transmit usage data, metrics, or note content to any external endpoint operated by the Gradatum project.
- **`event_log` table (`index.db`)** records LLM gateway calls. Fields persisted: `route`, `model_alias`, `provider`, `feature_id` (nullable), `status_code`, `latency_ms`, `outcome` (nullable), `agent_id` (nullable — client-supplied `X-Agent-Id` request header, not a server-verified identity). Retention: 30 days by default (configurable via `[event_log] retention_days`). No prompt or response content is stored.
- **`session_trace` table (`index.db`)** records session-log Tier 1 entries (agent action tracing). Fields persisted: `session_id`, `agent_id` (JWT `sub` — stable agent identifier, server-assigned), `tenant_id` (from JWT), `ts_ms`, `action_type`, `target` (nullable, ≤ 512 chars), `intent` (nullable, ≤ 200 chars), `outcome` (nullable), `marker` (nullable), `ref` (nullable). Retention: 90 days by default (configurable via `[session_trace] retention_days`).
- **HTTP audit log — not active by default.** The `JsonlFileSink` sink and the `HttpAuditEvent` data shape (`ts`, `event`, `actor` with JWT `kid`/`sub`/`aud`, `tenant_id`, `locus`, `note_id`, `content_hash`, `outcome`, `request_id`) are defined in `gradatum_core::audit::http` and implemented in `gradatum-server`. In v0.7.6, the server defaults to `NoopAuditSink`: **no audit files are written by default**. There is no `[audit]` configuration block in v0.7.6. Full HTTP audit logging with daily rotation and retention is planned for a future release.

---

## Agent identity security (`identity` section — v0.7.6)

Gradatum v0.7.6 introduces a protected `identity` section (13th canonical section) for storing per-agent soul notes — structured Markdown documents that describe an agent's behavioral invariants, guidelines, and policies.

### Access model

- **Enumeration is hidden**: `identity` notes are excluded from all search, list, timeline, by-status, review, trace, graph, and link surfaces for non-privileged callers. The filter (`identity_section_hidden`) and the read/trace guards are applied per-handler across the full HTTP and MCP surface — there is no single gateway guard that could be bypassed.
- **`vault_search` with `section=identity`**: returns results only to privileged callers (e.g. the MCP server at `initialize` time). Non-privileged callers receive an empty result set (fail-closed — not a 403, to avoid oracle attacks on identity note existence).
- **Read access** (`vault_read`): guarded by `enforce_identity_read_guard`. A non-privileged caller may only read their own identity note (matched by JWT `sub`). Reading another agent's note is rejected.
- **Write access** (`vault_write`): guarded by `enforce_identity_write_guard`. Only the bearer whose `agent_id` matches the JWT `sub` may write or update their own identity note. Cross-agent writes are rejected. `doc_kind` is forced to `Static` on all identity note writes.
- **Soul validation**: the note body must pass `validate_soul()` at write time. This checks for required structural sections (`INVARIANTS`, `GATES`, `NARRATIVE`), validates `extends:` resolution (bounded depth), and accepts an optional `scope` field. Structurally invalid soul notes are rejected at the write endpoint.
- **Worker reclassification guard**: the background curator treats identity notes as immutable — reclassification to a different section is a no-op, preventing the curator from overriding the section on re-ingest.

### MCP identity injection

On MCP `initialize`, the server injects the requesting agent's identity note body into the `instructions` field of the `initialize` response. The agent is identified by the JWT `sub` of the API key used for the session. If no identity note exists for the requesting agent, `instructions` is absent (graceful degradation — no error).

This mechanism is server-driven: clients do not need to request injection, and cannot inject arbitrary content into the `instructions` field.

### Scope

Identity section security is new in v0.7.6. It is enforced at the application layer only — it does not provide cryptographic confidentiality at rest. Filesystem-level encryption (LUKS or equivalent) remains the operator's responsibility for protecting stored notes against direct file access.

---

## Supply chain

- `cargo audit` and `cargo deny` run **daily** in CI with `fail-on-finding` (caveat C8).
- All dependencies are pinned with `=` for critical workspace deps (decision R11).
- A SBOM (CycloneDX) will be published once tooling is in place.
- Vendored headers are included in the release tarball; checksums published.

---

## Acknowledgements

A `SECURITY-HALL-OF-FAME.md` will be published once the first vulnerability is resolved. Reporters are credited with their preferred handle (or kept anonymous on request).
