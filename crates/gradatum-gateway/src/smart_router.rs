//! SmartRouter — resolves `feature_id` to an `AliasTarget` with layered default parameters.
//!
//! Applies the following parameters to a completion request in decreasing priority order:
//!
//! 1. Explicit values from the client request (`temperature`, `max_tokens` when provided)
//! 2. AgentAware parameters for the `feature_id` (`[gateway."<feature_id>"]`)
//! 3. Alias defaults (`temperature_default`, `max_tokens_default`)
//!
//! Usage:
//! - The chat handler extracts the `X-Feature-Id` header and calls `apply()`.
//! - `apply()` mutates the request in place when overrides apply.

use crate::commons::chat::ChatCompletionRequest;
use crate::config::{AgentAwareParams, AliasTarget};

/// Applies SmartRouter parameters to a completion request.
///
/// Priority:
/// 1. Explicit values already in the request (never overwritten)
/// 2. `AgentAwareParams` (by `feature_id`, when provided)
/// 3. Alias defaults (`temperature_default`, `max_tokens_default`)
///
/// Returns the effective alias to use (may be overridden by `AgentAwareParams.alias_override`).
///
/// # Side effects
///
/// Mutates `request.temperature` and/or `request.max_tokens` when defaults exist
/// and the client did not supply those fields.
pub fn apply(
    request: &mut ChatCompletionRequest,
    alias: &AliasTarget,
    agent_params: Option<&AgentAwareParams>,
) -> AppliedRouting {
    // Resolve alias override from AgentAware when present.
    let alias_override = agent_params.and_then(|p| p.alias_override.as_deref());

    // Apply temperature in priority order.
    if request.temperature.is_none() {
        // Priority 2: AgentAware temperature.
        if let Some(t) = agent_params.and_then(|p| p.temperature) {
            request.temperature = Some(t);
        } else if let Some(t) = alias.temperature_default {
            // Priority 3: alias default.
            request.temperature = Some(t);
        }
    }

    // Apply max_tokens in priority order.
    if request.max_tokens.is_none() {
        // Priority 2: AgentAware max_tokens.
        if let Some(n) = agent_params.and_then(|p| p.max_tokens) {
            request.max_tokens = Some(n);
        } else if let Some(n) = alias.max_tokens_default {
            // Priority 3: alias default.
            request.max_tokens = Some(n);
        }
    }

    AppliedRouting {
        alias_override: alias_override.map(|s| s.to_owned()),
    }
}

/// Result of applying the SmartRouter.
#[derive(Debug, Clone)]
pub struct AppliedRouting {
    /// Alias overridden by `AgentAwareParams`, if applicable.
    ///
    /// `None` = no override; use the normally resolved alias.
    /// `Some(alias)` = use this alias instead.
    pub alias_override: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commons::chat::Message;
    use crate::config::AgentAwareParams;

    fn make_request(temperature: Option<f32>, max_tokens: Option<u32>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![Message::user("bonjour")],
            max_tokens,
            stream: None,
            temperature,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            chat_template_kwargs: None,
        }
    }

    fn make_alias(temp: Option<f32>, max_tokens: Option<u32>) -> AliasTarget {
        AliasTarget {
            provider: "p".to_string(),
            model: "m".to_string(),
            fallback_provider: None,
            fallback_model: None,
            temperature_default: temp,
            max_tokens_default: max_tokens,
            vision_capable: false,
        }
    }

    #[test]
    fn test_no_override_when_request_has_values() {
        let mut req = make_request(Some(0.5), Some(100));
        let alias = make_alias(Some(0.9), Some(200));
        let result = apply(&mut req, &alias, None);
        // Les valeurs explicites ne doivent pas être écrasées.
        assert_eq!(req.temperature, Some(0.5));
        assert_eq!(req.max_tokens, Some(100));
        assert!(result.alias_override.is_none());
    }

    #[test]
    fn test_alias_defaults_applied_when_request_empty() {
        let mut req = make_request(None, None);
        let alias = make_alias(Some(0.7), Some(512));
        apply(&mut req, &alias, None);
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.max_tokens, Some(512));
    }

    #[test]
    fn test_agent_params_override_alias_defaults() {
        let mut req = make_request(None, None);
        let alias = make_alias(Some(0.7), Some(512));
        let agent = AgentAwareParams {
            temperature: Some(0.1),
            max_tokens: Some(1024),
            alias_override: None,
        };
        apply(&mut req, &alias, Some(&agent));
        // AgentAware prime sur les défauts alias.
        assert_eq!(req.temperature, Some(0.1));
        assert_eq!(req.max_tokens, Some(1024));
    }

    #[test]
    fn test_request_values_prime_over_agent_params() {
        let mut req = make_request(Some(0.3), Some(50));
        let alias = make_alias(Some(0.7), Some(512));
        let agent = AgentAwareParams {
            temperature: Some(0.1),
            max_tokens: Some(1024),
            alias_override: None,
        };
        apply(&mut req, &alias, Some(&agent));
        // Les valeurs explicites de la requête ne doivent pas être modifiées.
        assert_eq!(req.temperature, Some(0.3));
        assert_eq!(req.max_tokens, Some(50));
    }

    #[test]
    fn test_alias_override_returned() {
        let mut req = make_request(None, None);
        let alias = make_alias(None, None);
        let agent = AgentAwareParams {
            temperature: None,
            max_tokens: None,
            alias_override: Some("other-alias".to_string()),
        };
        let result = apply(&mut req, &alias, Some(&agent));
        assert_eq!(result.alias_override.as_deref(), Some("other-alias"));
    }

    #[test]
    fn test_no_alias_defaults_no_change() {
        let mut req = make_request(None, None);
        let alias = make_alias(None, None);
        apply(&mut req, &alias, None);
        assert!(req.temperature.is_none());
        assert!(req.max_tokens.is_none());
    }
}
