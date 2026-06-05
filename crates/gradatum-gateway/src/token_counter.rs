//! Comptage heuristique de tokens pour la validation du cap de contexte.
//!
//! Heuristique chars/4 calibrée sur les LLMs modernes (ratio 3.7-4.0 chars/token).
//! Conservative par design : mieux vaut rejeter légèrement tôt que risquer
//! un freeze backend sur corpus trop large.

use crate::commons::chat::ChatCompletionRequest;

const TOKENS_PER_MESSAGE_OVERHEAD: u64 = 4;
const CHARS_PER_TOKEN: u64 = 4;

/// Estime le nombre total de tokens pour une requête chat (heuristique chars/4).
///
/// Total = Σ(tokens_par_message) + max_tokens_demandé
#[must_use]
pub fn estimate_total_tokens(request: &ChatCompletionRequest) -> u64 {
    let input_tokens = estimate_input_tokens(request);
    let output_tokens = request.max_tokens.map(|n| n as u64).unwrap_or(0);
    input_tokens.saturating_add(output_tokens)
}

/// Estime les tokens d'entrée uniquement (sans max_tokens).
#[must_use]
pub fn estimate_input_tokens(request: &ChatCompletionRequest) -> u64 {
    request
        .messages
        .iter()
        .map(|msg| {
            let content_chars = msg.content.chars().count() as u64;
            let content_tokens = content_chars.div_ceil(CHARS_PER_TOKEN);
            content_tokens.saturating_add(TOKENS_PER_MESSAGE_OVERHEAD)
        })
        .fold(0u64, |acc, t| acc.saturating_add(t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commons::chat::Message;

    fn make_request(messages: Vec<Message>, max_tokens: Option<u32>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "test-model".to_string(),
            messages,
            max_tokens,
            stream: None,
            temperature: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            chat_template_kwargs: None,
        }
    }

    #[test]
    fn test_vide_retourne_zero_input() {
        let req = make_request(vec![], None);
        assert_eq!(estimate_input_tokens(&req), 0);
    }

    #[test]
    fn test_message_simple_calcul_correct() {
        let content = "a".repeat(40);
        let req = make_request(vec![Message::user(&content)], None);
        assert_eq!(
            estimate_input_tokens(&req),
            14,
            "40 chars / 4 + 4 overhead = 14"
        );
    }

    #[test]
    fn test_max_tokens_ajouté_au_total() {
        let content = "a".repeat(40);
        let req = make_request(vec![Message::user(&content)], Some(100));
        assert_eq!(
            estimate_total_tokens(&req),
            114,
            "14 input + 100 max_tokens = 114"
        );
    }

    #[test]
    fn test_saturation_pas_de_panique() {
        let content = "x".repeat(1_000_000);
        let req = make_request(vec![Message::user(&content)], Some(u32::MAX));
        let _ = estimate_total_tokens(&req);
    }
}
