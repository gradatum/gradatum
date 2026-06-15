# gradatum-gateway

> Unified LLM router — named aliases, multi-provider fallback, circuit-breaker, and reranker endpoint.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-gateway` is the LLM routing layer of gradatum. It exposes an OpenAI-compatible HTTP
API and dispatches requests to backend providers via a configurable alias registry.

Key capabilities:

- **Named aliases** — map logical model names (e.g. `curator`, `embed`, `default`) to concrete
  backend providers (local engine instances, remote HTTP endpoints).
- **Circuit-breaker with local fallback** — on primary failure, routes automatically to a
  configured fallback provider without surfacing errors to the caller.
- **Unified chat + embed routing** — single gateway handles both `/v1/chat/completions` and
  `/v1/embeddings` endpoints across all aliases.
- **Reranker endpoint** — `POST /v1/rerank` backed by a `Reranker` implementation (F-08).
- **SmartRouter** — alias-aware request dispatch with per-alias parameter overrides.
- **VaultAware hook** — fire-and-forget QaEvent emission for observability.
- **Rate limiting** — per-IP rate limiting using the real TCP socket address.
- **CORS whitelist** — configurable origin allowlist (not permissive by default).
- **Multimodal chat routing** — content-array messages (text + base64 image parts) routed to a vision-capable provider gate — v0.4.3.

## Usage

```bash
gradatum-gateway --config /etc/gradatum/gateway.toml
```

## License

Apache-2.0
