# RFC-0003 — HTTP API surface and MCP integration topology

| Field | Value |
|---|---|
| **RFC number** | 0003 |
| **Status** | `accepted` |
| **Started** | 2026-05-04 |
| **Resolved** | 2026-05-04 |
| **Tracking issue** | — (Phase 2.0 design) |
| **Affected crates** | `gradatum-server`, `gradatum-mcp-stub` (existing as binary stub, full impl from Phase 2.0) |
| **Authors** | Gradatum maintainers |

---

## 1. Status

Accepted via maintainer review on 2026-05-04. Three GO / APPROVE-WITH-CAVEATS verdicts; **14 caveats absorbed inline** before ratification (3 P0 + 11 P1; 8 P2 deferred Phase 2.1+). No P0 blockers remain. This RFC formalises the API-surface topology decision before any Phase 2.0 implementation begins.

## 2. Motivation

Phase 2.0 introduces `gradatum-server` as the project's first long-running daemon. Three distinct client classes need access to the vault: native HTTP/SDK consumers, MCP-aware AI agents (Claude Code, Claude Desktop, Continue.dev, …), and OpenAI-compatible LLM clients (post-Phase 2.0, via the optional `gradatum-gateway` binary).

Without an explicit decision document on the API surface, Phase 2.0 risks accumulating ad-hoc routing patterns and inconsistent base URLs. Specifically:

- How are HTTP API and MCP transports multiplexed on the daemon's listener?
- What is the recommended setup for each MCP-aware client family?
- Where does OpenAI compatibility live (same binary, different binary, different port)?

This RFC answers all three. It commits to a single-port path-prefix topology for `gradatum-server`, a stdio stub binary for MCP integration, and explicit deferral of OpenAI compatibility to `gradatum-gateway` (a separate optional binary, see [`PORTS.md`](../../PORTS.md)).

## 3. Decision summary

1. **`gradatum-server` exposes one TCP listener, one HTTP port** (default `19090`, see [`PORTS.md`](../../PORTS.md)).
2. **Path-prefix routing** under that port, via `axum::Router::nest()`:
   - `/api/v1` — native HTTP API (CRUD notes, search, graph, trace, classify, …).
   - `/mcp` — MCP-over-HTTP (streamable-http transport, `rmcp 0.17`).
   - `/sse` — MCP-over-SSE (deprecated transport, kept for legacy clients).
   - `/health` — liveness/readiness probe (unauthenticated).
   - `/admin` — admin endpoints (admin bearer scope).
3. **Metrics listener** is separate and bound to loopback (`127.0.0.1:19091`, security caveat C7 in [ARCHITECTURE.md](../../ARCHITECTURE.md) §12).
4. **MCP integration via `gradatum-mcp-stub`** — a thin stdio binary that translates each MCP method call into an HTTP request to a configured `gradatum-server` URL. This is the recommended setup for every MCP-aware AI client today.
5. **OpenAI compatibility is out of scope for this RFC.** It is delivered post-Phase 2.0 by the optional `gradatum-gateway` binary on port `19093` (see [`PORTS.md`](../../PORTS.md)) and will be the subject of a future RFC.

## 4. Single-port path-prefix routing

```
http://<host>:19090
├── /api/v1/...        → native gradatum HTTP API   (Rust SDK, gradatum-cli, curl, custom integrations)
├── /mcp               → MCP-over-HTTP             (streamable-http transport, rmcp 0.17)
├── /sse               → MCP-over-SSE              (legacy, deprecated for new clients)
├── /health            → liveness/readiness probe   (unauthenticated)
└── /admin/...         → admin endpoints            (admin bearer scope)
```

Routing is implemented at startup via `axum::Router::nest()` in `gradatum-server`. Each sub-tree owns its handlers, schemas, and middleware. Authentication middleware (`gradatum-acl-auth`) is applied uniformly except on `/health`.

A separate listener on `127.0.0.1:19091` exposes Prometheus `/metrics`. This is **not** reachable from the main port.

## 5. MCP integration via `gradatum-mcp-stub`

