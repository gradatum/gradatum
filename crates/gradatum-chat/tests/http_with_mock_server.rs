//! Tests d'intégration — backend HTTP avec mock server wiremock.
//!
//! 4 scénarios :
//! 1. Réponse JSON propre → parse correct
//! 2. Réponse avec préambule texte → extraction regex du bloc JSON
//! 3. Serveur retourne 502 → ChatError HTTP
//! 4. Contenu non-JSON → ChatError::ParseFailure

mod common;

use common::build_note_with_body;
use gradatum_chat::{Chat, ChatBackend, ChatError, CuratorContext, HttpChat};
use gradatum_core::status::NoteStatus;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn http_chat_parses_openai_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "{\"status\":\"live\",\"confidence\":0.92,\"reason\":\"clear admit\"}"
                }
            }]
        })))
        .mount(&server)
        .await;

    let endpoint = format!("{}/v1/chat/completions", server.uri());
    let chat = HttpChat::new(endpoint, "test-model");
    let note = build_note_with_body("test note content");

    let v = chat
        .classify_curator(&note, &CuratorContext::default())
        .await
        .unwrap();

    assert_eq!(v.confidence, 0.92);
    assert_eq!(v.proposed_status, NoteStatus::Live);
    assert_eq!(v.backend, ChatBackend::Http);
    assert_eq!(v.reason, "clear admit");
}

#[tokio::test]
async fn http_chat_parses_response_with_preamble() {
    let server = MockServer::start().await;

    // Le LLM préfixe sa réponse d'un préambule texte — cas réel avec certains modèles
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Here is the JSON classification:\n{\"status\":\"pending-review\",\"confidence\":0.65,\"reason\":\"ambiguous content\"}"
                }
            }]
        })))
        .mount(&server)
        .await;

    let endpoint = format!("{}/v1/chat/completions", server.uri());
    let chat = HttpChat::new(endpoint, "test-model");
    let note = build_note_with_body("test note content");

    let v = chat
        .classify_curator(&note, &CuratorContext::default())
        .await
        .unwrap();

    assert_eq!(v.confidence, 0.65);
    assert_eq!(v.proposed_status, NoteStatus::PendingReview);
    assert_eq!(v.backend, ChatBackend::Http);
}

#[tokio::test]
async fn http_chat_502_returns_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;

    let endpoint = format!("{}/v1/chat/completions", server.uri());
    let chat = HttpChat::new(endpoint, "test-model");
    let note = build_note_with_body("test note content");

    let result = chat
        .classify_curator(&note, &CuratorContext::default())
        .await;

    assert!(result.is_err(), "un 502 devrait retourner une erreur");
    // reqwest::error_for_status() produit une ChatError::Http wrappant reqwest::Error
    assert!(
        matches!(result, Err(ChatError::Http(_))),
        "une erreur HTTP 502 devrait être ChatError::Http, obtenu: {:?}",
        result
    );
}

#[tokio::test]
async fn http_chat_invalid_json_returns_parse_error() {
    let server = MockServer::start().await;

    // Le LLM retourne du texte pur sans aucun bloc JSON
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Je ne peux pas classifier cette note."
                }
            }]
        })))
        .mount(&server)
        .await;

    let endpoint = format!("{}/v1/chat/completions", server.uri());
    let chat = HttpChat::new(endpoint, "test-model");
    let note = build_note_with_body("test note content");

    let result = chat
        .classify_curator(&note, &CuratorContext::default())
        .await;

    assert!(
        matches!(result, Err(ChatError::ParseFailure(_))),
        "contenu non-JSON devrait retourner ChatError::ParseFailure, obtenu: {:?}",
        result
    );
}
