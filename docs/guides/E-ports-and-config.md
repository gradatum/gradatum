# Guide E — Ports & configuration reference

> Moved from `PORTS.md` (repo root) — see that file for a one-line pointer back here.
> This is the canonical port matrix; every other document (`ARCHITECTURE.md`, `README.md`,
> `docs/DEPLOYMENT.md`) links here rather than restating it.

> Range `19090–19100` reserved. Each port is `19090 + role offset`.

---

## Convention

Gradatum follows the **`19090 + offset`** pattern to:

- Stay outside the canonical Prometheus range (`9090–9099`) — operators routinely use `9090–9099` for `prometheus`, `alertmanager`, `pushgateway`, and node exporters. Gradatum components must not collide.
- Cluster all Gradatum services in a single 11-port range (`19090–19100`), making firewall rules, reverse-proxy configs and observability dashboards trivial to scope.
- Leave room above the core range for project-local extensions (custom adapters, third-party integrations) without risking conflict. The exact boundaries are stated once, in "Range allocation policy" below — this section does not restate them.

---

## Default port matrix

| Service | Default port | Offset | Notes |
|---|---|---|---|
| `gradatum-server` (HTTP/MCP) | **19090** | 0 | Main entrypoint. SSE, JSON, MCP-over-HTTP. |
| `gradatum-server` metrics | 19091 | 1 | Prometheus `/metrics`, separate listener bound to loopback by default (security caveat C7 — metrics binding must stay loopback, see [SECURITY.md § Hardening defaults](../../SECURITY.md#hardening-defaults)). |
| `gradatum-server` internal API | 19092 | 2 | `/internal/v1/*` (worker ↔ server). Spawned **only** when `[internal_api] token` is configured, in its own background task. Loopback-enforced — but that check runs *inside* the background task: a non-loopback `[internal_api] bind` makes the internal listener exit with an error that is only **logged**, not propagated. **The main server keeps running and serving `/health` normally; the internal API simply never comes up.** This does not abort the boot. |
| `gradatum-gateway` (LLM gateway, optional) | **8436** — `19093` reserved, unused | 3 | OpenAI-compatible + Anthropic mappers. **This service has no default in code**: `[server] listen` is mandatory, so the value below is a convention, not a fallback. `19093` was reserved here under the `19090 + offset` scheme, but every shipped example and deployment binds **8436** (`crates/gradatum-gateway/examples/spike-engine-routing.toml`, `ARCHITECTURE.md`, `README.md`, `docs/DEPLOYMENT.md`). The `19093` reservation is recorded here for completeness, this table being the sole canonical port matrix. **Caveat:** `19093` would collide with Alertmanager's default `9093` only if an operator reused that numeric port elsewhere; the `19090+` prefix avoids it. |
| `gradatum-vault` HTTP (read-only API, optional) | 19094 | 4 | Reserved for vault-only deployments without `gradatum-server`. |
| Reserved | 19095 | 5 | — |
| `gradatum-studio` (admin UI) | 19096 | 6 | Port reserved for a future standalone Studio process. In `1.0.0`, Studio is served by `gradatum-server` at `/ui/*` (port 19090, no separate process). |
| Reserved | 19097–19099 | 7–9 | Future use. |
| `gradatum-worker` healthcheck | 19100 | 10 | Worker has no public listener; this port exposes `/health` only. |

**Override:** every default can be overridden via TOML, env var, or CLI flag — see "Override matrix" below.

---

## Override matrix

Listeners are configured as full **socket addresses** (`ip:port`), not as bare port numbers.
Precedence: **env > TOML > defaults** (figment layering).

`gradatum-server` takes exactly one CLI flag — `--config <path>`. It exposes no per-port flag.
Environment overrides use the `GRADATUM_` prefix with a **double underscore** as the section
separator (`Env::prefixed("GRADATUM_").split("__")`), so the variable name mirrors the TOML path.

| Setting | TOML key (`server.toml`) | Environment variable |
|---|---|---|
| Server listener | `[server] bind = "127.0.0.1:19090"` | `GRADATUM_SERVER__BIND` |
| Server metrics listener | `[server] metrics_bind = "127.0.0.1:19091"` | `GRADATUM_SERVER__METRICS_BIND` |
| Storage root | `[storage] root = "/var/lib/gradatum"` | `GRADATUM_STORAGE__ROOT` |
| Log format | `[log] format = "json"` | `GRADATUM_LOG__FORMAT` |

`gradatum-gateway` and `gradatum-engine` are separate binaries with their own config files;
`gradatum-engine` reads its config path as a **positional** argument
(`gradatum-engine <config-path>`) and takes **no** environment override — its TOML file is
its only configuration source (F-190). The `GRADATUM_ENGINE_` prefix names one thing only,
the event-log credential `GRADATUM_ENGINE_API_KEY`.
Full `[engine]` field reference: [docs/DEPLOYMENT.md §4](../DEPLOYMENT.md#4-configuration-reference).

**Bind safety (`gradatum-server`):** two fail-closed guards, both detailed in
[SECURITY.md § Hardening defaults](../../SECURITY.md#hardening-defaults) (referred to elsewhere
in this project as caveats C3 and C7):

- **C3 — main listener.** `[server] bind` on a non-loopback address **without** `[server.tls]`
  → the server refuses to boot.
- **C7 — metrics listener.** `[server] metrics_bind` must be loopback **unconditionally** — TLS
  does not lift this; a non-loopback metrics address aborts startup.
- The other listeners in the matrix above are not covered by these two guards. In particular,
  the internal-API listener (see table above) enforces its own loopback check, but a failure
  there does not abort the server's boot — see the caveat in that row.

---

## Known collisions to watch

| External service | Default port | Mitigation |
|---|---|---|
| Prometheus server | 9090 | Out of range — no collision. |
| Alertmanager | 9093 | Out of range — no collision. |
| Pushgateway | 9091 | Out of range — no collision. |
| Anything in `19090–19100` | — | Operator's responsibility to coordinate; Gradatum logs the bind error and exits non-zero on conflict. |

---

## Range allocation policy

- `19090–19100`: reserved for Gradatum core services. Any new Gradatum service must claim an offset here.
- `19101–19199`: free for operator-local extensions (custom adapters, ad-hoc proxies).
- `>= 19200`: out of Gradatum scope.

Adding a new Gradatum service requires updating the table above as part of the project-map
feature card that introduces it (see [`../../GOVERNANCE.md`](../../GOVERNANCE.md) § Structural
change tracking).

---

## `server.toml` — fields set by `gradatum-admin init`

`gradatum-admin init` (source: `crates/gradatum-admin/src/init.rs`, `generate_server_toml_template`)
writes these sections, unconditionally, `[internal_api]` included:

```toml
[server]
bind = "<--bind value, default 127.0.0.1:19090>"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "<--root value>"
vault_index_path = "<root>/vault/.gradatum/index.db"

[auth]
jwt_ttl_human_secs = 3600
jwt_ttl_service_secs = 86400
revocation_store = "sqlite"
revocation_db_path = "<root>/db/revocation.sqlite"
api_keys_db_path = "<root>/db/api_keys.sqlite"

[acl]
preset_path = "<root>/config/bearer.toml"

[log]
format = "json"

[embed]
enabled = true
endpoint = "http://localhost:8436/v1/embeddings"
model = "bge-m3-Q8_0"
dim = 1024
timeout_ms = 5000

[internal_api]
bind = "127.0.0.1:19092"
token = "<256-bit CSPRNG secret, hex-encoded>"
admin_token = "<256-bit CSPRNG secret, hex-encoded>"
```

`[internal_api]` is **not** opt-in: `init` writes it on every run, `bind` is always the loopback
default (`127.0.0.1:19092`, independent of `--bind`), and `InitArgs` exposes no flag to skip it.
`token` and `admin_token` are two independent 256-bit CSPRNG secrets minted fresh each `init` (or
`init --force`). `token` is additionally copied to `config/internal-worker.token.txt` (mode
0600), specifically so the operator does not have to parse `server.toml` to retrieve it.

**Operator gesture:** to run `gradatum-worker` against this server, read
`config/internal-worker.token.txt` and export its contents as `GRADATUM_INTERNAL_TOKEN` — do
not mint your own value (e.g. `openssl rand -hex 32`) independently of `init`; a self-generated
token will not match what `gradatum-server` reads from `[internal_api].token`, unless something
else overrides it (see "Override matrix" above — env beats TOML).

Verified empirically: `gradatum-admin init --root <tmp-dir> --non-interactive`, then inspecting
the resulting `server.toml` and `config/internal-worker.token.txt`.
