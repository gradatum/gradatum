//! Heuristic token counting for context cap validation.
//!
//! Uses a chars/4 heuristic calibrated on modern LLMs (3.7–4.0 chars/token ratio).
//! Intentionally conservative: it is safer to reject slightly early than to risk
//! a backend freeze on an oversized corpus.
//!
//! # Multimodal images
//!
//! `ImageUrl` parts are estimated at `TOKENS_PER_IMAGE` tokens (named constant).
//! The magnitude is calibrated on mmproj cost for vision models (e.g. LLaVA, Qwen-VL).
//! The constant is intentionally conservative to cover high-resolution images.

use crate::commons::chat::ChatCompletionRequest;

const TOKENS_PER_MESSAGE_OVERHEAD: u64 = 4;
const CHARS_PER_TOKEN: u64 = 4;

/// Estimated token cost of a single image for heuristic counting.
///
/// mmproj order of magnitude: ~1100 tokens/image (standard 336×336 resolution,
/// 24 tiles × 256 / 5.5 compression ratio). Conservative to cover high-resolution
/// images (anyres / dynamic tiling).
const TOKENS_PER_IMAGE: u64 = 1100;

/// Estimates the total token count for a chat request (chars/4 heuristic).
///
/// Total = Σ(tokens_per_message) + requested `max_tokens`
#[must_use]
pub fn estimate_total_tokens(request: &ChatCompletionRequest) -> u64 {
    let input_tokens = estimate_input_tokens(request);
    let output_tokens = request.max_tokens.map(|n| n as u64).unwrap_or(0);
    input_tokens.saturating_add(output_tokens)
}

/// Estimates input tokens only (excluding `max_tokens`).
///
/// For multimodal messages (`Parts`):
/// - text parts: chars/4 heuristic
/// - image parts: `TOKENS_PER_IMAGE` tokens each
#[must_use]
pub fn estimate_input_tokens(request: &ChatCompletionRequest) -> u64 {
    request
        .messages
        .iter()
        .map(|msg| {
            let text = msg.content.text_content();
            let content_chars = text.chars().count() as u64;
            let text_tokens = content_chars.div_ceil(CHARS_PER_TOKEN);
            let image_tokens = (msg.content.image_count() as u64).saturating_mul(TOKENS_PER_IMAGE);
            text_tokens
                .saturating_add(image_tokens)
                .saturating_add(TOKENS_PER_MESSAGE_OVERHEAD)
        })
        .fold(0u64, |acc, t| acc.saturating_add(t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commons::chat::{ContentPart, ImageUrlDetail, Message, MessageContent};

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

    /// Parts multimodales : texte seul → même calcul que Text.
    #[test]
    fn test_parts_text_only_equiv_text() {
        let text = "a".repeat(40);
        let msg_text = Message::user(&text);
        let msg_parts = Message {
            role: crate::commons::chat::Role::User,
            content: MessageContent::Parts(vec![ContentPart::Text { text: text.clone() }]),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let req_text = make_request(vec![msg_text], None);
        let req_parts = make_request(vec![msg_parts], None);
        assert_eq!(
            estimate_input_tokens(&req_text),
            estimate_input_tokens(&req_parts),
            "Parts(text only) doit produire le même résultat que Text"
        );
    }

    /// Une image → TOKENS_PER_IMAGE tokens supplémentaires.
    #[test]
    fn test_parts_image_compte_tokens_image() {
        // PNG 1×1 minimal (test canonique).
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let msg = Message {
            role: crate::commons::chat::Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "décris cette image".to_string(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrlDetail {
                        url: format!("data:image/png;base64,{}", png_b64),
                    },
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let req = make_request(vec![msg], None);
        let tokens = estimate_input_tokens(&req);
        // "décris cette image" = 18 chars → 18/4 = 5 (div_ceil) + 4 overhead + 1100 image
        let text_tokens: u64 = "décris cette image".chars().count() as u64;
        let expected = text_tokens.div_ceil(4) + 4 + TOKENS_PER_IMAGE;
        assert_eq!(
            tokens, expected,
            "1 image = TOKENS_PER_IMAGE ({}) tokens supplémentaires",
            TOKENS_PER_IMAGE
        );
    }

    /// Plusieurs images → coût × nombre d'images.
    #[test]
    fn test_parts_two_images_double_cost() {
        let url = "data:image/png;base64,abc";
        let msg = Message {
            role: crate::commons::chat::Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::ImageUrl {
                    image_url: ImageUrlDetail {
                        url: url.to_string(),
                    },
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrlDetail {
                        url: url.to_string(),
                    },
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let req = make_request(vec![msg], None);
        let tokens = estimate_input_tokens(&req);
        // 0 chars texte + 4 overhead + 2 × TOKENS_PER_IMAGE
        let expected = 4u64.saturating_add(2 * TOKENS_PER_IMAGE);
        assert_eq!(tokens, expected, "2 images = 2 × TOKENS_PER_IMAGE");
    }
}
