# gradatum-engine

> Rust supervisor for llama-server inference processes — transparent OpenAI-compatible reverse proxy with restart-on-failure.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-engine` manages one or more `llama-server` child processes, acting as a supervisor
and transparent HTTP proxy. It does not load models itself — it spawns an external
`llama-server` binary and forwards requests to it, preserving the full OpenAI-compatible
interface including streaming, vision (mmproj), sampling parameters, and slot IDs.

Architecture in v0.3.x:

1. **Spawn** — launches `llama-server` via `tokio::process::Command` (never via shell).
2. **Wait-ready** — polls `GET /health` on the child process until it returns 200.
3. **Transparent reverse proxy** — forwards request bodies verbatim to the child; passes
   through SSE streams, `slot_id`, sampling fields, tool call parameters, and vision
   inputs without modification.
4. **Supervise** — bounded restart-on-failure with configurable retry limit; shuts down
   gracefully on SIGTERM.

Supports multi-model deployments (one engine instance per model, each on its own port).
Bind address is fail-closed: only binds to configured LAN addresses, never open to all
interfaces by default.

## Usage

```bash
gradatum-engine --config /etc/gradatum/engine-curator.toml
```

## Event-log configuration (optional)

When `gradatum_url` is set, each served request emits a metadata-only event to the
gradatum server (`POST /api/v1/event-log`) — best-effort, never blocking inference.
Events carry **no prompt or response content** (privacy policy, P2-2).

```toml
[engine]
model_path  = "/opt/gradatum/models/qwen3-4b.gguf"
model_kind  = "chat"
port        = 11435
# --- event-log (F-19) ---
gradatum_url = "http://127.0.0.1:19090"   # loopback only — enables the HTTP event sink
agent_id     = "engine-curator"            # semantic emitter id (engine-curator|embed|vision|deep)
```

- `agent_id` (optional): semantic identifier of the emitting engine. Absent = legacy
  behavior (the event's `agent_id` stays null).
- `feature_id` is **derived automatically** from the served route: `/v1/embeddings`
  → `embed`, everything else → `chat`. It is not configured.

## Feature Flags

| Feature | Description |
|---|---|
| `serve` (default) | Compile the Axum HTTP server and llama-server supervisor |

## Anti-cycle invariant

`gradatum-engine` may depend on `gradatum-core` and `gradatum-dto`.
`gradatum-core` and `gradatum-dto` must never depend on `gradatum-engine`.

## License

Apache-2.0
