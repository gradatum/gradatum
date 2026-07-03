# gradatum-chat

> `Chat` and `LlmBackend` traits with heuristic, HTTP, and no-op backends, plus a circuit-breaker decorator.

**Status**: Alpha (v0.7.6) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-chat` provides the LLM classification backends used by `gradatum-curator`.
It exposes two trait levels:

**`Chat` trait** (high-level, note-oriented)

| Backend | Network | Description |
|---|---|---|
| `Heuristic` | None | Regex-based offline classifier — default for air-gapped deployments |
| `HttpChat` | Yes | Any OpenAI-compatible `/v1/chat/completions` endpoint |
| `Noop` | None | Returns a fixed verdict — tests and disabled-LLM configurations |

`CircuitBreakerChat` wraps any `Chat` backend with automatic fallback to the heuristic
on consecutive failures.

**`LlmBackend` trait** (low-level, prompt-oriented)

| Backend | Protocol | Description |
|---|---|---|
| `HeuristicBackend` | Offline | Default OSS, no network required |
| `OpenAiCompatBackend` | OpenAI v1 | OpenAI, llama.cpp, or any OpenAI-compatible host |
| `OllamaCompatBackend` | Ollama | Local Ollama `/api/chat` |
| `AnthropicCompatBackend` | Anthropic Messages | Claude models |
| `GeminiCompatBackend` | Gemini | Gemini Flash/Pro |

`CircuitBreaker<B>` wraps any `LlmBackend` with exponential backoff (30→60→120→300 s)
and transparent fallback to `HeuristicBackend` on consecutive failures.
`CircuitConfig` controls the failure threshold and backoff window.

## Usage

```toml
[dependencies]
gradatum-chat = "0.7.6"
```

## License

Apache-2.0
