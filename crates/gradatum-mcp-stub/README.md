# gradatum-mcp-stub

> MCP stdio adapter — forwards MCP tool calls to the gradatum-server HTTP API.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-mcp-stub` is a thin stdio process that bridges MCP hosts (Claude Code,
Claude Desktop, Continue.dev, etc.) to a running `gradatum-server` instance. Each MCP
tool call is serialized to JSON and forwarded as an HTTP POST to the corresponding REST
endpoint; the response is passed back to the host.

Supports auto-refresh authentication: when configured with an API key file, the stub
exchanges the key for a JWT at startup and renews it transparently before expiry.

## Usage

Configure in your MCP host:

```json
{
  "mcpServers": {
    "gradatum": {
      "command": "gradatum-mcp-stub",
      "env": {
        "GRADATUM_SERVER_URL": "http://127.0.0.1:19090",
        "GRADATUM_API_KEY_FILE": "/etc/gradatum/api.key"
      }
    }
  }
}
```

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `GRADATUM_SERVER_URL` | `http://127.0.0.1:19090` | Base URL of gradatum-server |
| `GRADATUM_API_KEY_FILE` | — | Path to a chmod-600 file containing `ak_xxx` (preferred) |
| `GRADATUM_BEARER_TOKEN` | — | Static JWT (fallback if `GRADATUM_API_KEY_FILE` is absent) |

## MCP Tools Exposed (18 tools)

| Category | Tools |
|---|---|
| Read (10) | `vault_search`, `vault_read`, `vault_list`, `vault_status`, `vault_graph`, `vault_links`, `vault_trace`, `vault_context`, `vault_authors`, `vault_tags` |
| Write (3) | `vault_write`, `vault_classify`, `vault_downgrade` |
| History F-40 (4) | `vault_history`, `vault_history_get`, `vault_restore`, `vault_diff` |
| Forget F-44 (1) | `vault_forget` |

## License

Apache-2.0
