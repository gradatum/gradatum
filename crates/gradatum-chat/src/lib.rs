//! # gradatum-chat
//!
//! Backends de classification LLM pour le curator gradatum.
//!
//! ## Architecture Phase 1 — Trait `Chat`
//!
//! Trait orienté `Note` → `CuratorVerdict`. Utilisé par `gradatum-curator::Curator<C>`.
//!
//! | Backend | Dépendance réseau | Cas d'usage |
//! |---|---|---|
//! | [`heuristic::Heuristic`] | Aucune | Classification offline invariant #3/R1 |
//! | [`http::HttpChat`] | Oui (OpenAI-compat) | LLM gateway-v2 / any OpenAI-compat host (D-perf-3) |
//! | [`noop::Noop`] | Aucune | Tests / fallback safe |
//!
//! ## Architecture Phase 2 — Trait `LlmBackend`
//!
//! Trait bas niveau orienté prompt (`system` + `user`) → `CuratorDecision`.
//! 5 backends + circuit breaker avec fallback transparent.
//!
//! | Backend | Protocole | Cas d'usage |
//! |---|---|---|
//! | [`heuristic_routing::HeuristicBackend`] | Offline | Default OSS, aucun réseau |
//! | [`openai_compat::OpenAiCompatBackend`] | OpenAI API v1 | OpenAI / llama.cpp / any OpenAI-compat host |
//! | [`ollama_compat::OllamaCompatBackend`] | Ollama `/api/chat` | Ollama local |
//! | [`anthropic_compat::AnthropicCompatBackend`] | Anthropic Messages | Claude Haiku/Sonnet |
//! | [`gemini_compat::GeminiCompatBackend`] | Gemini generateContent | Gemini Flash/Pro |
//!
//! ## Circuit breaker Phase 2
//!
//! [`circuit_breaker_llm::CircuitBreaker`] wrappant `LlmBackend` avec backoff
//! exponentiel 30→60→120→300s et fallback transparent vers `HeuristicBackend`.
//!
//! ## Circuit breaker Phase 1 (legacy)
//!
//! [`circuit_breaker::CircuitBreakerChat`] wrappant `Chat` — utilisé par `gradatum-curator`.
//!
//! ## Stability
//!
//! `0.x` — pas de garantie de stabilité d'API. Phase 1 = baseline fonctionnel.
//! Voir [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

// --- Phase 1 : trait Chat ---

pub mod chat_trait;
pub mod circuit_breaker;
pub mod error;
pub mod heuristic;
pub mod http;
pub mod noop;

// --- Phase 2 : trait LlmBackend ---

pub mod anthropic_compat;
pub mod backend;
pub mod circuit_breaker_llm;
pub mod gemini_compat;
pub mod heuristic_routing;
pub mod ollama_compat;
pub mod openai_compat;

// --- Re-exports Phase 1 ---

pub use chat_trait::{Chat, ChatBackend, CuratorContext, CuratorVerdict};
pub use circuit_breaker::CircuitBreakerChat;
pub use error::ChatError;
pub use heuristic::Heuristic;
pub use http::HttpChat;
pub use noop::Noop;

// --- Re-exports Phase 2 ---

pub use anthropic_compat::AnthropicCompatBackend;
pub use backend::{CuratorDecision, LlmBackend, LlmError};
pub use circuit_breaker_llm::{CircuitBreaker, CircuitConfig};
pub use gemini_compat::GeminiCompatBackend;
pub use heuristic_routing::HeuristicBackend;
pub use ollama_compat::OllamaCompatBackend;
pub use openai_compat::OpenAiCompatBackend;

/// Version du crate (issue du `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Détermine si une URL pointe vers un hôte local (loopback ou réseau privé RFC 1918).
///
/// Utilisé par les backends `LlmBackend::is_local()` pour classifier une URL comme locale.
/// Les plages considérées locales :
/// - `localhost` (résolution loopback)
/// - Loopback IPv4 `127.x.x.x`
/// - RFC 1918 classe A : `10.x.x.x`
/// - RFC 1918 classe B : `172.16-31.x.x`
/// - RFC 1918 classe C : premier octet 192, second octet 168
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
