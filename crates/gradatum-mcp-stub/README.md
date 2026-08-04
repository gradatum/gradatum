# gradatum-mcp-stub

> MCP stdio adapter — forwards MCP tool calls to the gradatum-server HTTP API.

**Status**: v1.0.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-mcp-stub` is a thin stdio process that bridges MCP hosts (Claude Code,
Claude Desktop, Continue.dev, etc.) to a running `gradatum-server` instance. Each MCP
tool call is serialized to JSON and forwarded to the corresponding REST endpoint (POST for
most tools; GET for `vault_status`, `vault_authors`, `vault_tags` and `vault_lessons_recall`);
the response is passed back to the host.

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

## MCP Tools Exposed

| Category | Tools |
|---|---|
| Read | `vault_search`, `vault_read`, `vault_list`, `vault_status`, `vault_graph`, `vault_links`, `vault_trace`, `vault_context`, `vault_timeline`, `vault_authors`, `vault_tags` |
| Write | `vault_write`, `create_feature_card`, `vault_classify`, `vault_downgrade` |
| History | `vault_history`, `vault_history_get`, `vault_restore`, `vault_diff` |
| Archives | `vault_archives_list` |
| Forget | `vault_forget` |
| Lessons Recall | `vault_lessons_recall` |
| Code Scope | `code_scope` |
| Proactive Recall | `vault_proactive_recall`, `vault_proactive_recall_feedback` |

The table above is a guide to the categories, not a contract. The stub proxies the
tools it declares at `tools/list`; that response is authoritative for a given build,
and it is not necessarily identical to the `tools/list` of the `gradatum-server` it
forwards to. To know what a running pair actually exposes, call `tools/list` on each —
do not rely on any count or list written here.

## License

Apache-2.0
