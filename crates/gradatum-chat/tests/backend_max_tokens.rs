//! Tests — propagation de `max_tokens` dans les backends OpenAI-compat et Anthropic.
//!
//! Vérifie que :
//! 1. `OpenAiCompatBackend::with_max_tokens(N)` propage N dans la requête HTTP body.
//! 2. `AnthropicCompatBackend::with_max_tokens(N)` propage N dans la requête Anthropic body.
//! 3. Le défaut (sans `with_max_tokens`) est 1024 dans les deux backends.
//!
//! Les mocks wiremock utilisent `body_partial_json` pour vérifier le champ `max_tokens`
//! dans le body de la requête sans inspecter l'intégralité du payload.

use gradatum_chat::anthropic_compat::AnthropicCompatBackend;
use gradatum_chat::backend::LlmBackend;
use gradatum_chat::openai_compat::OpenAiCompatBackend;
use secrecy::SecretString;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn openai_response_json(section: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": serde_json::json!({
                    "section": section,
                    "tags": [],
                    "wikilinks": [],
                    "duplicate_hint": null
                }).to_string()
            }
        }]
    })
}

fn anthropic_response_json(section: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "text",
            "text": serde_json::json!({
                "section": section,
                "tags": [],
                "wikilinks": [],
                "duplicate_hint": null
            }).to_string()
        }],
        "model": "claude-haiku-4-5",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 20}
    })
}

// ── Tests OpenAiCompatBackend ─────────────────────────────────────────────────

/// `with_max_tokens(512)` doit être visible dans le body JSON envoyé au serveur.
///
/// Le mock wiremock matche uniquement si le body contient `"max_tokens": 512`.
/// Si `with_max_tokens` ne propage pas la valeur, le mock ne matche pas et le
/// test échoue sur `expect(1)` (appel attendu mais non reçu par le mock).
#[tokio::test]
async fn openai_compat_with_max_tokens_propagates_to_request_body() {
    let mock_server = MockServer::start().await;

    // body_partial_json matche si le body JSON contient au moins ces champs.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({ "max_tokens": 512_u32 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_json("decisions")))
        .expect(1)
        .mount(&mock_server)
        .await;

    let backend = OpenAiCompatBackend::new(
        mock_server.uri(),
        "test-model".to_string(),
        SecretString::new("".to_string().into()),
    )
    .with_max_tokens(512);

    let result = backend.classify("system prompt", "user prompt").await;

    assert!(
        result.is_ok(),
        "la requête avec max_tokens=512 doit réussir : {:?}",
        result
    );
    // Le mock (.expect(1)) vérifie que le body contenait bien max_tokens=512.
}

/// Défaut sans `with_max_tokens` : doit envoyer max_tokens = 1024.
///
/// Aligné sur le gatekeeper legacy (`max_tokens: Some(1024)`).
#[tokio::test]
async fn openai_compat_default_max_tokens_is_1024() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({ "max_tokens": 1024_u32 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_json("reasoning")))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Pas de with_max_tokens → défaut 1024
    let backend = OpenAiCompatBackend::new(
        mock_server.uri(),
        "test-model".to_string(),
        SecretString::new("".to_string().into()),
    );

    let result = backend.classify("system prompt", "user prompt").await;

    assert!(
        result.is_ok(),
        "la requête avec max_tokens défaut (1024) doit réussir : {:?}",
        result
    );
}

// ── Tests AnthropicCompatBackend ──────────────────────────────────────────────

/// `with_max_tokens(2048)` doit être visible dans le body JSON Anthropic.
#[tokio::test]
async fn anthropic_compat_with_max_tokens_propagates_to_request_body() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(
            serde_json::json!({ "max_tokens": 2048_u32 }),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(anthropic_response_json("architecture")),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let backend = AnthropicCompatBackend::new(
        SecretString::new("sk-ant-test".to_string().into()),
        "claude-haiku-4-5".to_string(),
    )
    .with_base_url(mock_server.uri())
    .with_max_tokens(2048);

    let result = backend.classify("system prompt", "user prompt").await;

    assert!(
        result.is_ok(),
        "la requête Anthropic avec max_tokens=2048 doit réussir : {:?}",
        result
    );
}

/// Défaut Anthropic sans `with_max_tokens` : doit envoyer max_tokens = 1024.
#[tokio::test]
async fn anthropic_compat_default_max_tokens_is_1024() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(
            serde_json::json!({ "max_tokens": 1024_u32 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_json("debug")))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Pas de with_max_tokens → défaut 1024
    let backend = AnthropicCompatBackend::new(
        SecretString::new("sk-ant-test".to_string().into()),
        "claude-haiku-4-5".to_string(),
    )
    .with_base_url(mock_server.uri());

    let result = backend.classify("system prompt", "user prompt").await;

    assert!(
        result.is_ok(),
        "la requête Anthropic avec max_tokens défaut (1024) doit réussir : {:?}",
        result
    );
}
