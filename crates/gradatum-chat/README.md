# gradatum-chat

> `Chat` and `LlmBackend` traits with heuristic, HTTP, and no-op backends, plus a circuit-breaker decorator.

**Status**: v2.0.0 — public, Apache-2.0. Stable API under SemVer.
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

`CircuitBreakerChat` wraps any `Chat` backend and fails fast with `ChatError::CircuitOpen`
for the cooldown window once the consecutive-failure threshold is reached — the caller
chooses the fallback.

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
gradatum-chat = "2.0.0"
```

## License

Apache-2.0