`gradatum-mcp-stub` is a thin client binary, already listed in the dependency DAG ([ARCHITECTURE.md](../../ARCHITECTURE.md) §4, level L1). Its responsibilities:

- Speak the MCP protocol over **stdio** to its parent process (the AI agent).
- Translate each MCP method call into an HTTP request against a configured `gradatum-server` URL.
- Return the HTTP response back to the agent as an MCP result.

**Why stdio, not MCP-over-HTTP?** Stdio is the **universal safe default** across MCP-aware AI clients today (2026-05-04): Claude Desktop only supports stdio; Claude Code supports both stdio and HTTP MCP transports. Stdio also provides the simplest secure default: no listener on the agent host, no inbound port to firewall, no public certificate to manage. The HTTP transports `/mcp` and `/sse` exposed by `gradatum-server` (§4) are kept available for clients that prefer remote MCP transport in the future, but no current AI client requires them.

**Configuration.** The stub locates its `gradatum-server` and authenticates via two environment variables:

- `GRADATUM_SERVER_URL` — defaults to `http://127.0.0.1:19090` if unset. Any HTTP or HTTPS URL reachable from the agent host is valid (see §5.1 for topology variants).
- `GRADATUM_BEARER_TOKEN` — required; no default. The stub exits non-zero with a clear message if missing.

