# PORTS.md

> Port convention for Gradatum services.
> Range `19090–19100` reserved. Each port is `19090 + role offset`.

---

## Convention

Gradatum follows the **`19090 + offset`** pattern to:

- Stay outside the canonical Prometheus range (`9090–9099`) — operators routinely use `9090–9099` for `prometheus`, `alertmanager`, `pushgateway`, and node exporters. Gradatum components must not collide.
- Cluster all Gradatum services in a single 11-port range (`19090–19100`), making firewall rules, reverse-proxy configs and observability dashboards trivial to scope.
- Leave `19100–19200` available for project-local extensions (custom adapters, third-party integrations) without risking conflict.

---

## Default port matrix

| Service | Default port | Offset | Notes |
|---|---|---|---|
| `gradatum-server` (HTTP/MCP) | **19090** | 0 | Main entrypoint. SSE, JSON, MCP-over-HTTP. |
| `gradatum-server` metrics | 19091 | 1 | Prometheus `/metrics`, separate listener bound to loopback by default (security caveat C7). |
| Reserved (HTTP admin proxy, future) | 19092 | 2 | Reserved. Do not bind. |
| `gradatum-gateway` (LLM gateway, optional) | **19093** | 3 | OpenAI-compatible + Anthropic mappers. **Caveat:** collides with Alertmanager default `9093` only if operator decided to use the same numeric port elsewhere. The `19090+` prefix avoids that collision. |
| `gradatum-vault` HTTP (read-only API, optional) | 19094 | 4 | Reserved for vault-only deployments without `gradatum-server`. |
| Reserved | 19095 | 5 | — |
| `gradatum-studio` (admin UI, future) | 19096 | 6 | Studio is post-v0.1; port reserved now. |
| Reserved | 19097–19099 | 7–9 | Future use. |
| `gradatum-worker` healthcheck | 19100 | 10 | Worker has no public listener; this port exposes `/health` only. |

**Override:** every default can be overridden via TOML, env var, or CLI flag — see "Override matrix" below.

---

## Override matrix

The same setting can be expressed in three ways. Precedence: **CLI > env > TOML**.

| Setting | TOML key (`gradatum.toml`) | Environment variable | CLI flag |
|---|---|---|---|
| Server HTTP port | `[server] port = 19090` | `GRADATUM_SERVER_PORT` | `--server-port 19090` |
| Server metrics port | `[server] metrics_port = 19091` | `GRADATUM_SERVER_METRICS_PORT` | `--server-metrics-port 19091` |
| Server bind address | `[server] bind = "127.0.0.1"` | `GRADATUM_SERVER_BIND` | `--server-bind 127.0.0.1` |
| Gateway HTTP port | `[gateway] port = 19093` | `GRADATUM_GATEWAY_PORT` | `--gateway-port 19093` |
| Worker health port | `[worker] health_port = 19100` | `GRADATUM_WORKER_HEALTH_PORT` | `--worker-health-port 19100` |
| Studio port | `[studio] port = 19096` | `GRADATUM_STUDIO_PORT` | `--studio-port 19096` |

**Bind safety:** if any port is configured to bind on a non-loopback interface (`0.0.0.0`, public IP) **and** TLS is not configured for that listener, the service refuses to boot (security caveat C3, fail-closed).

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
