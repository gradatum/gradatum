# PORTS.md

> Port convention for Gradatum services.
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
| `gradatum-server` metrics | 19091 | 1 | Prometheus `/metrics`, separate listener bound to loopback by default (security caveat C7). |
| `gradatum-server` internal API | 19092 | 2 | `/internal/v1/*` (worker ↔ server). Spawned **only** when `[internal] token` is configured. Loopback-enforced at startup — a non-loopback `[internal] bind` aborts the boot. |
| `gradatum-gateway` (LLM gateway, optional) | **8436** — `19093` reserved, unused | 3 | OpenAI-compatible + Anthropic mappers. **This service has no default in code**: `[server] listen` is mandatory, so the value below is a convention, not a fallback. `19093` was reserved here under the `19090 + offset` scheme, but every shipped example and deployment binds **8436** (`crates/gradatum-gateway/examples/spike-engine-routing.toml`, `ARCHITECTURE.md`, `README.md`, `docs/DEPLOYMENT.md`). The `19093` reservation is recorded because RFC-0003 still cites it. **Caveat:** `19093` would collide with Alertmanager's default `9093` only if an operator reused that numeric port elsewhere; the `19090+` prefix avoids it. |
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
(`gradatum-engine <config-path>`), and layers env under the `GRADATUM_ENGINE_` prefix.

**Bind safety (`gradatum-server`):**

- `[server] bind` on a non-loopback address **without** `[server.tls]` → the server refuses to
  boot (security caveat C3, fail-closed).
- `[server] metrics_bind` must be loopback **unconditionally** — TLS does not lift this
  (caveat C7); a non-loopback metrics address aborts startup.
- The other listeners in the matrix above are not covered by these guards.

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

Adding a new Gradatum service requires updating the table above as part of the RFC that introduces it (see [`RFC-TEMPLATE.md`](RFC-TEMPLATE.md)).
