//! SmartRouter v81 — résolution feature_id → AliasTarget avec paramètres par défaut.
//!
//! Le SmartRouter applique, dans l'ordre de priorité décroissant, les paramètres suivants
//! pour une requête de complétion :
//!
//! 1. Valeurs explicites de la requête client (temperature, max_tokens fournis)
//! 2. Paramètres AgentAware par feature_id (`[gateway."<feature_id>"]`)
//! 3. Paramètres par défaut de l'alias (`temperature_default`, `max_tokens_default`)
//!
//! Usage :
//! - Le handler chat extrait l'en-tête `X-Feature-Id` et appelle `SmartRouter::apply()`.
//! - `apply()` mutate la requête en place si des overrides sont à appliquer.

use crate::commons::chat::ChatCompletionRequest;
use crate::config::{AgentAwareParams, AliasTarget};

/// Applique les paramètres SmartRouter à une requête de complétion.
///
/// Priorité :
/// 1. Valeurs explicites déjà dans la requête (ne pas écraser)
/// 2. AgentAwareParams (par feature_id, si fourni)
/// 3. Valeurs par défaut de l'alias (temperature_default, max_tokens_default)
///
/// Retourne l'alias effectif à utiliser (peut être overridé par AgentAwareParams.alias_override).
///
/// # Effets de bord
/// - Modifie `request.temperature` et/ou `request.max_tokens` si des valeurs par défaut
///   existent et que le client n'a pas fourni ces champs.
pub fn apply(
    request: &mut ChatCompletionRequest,
    alias: &AliasTarget,
    agent_params: Option<&AgentAwareParams>,
) -> AppliedRouting {
    // Résoudre l'alias override depuis AgentAware si présent.
    let alias_override = agent_params.and_then(|p| p.alias_override.as_deref());

    // Appliquer temperature dans l'ordre de priorité.
    if request.temperature.is_none() {
        // Priorité 2 : AgentAware temperature.
        if let Some(t) = agent_params.and_then(|p| p.temperature) {
            request.temperature = Some(t);
        } else if let Some(t) = alias.temperature_default {
            // Priorité 3 : défaut alias.
            request.temperature = Some(t);
        }
    }

    // Appliquer max_tokens dans l'ordre de priorité.
    if request.max_tokens.is_none() {
        // Priorité 2 : AgentAware max_tokens.
        if let Some(n) = agent_params.and_then(|p| p.max_tokens) {
            request.max_tokens = Some(n);
        } else if let Some(n) = alias.max_tokens_default {
            // Priorité 3 : défaut alias.
            request.max_tokens = Some(n);
        }
    }

    AppliedRouting {
        alias_override: alias_override.map(|s| s.to_owned()),
    }
}

/// Résultat de l'application du SmartRouter.
#[derive(Debug, Clone)]
pub struct AppliedRouting {
    /// Alias overridé par AgentAwareParams, si applicable.
    ///
    /// `None` = pas d'override, utiliser l'alias résolu normalement.
    /// `Some(alias)` = utiliser cet alias à la place.
    pub alias_override: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commons::chat::{Message, Role};
    use crate::config::AgentAwareParams;

    fn make_request(temperature: Option<f32>, max_tokens: Option<u32>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: "bonjour".to_string(),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
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