**Optional Claude Code env vars** (per [Claude Code MCP docs](https://code.claude.com/docs/en/mcp), retrieved 2026-05-04, set when invoking Claude Code itself, not in the stub config):

- `MCP_TIMEOUT=10000` — Claude Code MCP server startup timeout in milliseconds (default ~30s). Increase if cold-start of the stub or upstream `gradatum-server` exceeds the default.
- `MAX_MCP_OUTPUT_TOKENS=50000` — Claude Code warning threshold for MCP tool output (default 10000). Operators using `vault_search` with large result sets should bump this proactively.

**Crash and restart.** Stdio MCP servers are local processes; per the MCP spec they are **not auto-reconnected** by the agent. If `gradatum-mcp-stub` crashes (or the upstream `gradatum-server` becomes unreachable), the agent re-spawns the stub on the next MCP method invocation. No watchdog is required at the stub layer.

The 12 MCP methods exposed by the stub — `vault_authors`, `vault_classify`, `vault_context`, `vault_downgrade`, `vault_graph`, `vault_list`, `vault_read`, `vault_search`, `vault_status`, `vault_tags`, `vault_trace`, `vault_write` — map 1:1 to `/api/v1/...` endpoints on `gradatum-server`.

### 5.1 Deployment topologies

The stub-server split is decoupled by HTTP and supports three deployment topologies. The architecture treats them uniformly; only operator configuration (URL, TLS, bearer rotation) differs.

| # | Topology | Stub host | `gradatum-server` host | Transport | TLS | Typical latency per call | Status |
|---|---|---|---|---|---|---|---|
| A | **Same-host (loopback)** | User workstation | Same workstation | `http://127.0.0.1:19090` | Not required | < 1 ms | Supported and recommended from Phase 2.0 (dogfooding default). |
| B | **LAN / private network (VPN, Tailscale, homelab)** | User workstation | Internal server (LXC, VM, NAS) | `https://internal-host:19090` | **Required** (mTLS optional for service-mesh deployments) | 1–10 ms | Supported from Phase 2.0. |
| C | **Public internet (remote vault)** | User workstation | Public-reachable VPS or service mesh egress | `https://gradatum.example.com` | **Mandatory** (caveat C3 fail-closed) | 50–200 ms | Architecturally supported from Phase 2.0; official operator guide ships Phase 2.1+ (see §12 Q1). |

Trust boundaries are identical in all three: agent ↔ stub (stdio, child process, loopback) then stub ↔ server (HTTP[+TLS] + bearer). The bearer is the only secret crossing the network boundary; it never crosses the agent boundary. See §8.

`gradatum-server` refuses to boot on a non-loopback bind without TLS configured (security caveat C3 in [ARCHITECTURE.md](../../ARCHITECTURE.md) §12, fail-closed). This protects topologies B and C from accidental plaintext exposure.

**TLS handling.** `gradatum-server` does not embed a built-in TLS terminator in Phase 2.0 (`reqwest` rustls-tls is used client-side only). Operators in topologies B and C terminate TLS via a reverse proxy on the same host (Caddy, Traefik, nginx) and bind `gradatum-server` to loopback. Native rustls termination inside `gradatum-server` is a Phase 2.1+ deliverable, detailed in the official operator guide.

## 6. MCP client setup

### 6.1 Claude Code (CLI)

Claude Code supports three configuration scopes (per [Claude Code MCP docs](https://code.claude.com/docs/en/mcp), retrieved 2026-05-04):

| Scope | Stored in | Loaded in | Shared with team |
|---|---|---|---|
| `local` (default) | `~/.claude.json` (project-keyed) | Current project only | No |
| `project` | `<project>/.mcp.json` | Current project only | Yes (versioned in repo) |
| `user` | `~/.claude.json` (user-keyed) | All projects | No |

**Recommended one-liners.** Add `gradatum` as a stdio MCP server in the desired scope. `GRADATUM_SERVER_URL` is shown explicitly so the same one-liner works for any topology (§5.1); omit it to default to loopback `http://127.0.0.1:19090` (topology A).

```bash
# local scope (current project, private) — topology A example
claude mcp add --transport stdio \
  --env GRADATUM_SERVER_URL=http://127.0.0.1:19090 \
  --env GRADATUM_BEARER_TOKEN=<token> \
  gradatum -- gradatum-mcp-stub

# user scope (all projects, private) — topology B example (homelab)
claude mcp add --transport stdio --scope user \
  --env GRADATUM_SERVER_URL=https://gradatum.lan:19090 \
  --env GRADATUM_BEARER_TOKEN=<token> \
  gradatum -- gradatum-mcp-stub

# project scope (versioned in repo, shared) — bearer token must NOT be committed; document the env var requirement in CONTRIBUTING.md
claude mcp add --transport stdio --scope project \
  --env GRADATUM_SERVER_URL=http://127.0.0.1:19090 \
  gradatum -- gradatum-mcp-stub
```

Claude Code option ordering rule: all flags come **before** the server name; `--` separates the server name from the command and arguments.

**Resulting `.mcp.json`** (project scope):

```json
{
  "mcpServers": {
    "gradatum": {
      "command": "gradatum-mcp-stub",
      "args": [],
      "env": {
        "GRADATUM_SERVER_URL": "http://127.0.0.1:19090",
        "GRADATUM_BEARER_TOKEN": "<token>"
      }
    }
  }
}
```

Manage with `claude mcp list`, `claude mcp get gradatum`, `claude mcp remove gradatum`. Inside Claude Code, `/mcp` shows runtime status.

### 6.2 Claude Desktop

Config file paths (per [MCP user quickstart](https://modelcontextprotocol.io/quickstart/user), retrieved 2026-05-04):

| OS | Path |
|---|---|
| macOS | `~/Library/Application Support/Claude/claude_desktop_config.json` |
| Windows | `%APPDATA%\Claude\claude_desktop_config.json` |
| Linux | **Not officially supported.** Use Claude Code (CLI) instead. |

Claude Desktop has no CLI for MCP management. Edit the JSON file manually:

```json
{
  "mcpServers": {
    "gradatum": {
      "command": "gradatum-mcp-stub",
      "args": [],
      "env": {
        "GRADATUM_SERVER_URL": "http://127.0.0.1:19090",
        "GRADATUM_BEARER_TOKEN": "<token>"
      }
    }
  }
}
```

Restart Claude Desktop completely (quit, then relaunch). Verify the MCP indicator appears in the bottom-right corner of the conversation input.

**Logs.**

| OS | Path |
|---|---|
| macOS | `~/Library/Logs/Claude/mcp-server-gradatum.log` |
| Windows | `%APPDATA%\Claude\logs\mcp-server-gradatum.log` |

`mcp.log` (same directory) contains general MCP connection logs.

### 6.3 Generic MCP clients (Continue.dev, …)

Any MCP-aware client following the protocol spec uses the same stdio command pattern:

- Command: `gradatum-mcp-stub`
- Args: `[]`
- Env: `GRADATUM_SERVER_URL`, `GRADATUM_BEARER_TOKEN`

Refer to client-specific documentation for the configuration file location and schema. The MCP protocol payload itself is identical across clients.

## 7. Client matrix

The "Network scope" column references the deployment topologies defined in §5.1: `A` = same-host loopback, `B` = LAN/VPN, `C` = public internet. All MCP-stdio clients support topologies A/B/C uniformly via stub configuration; native HTTP clients select the scope from the URL they call.

| Client class | Transport | Endpoint / command | Network scope (§5.1) | Typical use |
|---|---|---|---|---|
| `gradatum-cli` | HTTP | `http(s)://<server>:19090/api/v1` | A / B / C | Shell usage |
| `gradatum-sdk-rs` | HTTP | `http(s)://<server>:19090/api/v1` | A / B / C | Rust integrations |
| Native HTTP clients (`curl`, custom SDK) | HTTP | `http(s)://<server>:19090/api/v1` | A / B / C | Scripts, integrations |
| MCP-over-HTTP clients (rmcp-aware) | HTTP / SSE | `http(s)://<server>:19090/mcp` (streamable-http) or `/sse` *(deprecated)* | A / B / C | Future browser-based or remote MCP clients |
| Claude Code | MCP stdio (`gradatum-mcp-stub`) | `claude mcp add ... -- gradatum-mcp-stub` | A / B / C (via `GRADATUM_SERVER_URL`) | AI agent CLI |
| Claude Desktop | MCP stdio (`gradatum-mcp-stub`) | `claude_desktop_config.json` | A / B / C (via `GRADATUM_SERVER_URL`) | AI agent desktop app |
| Continue.dev, … | MCP stdio (`gradatum-mcp-stub`) | client-specific config | A / B / C (via `GRADATUM_SERVER_URL`) | Other AI agents |
| OpenAI SDK (post Phase 2.0) | HTTP via `gradatum-gateway` | `http(s)://<gateway>:19093/v1` | A / B / C | LLM-style clients. Out of RFC-0003 scope; covered by a future RFC introducing `gradatum-gateway`. |

## 8. Security and ACL

**Trust boundaries.** Two distinct boundaries exist in every deployment topology (§5.1):

1. **Agent ↔ stub** — stdio between two processes on the same host, parent-child relationship. No secret crosses this boundary in the protocol payload; the agent never sees `GRADATUM_BEARER_TOKEN`.
2. **Stub ↔ server** — HTTP (or HTTPS in topologies B/C) over the network. Bearer token in the `Authorization: Bearer <token>` header is the **only** secret crossing this boundary.

**Bind policy.**

- `gradatum-server` binds `127.0.0.1` by default. Public bind (`0.0.0.0`, public IP) refuses to boot without TLS configured (security caveat C3, [ARCHITECTURE.md](../../ARCHITECTURE.md) §12, fail-closed). Topology B requires TLS for non-loopback bind even on private networks; topology C requires TLS unconditionally.
- The metrics listener (`:19091`) is bound to loopback only and not reachable through the main port.

**Endpoint auth.**

- All endpoints under `/api/v1`, `/mcp`, `/sse`, `/admin` require a bearer token in `Authorization: Bearer <token>`.
- `/health` is unauthenticated (liveness/readiness probe).
- `/admin` endpoints additionally require the admin bearer scope.

**Stub auth.** `gradatum-mcp-stub` reads `GRADATUM_BEARER_TOKEN` from its environment at startup. The token is set by the operator in the agent's MCP config (`.mcp.json`, `claude_desktop_config.json`, …) and is never logged or printed by the stub. Token rotation is operator-driven: regenerate via `gradatum-admin token issue`, update the agent config, restart the agent.

**Bearer token storage — known caveat.** Storing `GRADATUM_BEARER_TOKEN` as an env var (the default mechanism in every MCP client config today) makes the token visible to any process running under the same UID via `ps -e -o pid,cmd,environ` and `/proc/<pid>/environ` on Linux (equivalent surfaces on macOS and Windows). On a single-user workstation this is acceptable defence-in-depth (topology A). On shared hosts, multi-tenant VMs, or any environment where local processes are not all trusted (typical for topologies B/C), operators should harden the storage:

- **OS keyring** — read the token at stub startup from `secret-tool` (Linux), Keychain (macOS), or Credential Manager (Windows); pass it to the agent's MCP `env` block via a wrapper script.
- **Restricted file** — write the token to a `chmod 600` file owned by the agent UID; the stub reads it via a `GRADATUM_BEARER_TOKEN_FILE` env var (Phase 2.1+ feature, file-based reading not yet implemented in Phase 2.0).
- **Socket-activated credentials** (systemd) — pass the token as a credential via `LoadCredential=` for service-managed agents.

The stub specification for Phase 2.0 covers env-var read only; the file/keyring paths above are operator-side hardening recipes documented in the Phase 2.1+ operator guide (§12 Q1). See also [ARCHITECTURE.md](../../ARCHITECTURE.md) caveat C17 (secrets handling).

**Topology-specific guidance.**

- **A (loopback):** bearer still required (defence-in-depth against local untrusted processes that could enumerate and probe `:19090`); rotation cadence operator's choice.
- **B (LAN/VPN):** TLS required (mTLS optional for service-mesh deployments). Bearer rotation aligned with VPN credential rotation. Consider keyring-based storage if the agent host runs untrusted workloads.
- **C (public):** TLS mandatory; bearer rotation on a fixed cadence (suggested ≤ 90 days); audit log retention per `gradatum-curator` policy. Keyring-based storage strongly recommended. Official guide ships Phase 2.1+ (see §12 Q1).

## 9. Cross-platform considerations (RFC-0002)

- **Linux** (primary tier, RFC-0002): `gradatum-mcp-stub` installed via `cargo install` or an `apt` package; configuration via Claude Code (CLI).
- **Windows** (secondary tier, RFC-0002): `gradatum-mcp-stub.exe` must be on `PATH` or referenced by absolute path in `claude_desktop_config.json` / `.mcp.json`. Pre-release manual validation per RFC-0002 §6.2.
- **macOS** (future roadmap, RFC-0002): no current validation; the stub remains portable by design (`reqwest` rustls-tls, no Linux-only syscalls).

For Windows-specific operational guidance (cert store, paths, troubleshooting), see [`docs/WINDOWS-GUIDE.md`](../WINDOWS-GUIDE.md).

## 10. Alternatives considered

| Alternative | Pros | Cons | Reason rejected |
|---|---|---|---|
| Three separate TCP ports (one per surface) | Strict surface isolation; per-port firewall rules | Multiplies operator configuration; conflicts with the `19090 + offset` reservation in [`PORTS.md`](../../PORTS.md). | Path-prefix routing inside one listener delivers the same isolation with less operator overhead. |
| MCP-over-HTTP only (no stdio stub) | One less binary to maintain. | Claude Desktop only supports stdio; mandating HTTP MCP would exclude it from Phase 2.0. | The stub is thin (~200 LOC, deps `rmcp` + `reqwest` + `serde`); cost of one extra binary is dominated by the universal client coverage. |
| Embed OpenAI compat (`/v1/chat/completions`, `/v1/embeddings`) directly in `gradatum-server` | One binary covers all surfaces. | Couples memory-backbone responsibilities to LLM-gateway responsibilities; inflates `gradatum-server` deps; conflicts with the `gradatum-gateway` binary already reserved on port `19093` in [`PORTS.md`](../../PORTS.md). | A separate optional `gradatum-gateway` binary on `19093` preserves single-responsibility per binary; covered by a future RFC. |

## 11. Drawbacks

- One additional binary (`gradatum-mcp-stub`) to build, version, and document. Mitigation: thin (~200 LOC), depends only on `rmcp`, `reqwest`, `serde`; ships in the same release pipeline as `gradatum-server`.
- Bearer token duplicated across agent config files (`.mcp.json`, `claude_desktop_config.json`, and any other client-specific MCP config) introduces an **operational risk of desynchronisation during rotation**: a missed file leaves an agent using a revoked token until the operator restarts it. Mitigation: documented in [`docs/WINDOWS-GUIDE.md`](../WINDOWS-GUIDE.md) and [`CONTRIBUTING.md`](../../CONTRIBUTING.md); future tooling (`gradatum-admin token issue --client claude-code`, Phase 4) automates generation and tracks active deployments to flag stale configs at rotation time.
- The path-prefix split (`/api/v1` vs `/mcp`) requires every endpoint owner to choose explicitly between native HTTP semantics and MCP semantics for the same underlying operation. Mitigation: the 12 MCP methods are a strict subset and map 1:1 to `/api/v1` endpoints; `gradatum-mcp-stub` enforces the mapping.

> **AM4 reminder** ([`RELEASE-POLICY.md`](../../RELEASE-POLICY.md)): this section is authored by maintainers and must be defendable in synchronous review without referring to AI-generated text.

## 12. Unresolved questions

- **Q1:** When does Gradatum publish the official deployment guide for topology C (public internet)? Topologies A and B are validated and recommended from Phase 2.0; topology C is architecturally supported (the stub already speaks HTTPS, the server fail-closes without TLS) but the end-to-end operator guide — TLS termination patterns, bearer rotation cadence, audit-log retention, rate limiting, DDoS posture — ships **Phase 2.1+** under the cross-host migration deliverable. Until then, topology C is "use at your own risk" with known-good Phase 1 building blocks.
- **Q2:** Should the bearer token configuration be unified via a single agent-side config file (e.g. `~/.gradatum/agent.toml`) rather than duplicated across each client's MCP config? **Decision deferred to Phase 4** (packaging and operator-experience polish).
- **Q3:** Should MCP-over-SSE (`/sse`) be removed in v0.2 in favour of streamable-http only? Concrete decision criterion: when the primary target MCP-over-HTTP clients (Claude Code, Claude Desktop) have all validated streamable-http in production. **Decision deferred to the pre-v1.0 transport audit.**

## 13. Cross-references

- [`RFC-0001-versioning-gradatum-core.md`](RFC-0001-versioning-gradatum-core.md) — trait stability tiers; framing for the `Chat` / `Embedder` / `Reranker` traits the server consumes.
- [`RFC-0002-cross-platform-support.md`](RFC-0002-cross-platform-support.md) — Linux primary + Windows secondary tier; `gradatum-mcp-stub` portability follows the same R1–R13 rules.
- [`ARCHITECTURE.md`](../../ARCHITECTURE.md) §"4 plans" — `gradatum-server` and `gradatum-mcp-stub` already listed in the control-plane / clients diagram.
- [ARCHITECTURE.md](../../ARCHITECTURE.md) §3, §4 — `gradatum-mcp-stub` in the L1 dependency DAG; `gradatum-server` at L4.
- [`PORTS.md`](../../PORTS.md) — port convention (`19090 + offset`); `gradatum-server` at `19090`, `gradatum-gateway` at `19093`, metrics at `19091` loopback.
- Phase 2.0 exit criteria: see [CHANGELOG.md](../../CHANGELOG.md) for implemented MCP methods.
- [`docs/WINDOWS-GUIDE.md`](../WINDOWS-GUIDE.md) — Windows-specific paths, cert store troubleshooting, `KNOWN_ISSUES-WINDOWS.md` pointer.
- [Claude Code MCP documentation (Anthropic)](https://code.claude.com/docs/en/mcp) — scopes, transports, CLI commands. Retrieved 2026-05-04.
- [MCP user quickstart (Claude Desktop)](https://modelcontextprotocol.io/quickstart/user) — config paths, JSON schema. Retrieved 2026-05-04.
- [Model Context Protocol specification](https://modelcontextprotocol.io/) — protocol reference.

## References

- Phase 2.0 design: internal design document (not published in this repository).
- `rmcp` crate (Rust MCP server/client): https://github.com/modelcontextprotocol/rust-sdk
- Maintainer review verdicts (2026-05-04): decision records, tag `[gradatum, RFC-0003]`.
