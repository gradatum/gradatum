//! # gradatum-chat
//!
//! LLM classification backends for the gradatum curator.
//!
//! ## Trait `Chat` — note-oriented classification
//!
//! Note-oriented trait: `Note` → `CuratorVerdict`. Used by `gradatum-curator::Curator<C>`.
//!
//! | Backend | Network dependency | Use case |
//! |---|---|---|
//! | [`heuristic::Heuristic`] | None | Offline classification |
//! | [`http::HttpChat`] | Yes (OpenAI-compat) | Any OpenAI-compatible LLM endpoint |
//! | [`noop::Noop`] | None | Tests / safe fallback |
//!
//! ## Trait `LlmBackend` — low-level classification
//!
//! Low-level prompt-oriented trait (`system` + `user`) → `CuratorDecision`.
//! Five backends plus a circuit breaker with transparent fallback.
//!
//! | Backend | Protocol | Use case |
//! |---|---|---|
//! | [`heuristic_routing::HeuristicBackend`] | Offline | Default OSS, no network |
//! | [`openai_compat::OpenAiCompatBackend`] | OpenAI API v1 | OpenAI / llama.cpp / any OpenAI-compat host |
//! | [`ollama_compat::OllamaCompatBackend`] | Ollama `/api/chat` | Local Ollama |
//! | [`anthropic_compat::AnthropicCompatBackend`] | Anthropic Messages | Claude Haiku/Sonnet |
//! | [`gemini_compat::GeminiCompatBackend`] | Gemini generateContent | Gemini Flash/Pro |
//!
//! ## Circuit breaker `LlmBackend`
//!
//! [`circuit_breaker_llm::CircuitBreaker`] wrapping `LlmBackend` with exponential
//! backoff (30→60→120→300 s) and transparent fallback to `HeuristicBackend`.
//!
//! ## Circuit breaker `Chat` (legacy)
//!
//! [`circuit_breaker::CircuitBreakerChat`] wrapping `Chat` — legacy, not wired in `1.0.0`
//! (no construction site outside this crate).
//!
//! ## Stability
//!
//! `1.0.0` — public API under [SemVer 2.0.0](https://semver.org); backward-compatible additions only within `1.x`.
//! See [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

// --- Trait Chat ---

pub mod chat_trait;
pub mod circuit_breaker;
pub mod error;
pub mod heuristic;
pub mod http;
pub mod noop;

// --- Trait LlmBackend ---

pub mod anthropic_compat;
pub mod backend;
pub mod circuit_breaker_llm;
pub mod gemini_compat;
pub mod heuristic_routing;
pub mod ollama_compat;
pub mod openai_compat;

// --- Re-exports Chat ---

pub use chat_trait::{Chat, ChatBackend, CuratorContext, CuratorVerdict};
pub use circuit_breaker::CircuitBreakerChat;
pub use error::ChatError;
pub use heuristic::Heuristic;
pub use http::HttpChat;
pub use noop::Noop;

// --- Re-exports LlmBackend ---

pub use anthropic_compat::AnthropicCompatBackend;
pub use backend::{CuratorDecision, LlmBackend, LlmError};
pub use circuit_breaker_llm::{CircuitBreaker, CircuitConfig};
pub use gemini_compat::GeminiCompatBackend;
pub use heuristic_routing::HeuristicBackend;
pub use ollama_compat::OllamaCompatBackend;
pub use openai_compat::OpenAiCompatBackend;

/// Crate version, taken from `workspace.package.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns `true` if the URL points to a local host (loopback or RFC 1918 private range).
///
/// Used by `LlmBackend::is_local()` implementations to classify a URL as local.
/// Ranges considered local:
/// - `localhost` (loopback resolution)
/// - IPv4 loopback `127.x.x.x`
/// - RFC 1918 class A: `10.x.x.x`
/// - RFC 1918 class B: `172.16-31.x.x`
/// - RFC 1918 class C: first octet 192, second octet 168
pub(crate) fn is_local_url(url: &str) -> bool {
    // Extraire l'hôte depuis l'URL (format http://host:port/...).
    let host = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .map(|s| s.split('/').next().unwrap_or(s))
        .map(|h| h.split(':').next().unwrap_or(h))
        .unwrap_or(url);

    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                let octets = v4.octets();
                // 127.0.0.0/8 — loopback
                octets[0] == 127
                // 10.0.0.0/8 — RFC 1918 classe A
                || octets[0] == 10
                // 172.16.0.0/12 — RFC 1918 classe B
                || (octets[0] == 172 && (octets[1] >= 16 && octets[1] <= 31))
                // RFC 1918 classe C (octets 192 / 168)
                || (octets[0] == 192 && octets[1] == 168)
            }
            std::net::IpAddr::V6(v6) => v6.is_loopback(),
        };
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
