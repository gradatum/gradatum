# gradatum-mcp-stub

> Thin MCP stdio adapter: forwards MCP tool calls to gradatum-server HTTP API.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Usage

Configure in your MCP host (Claude Desktop, Claude Code, Continue.dev):

```json
{
  "mcpServers": {
    "gradatum": {
      "command": "gradatum-mcp-stub",
      "env": {
        "GRADATUM_SERVER_URL": "http://127.0.0.1:19090",
        "GRADATUM_BEARER_TOKEN": "<your-jwt-token>"
      }
    }
  }
}
```

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `GRADATUM_SERVER_URL` | `http://127.0.0.1:19090` | gradatum-server base URL |
| `GRADATUM_BEARER_TOKEN` | **(required)** | JWT bearer token |

## MCP Tools exposed (10 tools)

| Tool | Type | Description |
|---|---|---|
| `vault_search` | POST | Full-text + semantic search |
| `vault_read` | POST | Read note by path |
| `vault_list` | POST | List notes with filters |
| `vault_status` | GET | Vault status and stats |
| `vault_graph` | POST | Wikilink graph from root note |
| `vault_links` | POST | Wikilinks for a note |
| `vault_trace` | POST | Trace chain through notes |
| `vault_context` | POST | Context window for a note |
| `vault_authors` | GET | List note authors |
| `vault_tags` | GET | List tags with frequencies |

## Reconnect

Exponential backoff 100ms → 5s, max 10 attempts.
On 11th failure: `McpError::internal_error("server unavailable")`.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation + Read API
- License : Apache-2.0