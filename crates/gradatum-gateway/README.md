# gradatum-gateway

> Unified LLM router — named aliases, multi-provider fallback, circuit-breaker, and reranker endpoint.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-gateway` is the LLM routing layer of gradatum. It exposes an OpenAI-compatible HTTP
API alongside the Anthropic Messages API, and dispatches requests to backend providers via a
configurable alias registry.

Key capabilities:

- **Named aliases** — map logical model names (e.g. `curator`, `embed`, `default`) to concrete
  backend providers (local engine instances, remote HTTP endpoints).
- **Anthropic Messages API** — `POST /v1/messages` (JSON and SSE streaming, full tool use) and
  `POST /v1/messages/count_tokens`, translated to and from the internal OpenAI-shaped format.
- **Circuit-breaker with local fallback** — on primary failure, routes automatically to a
  configured fallback provider. Exception: image (vision) requests never fall back to a
  text-only provider — an explicit 503 is returned instead.
- **Unified chat + embed routing** — single gateway handles both `/v1/chat/completions` and
  `/v1/embeddings` endpoints across all aliases.
- **Reranker endpoint** — `POST /v1/rerank` backed by a `Reranker` implementation.
- **SmartRouter** — alias-aware request dispatch with per-alias parameter overrides.
- **VaultAware hook** — fire-and-forget QaEvent emission for observability.
- **Rate limiting** — per-IP rate limiting using the real TCP socket address.
- **CORS whitelist** — configurable origin allowlist (not permissive by default).
- **Multimodal chat routing** — content-array messages (text + base64 image parts) routed to a vision-capable provider gate.

## Usage

```bash
gradatum-gateway --config /etc/gradatum/gateway.toml
```

## License

Apache-2.0
