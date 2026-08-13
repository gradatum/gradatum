# gradatum-mcp-stub

> MCP stdio adapter — forwards MCP tool calls to the gradatum-server HTTP API.

**Status**: **retired from the distribution as of `2.0.0`.** `publish = false` — not built, not
shipped, not on crates.io as anything newer than its last published version, `1.0.0`. Source is
kept in this repository (retirement is reversible), but there is nothing to install from this
crate today.

**Why**: the binary only ever compiled for `x86_64-unknown-linux-gnu`, while its intended
audience — chiefly Claude Desktop, which drives its own auth flow and cannot attach a custom
header — runs on macOS or Windows, not Linux. `gradatum-server`'s native MCP transport (`/mcp`,
Streamable HTTP) is now the only integration path, for any client able to attach a request
header. See [Guide D — MCP & Studio](https://github.com/gradatum/gradatum/blob/main/docs/guides/D-mcp-and-studio.md)
in the main repository for current MCP setup instructions.

Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview (historical — describes the `1.0.0` binary, not distributed since)

`gradatum-mcp-stub` was a thin stdio process that bridged MCP hosts (Claude Code,
Claude Desktop, Continue.dev, etc.) to a running `gradatum-server` instance. Each MCP
tool call was serialized to JSON and forwarded to the corresponding REST endpoint (POST for
most tools; GET for `vault_status`, `vault_authors`, `vault_tags` and `vault_lessons_recall`);
the response was passed back to the host.

It supported auto-refresh authentication: when configured with an API key file, the stub
exchanged the key for a JWT at startup and renewed it transparently before expiry.

## Usage (historical — this binary is not distributed; kept for reference only)

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

## Environment Variables (historical)

| Variable | Default | Description |
|---|---|---|
| `GRADATUM_SERVER_URL` | `http://127.0.0.1:19090` | Base URL of gradatum-server |
| `GRADATUM_API_KEY_FILE` | — | Path to a chmod-600 file containing `ak_xxx` (preferred) |
| `GRADATUM_BEARER_TOKEN` | — | Static JWT (fallback if `GRADATUM_API_KEY_FILE` is absent) |

## MCP Tools Exposed (historical)

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
