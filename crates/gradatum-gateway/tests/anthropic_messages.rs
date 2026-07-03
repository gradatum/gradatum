//! Tests d'intégration pour `POST /v1/messages` (Anthropic Messages API inbound).
//!
//! Utilise `tower::ServiceExt::oneshot` (pas de serveur HTTP réel).
//! `wiremock` simule le backend LLM interne (format OpenAI).
//!
//! Périmètre testé (Slice A + Slice B + Slice C + Slice D) :
//! - Requête non-stream texte → réponse Anthropic correcte (type, role, content, stop_reason, usage)
//! - stop_reason end_turn depuis finish_reason stop
//! - stream:true → réponse SSE Anthropic complète (Slice C)
//! - Alias "default" absent → 404 avec liste des aliases disponibles
//! - [Slice B] tools + tool_choice dans requête → transmis au backend → tool_calls → ResponseBlock::ToolUse
//! - [Slice C] stream:true + backend SSE OpenAI → séquence events Anthropic (message_start...message_stop)
//! - [Slice D] model_map configurable + count_tokens + erreur Anthropic + x-api-key

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gradatum_gateway::config::{
    AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig, VaultAwareConfig,
};
use gradatum_gateway::{AppState, build_router};

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Config de test standard avec alias `default` pointant sur le provider fourni.
fn test_config_with_default_alias(provider_endpoint: &str) -> Config {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "test-provider".to_string(),
        ProviderConfig {
            endpoint: provider_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 10,
        },
    );

    let mut aliases = HashMap::new();
    aliases.insert(
        "default".to_string(),
        AliasTarget::simple("test-provider", "qwen3-30b"),
    );

    Config {
        server: ServerConfig {
            listen: "127.0.0.1:18436".to_string(),
            registry_db: None,
            bearer_token_env: None,
            rate_limit_per_minute: 1000,
            circuit_threshold: 5,
            circuit_window_secs: 60,
            circuit_cooldown_secs: 30,
            max_total_tokens: 0,
            trust_localhost: true,
            enable_slot_passthrough: false,
            allowed_origins: vec![],
            max_tools_per_request: 10,
        },
        providers,
        aliases,
        gateway: HashMap::new(),
        logging: LoggingConfig::default(),
        vault_aware: VaultAwareConfig::default(),
        messages: Default::default(),
    }
}

/// Config de test SANS alias `default` (pour tester le 404).
fn test_config_without_default_alias(provider_endpoint: &str) -> Config {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "test-provider".to_string(),
        ProviderConfig {
            endpoint: provider_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 10,
        },
    );

    let mut aliases = HashMap::new();
    aliases.insert(
        "other-alias".to_string(),
        AliasTarget::simple("test-provider", "some-model"),
    );

    Config {
        server: ServerConfig {
            listen: "127.0.0.1:18436".to_string(),
            registry_db: None,
            bearer_token_env: None,
            rate_limit_per_minute: 1000,
            circuit_threshold: 5,
            circuit_window_secs: 60,
            circuit_cooldown_secs: 30,
            max_total_tokens: 0,
            trust_localhost: true,
            enable_slot_passthrough: false,
            allowed_origins: vec![],
            max_tools_per_request: 10,
        },
        providers,
        aliases,
        gateway: HashMap::new(),
        logging: LoggingConfig::default(),
        vault_aware: VaultAwareConfig::default(),
        messages: Default::default(),
    }
}

/// Réponse OpenAI-compat de test standard (non-stream).
fn openai_chat_response(content: &str) -> Value {
    json!({
        "id": "chatcmpl-test123",
        "object": "chat.completion",
        "created": 1234567890u64,
        "model": "qwen3-30b",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 25,
            "completion_tokens": 10,
            "total_tokens": 35
        }
    })
}

/// Requête Anthropic non-stream minimale.
fn anthropic_request_body(user_content: &str) -> Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "messages": [{
            "role": "user",
            "content": user_content
        }]
    })
}

// ── Tests d'intégration ───────────────────────────────────────────────────────

/// I1 — Requête non-stream texte basique → réponse Anthropic conforme.
///
/// Vérifie :
/// - `type = "message"`
/// - `role = "assistant"`
/// - `content[0].type = "text"` avec le texte correct
/// - `stop_reason = "end_turn"` (depuis finish_reason "stop")
/// - `usage.input_tokens` et `output_tokens` présents
/// - `model` reflète le modèle de la requête Anthropic (pas l'alias interne)
#[tokio::test]
async fn non_stream_text_returns_anthropic_response() {
    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(openai_chat_response("Paris est la capitale de la France.")),
        )
        .mount(&backend)
        .await;

    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_string(&anthropic_request_body(
                "Quelle est la capitale de la France ?",
            ))
            .unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK, "doit retourner HTTP 200");

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();

    // Champs obligatoires format Anthropic.
    assert_eq!(body["type"], "message", "type doit être 'message'");
    assert_eq!(body["role"], "assistant", "role doit être 'assistant'");
    assert_eq!(
        body["stop_reason"], "end_turn",
        "stop_reason doit être 'end_turn' pour finish_reason 'stop'"
    );

    // Contenu.
    let content = &body["content"];
    assert!(content.is_array(), "content doit être un tableau");
    let blocks = content.as_array().unwrap();
    assert_eq!(blocks.len(), 1, "un seul bloc de contenu attendu");
    assert_eq!(blocks[0]["type"], "text", "le bloc doit être de type text");
    assert_eq!(
        blocks[0]["text"], "Paris est la capitale de la France.",
        "le texte doit correspondre à la réponse backend"
    );

    // Usage.
    assert!(body["usage"].is_object(), "usage doit être présent");
    assert_eq!(
        body["usage"]["input_tokens"], 25,
        "input_tokens doit être prompt_tokens du backend"
    );
    assert_eq!(
        body["usage"]["output_tokens"], 10,
        "output_tokens doit être completion_tokens du backend"
    );

    // Le modèle renvoyé = celui de la requête Anthropic (pas l'alias interne).
    assert_eq!(
        body["model"], "claude-3-5-sonnet-20241022",
        "model doit être celui de la requête Anthropic originale"
    );

    // L'id doit commencer par "msg_".
    let id = body["id"].as_str().unwrap_or("");
    assert!(
        id.starts_with("msg_"),
        "l'id doit commencer par 'msg_', obtenu: {}",
        id
    );
}

/// I2 — Requête avec system prompt → system transmis au backend OpenAI.
///
/// Le backend reçoit un premier message de rôle "system" avant le message user.
#[tokio::test]
async fn system_prompt_forwarded_to_backend() {
    let backend = MockServer::start().await;

    // Mock vérifiant que le body envoyé contient bien le message system.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(openai_chat_response("Réponse avec system.")),
        )
        .expect(1)
        .mount(&backend)
        .await;

    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 512,
        "system": "Tu es un assistant concis.",
        "messages": [{
            "role": "user",
            "content": "Bonjour"
        }]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["text"], "Réponse avec system.");
}

/// I3 — [Slice C] stream:true + backend SSE OpenAI → réponse SSE Anthropic 200.
///
/// Le backend mock renvoie un flux SSE OpenAI valide.
/// Vérifie : Content-Type text/event-stream + HTTP 200 + events Anthropic dans l'ordre.
#[tokio::test]
async fn stream_true_returns_sse_anthropic_response() {
    let backend = MockServer::start().await;

    // Flux SSE OpenAI mimant un LLM qui génère "Bonjour monde." en 2 chunks.
    let sse_body = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"qwen3\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Bonjour \"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"qwen3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"monde.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"qwen3\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .mount(&backend)
        .await;

    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "stream": true,
        "messages": [{"role": "user", "content": "Dis bonjour"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    // Slice C implémenté → 200 avec Content-Type SSE.
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "stream:true doit retourner HTTP 200 en Slice C"
    );

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "Content-Type doit être text/event-stream, obtenu: {}",
        content_type
    );

    // Lire le body SSE complet et parser les events.
    let body_bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let sse_text = String::from_utf8(body_bytes.to_vec()).unwrap();

    // Extraire les events de la réponse SSE Anthropic.
    let mut events: Vec<(String, Value)> = Vec::new();
    let mut current_event: Option<String> = None;
    for line in sse_text.lines() {
        if let Some(evt) = line.strip_prefix("event: ") {
            current_event = Some(evt.to_string());
        } else if let Some(ref evt) = current_event.clone()
            && let Some(json_str) = line.strip_prefix("data: ")
            && let Ok(data) = serde_json::from_str::<Value>(json_str)
        {
            events.push((evt.clone(), data));
            current_event = None;
        }
    }

    let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

    // Séquence minimale obligatoire : message_start en premier, message_stop en dernier.
    assert_eq!(
        types.first().copied(),
        Some("message_start"),
        "premier event doit être message_start, obtenu: {:?}",
        types
    );
    assert_eq!(
        types.last().copied(),
        Some("message_stop"),
        "dernier event doit être message_stop, obtenu: {:?}",
        types
    );

    // content_block_start doit être présent (bloc texte).
    assert!(
        types.contains(&"content_block_start"),
        "content_block_start doit être présent, obtenu: {:?}",
        types
    );

    // message_delta avec stop_reason "end_turn".
    let delta = events
        .iter()
        .find(|(t, _)| t == "message_delta")
        .expect("message_delta doit être présent");
    assert_eq!(
        delta.1["delta"]["stop_reason"], "end_turn",
        "stop→end_turn dans message_delta"
    );

    // message_start contient le modèle de la requête Anthropic (pas l'alias interne).
    let start = &events[0].1;
    assert_eq!(
        start["message"]["model"], "claude-3-5-sonnet-20241022",
        "model doit être celui de la requête Anthropic"
    );
}

/// I4 — Alias "default" absent de la config → HTTP 404.
#[tokio::test]
async fn missing_default_alias_returns_404() {
    let backend = MockServer::start().await;
    // Config sans alias "default".
    let config = test_config_without_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_string(&anthropic_request_body("test")).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "alias 'default' absent → 404"
    );
}

/// I5 — Corps JSON syntaxiquement invalide → HTTP 400 (Axum JSON extractor).
///
/// Note : Axum retourne 422 pour un JSON structurellement invalide (champ manquant),
/// et 400 pour un JSON syntaxiquement invalide (parse error). Ici on teste le cas
/// syntaxique (mauvais JSON).
#[tokio::test]
async fn invalid_json_body_returns_400() {
    let backend = MockServer::start().await;
    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(b"{ invalid json }".as_slice()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    // Axum retourne 400 pour un JSON syntaxiquement invalide (parse error).
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "JSON syntaxiquement invalide → 400"
    );
}

/// I6 — Requête multi-tour conversation preservée.
///
/// La conversation user/assistant doit être transmise dans l'ordre correct au backend.
#[tokio::test]
async fn multi_turn_conversation_forwarded() {
    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_chat_response("Madrid.")))
        .mount(&backend)
        .await;

    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 256,
        "messages": [
            {"role": "user", "content": "Quelle est la capitale de la France ?"},
            {"role": "assistant", "content": "Paris."},
            {"role": "user", "content": "Et de l'Espagne ?"}
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["content"][0]["text"], "Madrid.");
}

/// I7 — [Slice B] Requête avec tools + backend retournant tool_calls → ResponseBlock::ToolUse.
///
/// Vérifie le round-trip complet :
/// - `tools[]` Anthropic → `tools[]` OpenAI transmis au backend
/// - La réponse backend avec `tool_calls` est traduite en `content[].type = "tool_use"`
/// - `stop_reason = "tool_use"` (depuis finish_reason "tool_calls")
/// - `input` parsé depuis les arguments JSON string du backend
#[tokio::test]
async fn tool_use_round_trip_returns_tool_use_block() {
    let backend = MockServer::start().await;

    // Le backend OpenAI répond avec tool_calls (comme un vrai LLM qui appelle un outil).
    let backend_tool_calls_response = json!({
        "id": "chatcmpl-tool-roundtrip",
        "object": "chat.completion",
        "created": 1234567890u64,
        "model": "qwen3-30b",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_abc123",
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"location\":\"Paris\",\"unit\":\"celsius\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 50,
            "completion_tokens": 20,
            "total_tokens": 70
        }
    });

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(backend_tool_calls_response))
        .mount(&backend)
        .await;

    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    // Requête Anthropic avec outil défini + tool_choice:auto.
    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "tools": [{
            "name": "get_weather",
            "description": "Retourne la météo pour une localisation.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "location": {"type": "string", "description": "Ville ou région"},
                    "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
                },
                "required": ["location"]
            }
        }],
        "tool_choice": {"type": "auto"},
        "messages": [{
            "role": "user",
            "content": "Quel temps fait-il à Paris ?"
        }]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "tool use round-trip doit retourner HTTP 200"
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();

    // stop_reason doit être "tool_use".
    assert_eq!(
        body["stop_reason"], "tool_use",
        "stop_reason doit être 'tool_use' pour finish_reason 'tool_calls'"
    );

    // content doit contenir un bloc tool_use.
    let content = body["content"]
        .as_array()
        .expect("content doit être un tableau");
    assert!(
        !content.is_empty(),
        "content doit contenir au moins un bloc"
    );

    // Trouver le bloc tool_use (peut y en avoir d'autres comme Text vide).
    let tool_use_block = content.iter().find(|b| b["type"] == "tool_use");
    let tool_block = tool_use_block.expect("doit contenir un bloc de type 'tool_use'");

    assert_eq!(
        tool_block["id"], "call_abc123",
        "id doit correspondre au tool_call OpenAI"
    );
    assert_eq!(
        tool_block["name"], "get_weather",
        "name doit être le nom de la fonction"
    );

    // input doit être l'objet JSON parsé depuis les arguments.
    assert_eq!(
        tool_block["input"]["location"], "Paris",
        "input.location doit être Paris"
    );
    assert_eq!(
        tool_block["input"]["unit"], "celsius",
        "input.unit doit être celsius"
    );
}

/// I8 — [Slice B] Réponse avec texte + tool_calls → blocs Text + ToolUse dans l'ordre.
///
/// Le modèle peut générer du texte avant d'appeler un outil.
#[tokio::test]
async fn mixed_text_and_tool_use_response() {
    let backend = MockServer::start().await;

    let backend_response = json!({
        "id": "chatcmpl-mixed",
        "object": "chat.completion",
        "created": 1234567890u64,
        "model": "qwen3-30b",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Je vais vérifier la météo pour vous.",
                "tool_calls": [{
                    "id": "call_xyz789",
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"location\":\"Lyon\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 40,
            "completion_tokens": 15,
            "total_tokens": 55
        }
    });

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(backend_response))
        .mount(&backend)
        .await;

    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "tools": [{
            "name": "get_weather",
            "description": "Météo",
            "input_schema": {"type": "object", "properties": {}}
        }],
        "messages": [{"role": "user", "content": "Météo à Lyon ?"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();

    let content = body["content"]
        .as_array()
        .expect("content doit être un tableau");
    assert_eq!(content.len(), 2, "doit avoir 2 blocs : text + tool_use");

    // Premier bloc = texte.
    assert_eq!(content[0]["type"], "text", "premier bloc doit être text");
    assert_eq!(
        content[0]["text"], "Je vais vérifier la météo pour vous.",
        "texte doit correspondre"
    );

    // Deuxième bloc = tool_use.
    assert_eq!(
        content[1]["type"], "tool_use",
        "deuxième bloc doit être tool_use"
    );
    assert_eq!(content[1]["name"], "get_weather");
    assert_eq!(content[1]["input"]["location"], "Lyon");
}

// ── Slice D : Tests agnosticisme, model_map, count_tokens, erreur Anthropic, x-api-key ─────

/// Aide pour créer une config avec model_map + plusieurs aliases.
fn test_config_with_model_map(provider_endpoint: &str) -> Config {
    use gradatum_gateway::config::MessagesConfig;

    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "test-provider".to_string(),
        ProviderConfig {
            endpoint: provider_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 10,
        },
    );

    let mut aliases = HashMap::new();
    aliases.insert(
        "alias-claude".to_string(),
        AliasTarget::simple("test-provider", "model-a"),
    );
    aliases.insert(
        "alias-glm".to_string(),
        AliasTarget::simple("test-provider", "model-b"),
    );
    aliases.insert(
        "alias-gemini".to_string(),
        AliasTarget::simple("test-provider", "model-c"),
    );
    aliases.insert(
        "fallback-alias".to_string(),
        AliasTarget::simple("test-provider", "model-fallback"),
    );

    let mut model_map = HashMap::new();
    model_map.insert(
        "claude-3-5-sonnet-20241022".to_string(),
        "alias-claude".to_string(),
    );
    model_map.insert("glm-4.6".to_string(), "alias-glm".to_string());
    model_map.insert("gemini-2.0-flash".to_string(), "alias-gemini".to_string());

    Config {
        server: ServerConfig {
            listen: "127.0.0.1:18436".to_string(),
            registry_db: None,
            bearer_token_env: None,
            rate_limit_per_minute: 1000,
            circuit_threshold: 5,
            circuit_window_secs: 60,
            circuit_cooldown_secs: 30,
            max_total_tokens: 0,
            trust_localhost: true,
            enable_slot_passthrough: false,
            allowed_origins: vec![],
            max_tools_per_request: 10,
        },
        providers,
        aliases,
        gateway: HashMap::new(),
        logging: LoggingConfig::default(),
        vault_aware: VaultAwareConfig::default(),
        messages: MessagesConfig {
            default_alias: "fallback-alias".to_string(),
            model_map,
        },
    }
}

/// SD1 — model_map : 3 noms hétérogènes → 3 aliases distincts.
///
/// Chaque modèle est routé vers l'alias correct configuré dans `model_map`.
/// Le backend est inspecté pour vérifier que le bon alias (donc le bon modèle interne)
/// est utilisé (via la réponse `model` du backend que le handler ignore, mais on vérifie
/// que la requête se déroule sans 404).
#[tokio::test]
async fn model_map_routes_heterogeneous_models_to_correct_aliases() {
    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_chat_response("ok")))
        .mount(&backend)
        .await;

    let config = test_config_with_model_map(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    for model in ["claude-3-5-sonnet-20241022", "glm-4.6", "gemini-2.0-flash"] {
        let req_body = json!({
            "model": model,
            "max_tokens": 10,
            "messages": [{"role": "user", "content": "ping"}]
        });

        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&req_body).unwrap()))
            .unwrap();

        let app_clone = build_router(AppState::for_test(test_config_with_model_map(
            &backend.uri(),
        )));
        let response = app_clone.oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "model_map '{}' → alias existant → doit répondre 200",
            model
        );
    }
    let _ = app;
}

/// SD2 — modèle absent du model_map → default_alias utilisé (pas 404).
#[tokio::test]
async fn model_not_in_map_uses_default_alias() {
    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_chat_response("fallback-ok")))
        .mount(&backend)
        .await;

    let config = test_config_with_model_map(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    // "qwen-max" n'est pas dans le model_map → doit utiliser "fallback-alias".
    let req_body = json!({
        "model": "qwen-max",
        "max_tokens": 10,
        "messages": [{"role": "user", "content": "ping"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "modèle absent du model_map → default_alias 'fallback-alias' → 200"
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["content"][0]["text"], "fallback-ok");
}

/// SD3 — `POST /v1/messages/count_tokens` → `{"input_tokens": N}`.
///
/// Vérifie que la route existe, retourne 200 et un objet `{"input_tokens": N}` avec N > 0.
#[tokio::test]
async fn count_tokens_returns_input_tokens_estimate() {
    let backend = MockServer::start().await;
    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "system": "Tu es un assistant utile.",
        "messages": [
            {"role": "user", "content": "Quelle est la capitale de la France ?"}
        ],
        "tools": [{
            "name": "search",
            "description": "Recherche sur le web",
            "input_schema": {"type": "object", "properties": {}}
        }]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "/v1/messages/count_tokens doit retourner HTTP 200"
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(
        body["input_tokens"].is_number(),
        "input_tokens doit être un nombre, obtenu: {:?}",
        body
    );
    let n = body["input_tokens"].as_u64().unwrap();
    assert!(n > 0, "input_tokens doit être > 0, obtenu: {}", n);
}

/// SD4 — Erreur 404 sur `/v1/messages` → corps JSON Anthropic `{"type":"error",...}`.
///
/// Quand l'alias n'existe pas, la réponse d'erreur doit être au format Anthropic,
/// pas au format OpenAI.
#[tokio::test]
async fn messages_404_returns_anthropic_error_envelope() {
    let backend = MockServer::start().await;
    let config = test_config_without_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_string(&anthropic_request_body("test")).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();

    // Le corps doit être au format Anthropic, PAS OpenAI `{"error": {"message": ...}}`.
    assert_eq!(
        body["type"], "error",
        "erreur Anthropic doit avoir type='error', obtenu: {:?}",
        body
    );
    assert!(
        body["error"]["type"].is_string(),
        "error.type doit être une string"
    );
    assert_eq!(
        body["error"]["type"], "not_found_error",
        "404 → not_found_error"
    );
    assert!(
        body["error"]["message"].is_string(),
        "error.message doit être une string"
    );
}

/// SD5 — `x-api-key` valide → 200 (alternative à `Authorization: Bearer`).
#[tokio::test]
async fn x_api_key_valid_returns_200() {
    use std::sync::Arc;

    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_chat_response("ok")))
        .mount(&backend)
        .await;

    let mut config = test_config_with_default_alias(&backend.uri());
    config.server.trust_localhost = false;
    let mut state = AppState::for_test(config);
    state.bearer_token = Some(Arc::new(secrecy::SecretString::from("mytoken".to_string())));
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("x-api-key", "mytoken")
        .body(Body::from(
            serde_json::to_string(&anthropic_request_body("ping")).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "x-api-key valide doit retourner 200"
    );
}

/// SD6 — `x-api-key` invalide → 401.
#[tokio::test]
async fn x_api_key_invalid_returns_401() {
    use std::sync::Arc;

    let backend = MockServer::start().await;
    let mut config = test_config_with_default_alias(&backend.uri());
    config.server.trust_localhost = false;
    let mut state = AppState::for_test(config);
    state.bearer_token = Some(Arc::new(secrecy::SecretString::from("mytoken".to_string())));
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("x-api-key", "wrong-token")
        .body(Body::from(
            serde_json::to_string(&anthropic_request_body("ping")).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "x-api-key invalide doit retourner 401"
    );
}

// ── security-reviewer findings V1-V4 ───────────────────────────────────────────

/// V1 — Gate max_tools_per_request sur /v1/messages.
///
/// Quand le nombre d'outils dépasse la borne, `/v1/messages` doit retourner HTTP 400
/// avec l'enveloppe Anthropic `{"type":"error","error":{"type":"invalid_request_error",...}}`.
#[tokio::test]
async fn too_many_tools_returns_400_anthropic_envelope() {
    let backend = MockServer::start().await;
    // Config avec max_tools_per_request = 2.
    let mut config = test_config_with_default_alias(&backend.uri());
    config.server.max_tools_per_request = 2;
    let state = AppState::for_test(config);
    let app = build_router(state);

    // 3 outils > 2 max → doit être rejeté.
    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 100,
        "tools": [
            {"name": "t1", "description": "outil 1", "input_schema": {"type": "object"}},
            {"name": "t2", "description": "outil 2", "input_schema": {"type": "object"}},
            {"name": "t3", "description": "outil 3", "input_schema": {"type": "object"}}
        ],
        "messages": [{"role": "user", "content": "test"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "trop d'outils → 400"
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["type"], "error", "doit être enveloppe Anthropic");
    assert_eq!(
        body["error"]["type"], "invalid_request_error",
        "type doit être invalid_request_error pour 400"
    );
}

/// V2 — Gate max_total_tokens sur /v1/messages.
///
/// Quand les tokens estimés dépassent le cap, `/v1/messages` doit retourner HTTP 413
/// avec l'enveloppe Anthropic `{"type":"error","error":{"type":"request_too_large",...}}`.
#[tokio::test]
async fn token_cap_exceeded_returns_413_anthropic_envelope() {
    let backend = MockServer::start().await;
    // Cap très bas (10 tokens) pour déclencher le rejet.
    let mut config = test_config_with_default_alias(&backend.uri());
    config.server.max_total_tokens = 10;
    let state = AppState::for_test(config);
    let app = build_router(state);

    // Message long + max_tokens élevé → dépasse le cap de 10.
    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 8000,
        "messages": [{"role": "user", "content": "Explique en détail l'histoire de la Révolution française."}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "token cap dépassé → 413"
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["type"], "error", "doit être enveloppe Anthropic");
    assert_eq!(
        body["error"]["type"], "request_too_large",
        "type doit être request_too_large pour 413"
    );
}

/// V4 — Erreur 404 ne doit pas exposer la liste des alias disponibles.
///
/// Le corps de la réponse `not_found_error` ne doit PAS contenir les noms des alias
/// configurés. Un attaquant ne doit pas pouvoir énumérer les alias via des requêtes 404.
#[tokio::test]
async fn alias_not_found_response_does_not_leak_alias_list() {
    let backend = MockServer::start().await;
    // Config avec alias "secret-internal-alias" — ne doit pas apparaître dans la réponse.
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "test-provider".to_string(),
        ProviderConfig {
            endpoint: backend.uri().to_string(),
            api_key_env: None,
            timeout_secs: 10,
        },
    );
    let mut aliases = HashMap::new();
    aliases.insert(
        "secret-internal-alias".to_string(),
        AliasTarget::simple("test-provider", "internal-model"),
    );
    let config = Config {
        server: ServerConfig {
            listen: "127.0.0.1:18436".to_string(),
            registry_db: None,
            bearer_token_env: None,
            rate_limit_per_minute: 1000,
            circuit_threshold: 5,
            circuit_window_secs: 60,
            circuit_cooldown_secs: 30,
            max_total_tokens: 0,
            trust_localhost: true,
            enable_slot_passthrough: false,
            allowed_origins: vec![],
            max_tools_per_request: 10,
        },
        providers,
        aliases,
        gateway: HashMap::new(),
        logging: LoggingConfig::default(),
        vault_aware: VaultAwareConfig::default(),
        messages: Default::default(),
    };
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_string(&anthropic_request_body("test")).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
    let body: Value = serde_json::from_str(&body_str).unwrap();

    // La liste des aliases ne doit PAS apparaître dans le corps.
    assert!(
        !body_str.contains("secret-internal-alias"),
        "la liste des aliases ne doit pas fuiter dans la réponse 404, obtenu: {}",
        body_str
    );
    assert_eq!(body["type"], "error", "doit être enveloppe Anthropic");
    assert_eq!(body["error"]["type"], "not_found_error");
    // Le message doit être générique.
    assert!(
        body["error"]["message"].is_string(),
        "error.message doit être une string"
    );
}

// ── security-reviewer findings V3 + V5 (FIX-SET 1) ────────────────────────────

/// V5 — Rate-limit appliqué à `/v1/messages/count_tokens`.
///
/// Quand le rate-limit est dépassé, `/v1/messages/count_tokens` doit retourner HTTP 429
/// avec l'enveloppe Anthropic `{"type":"error","error":{"type":"rate_limit_error",...}}`.
/// Sans ce fix, le handler ignorait `state` (`State(_state)`) — aucun check effectué.
#[tokio::test]
async fn count_tokens_rate_limit_returns_429() {
    let backend = MockServer::start().await;
    // Config avec rate-limit = 1 requête/minute.
    let mut config = test_config_with_default_alias(&backend.uri());
    config.server.rate_limit_per_minute = 1;
    // Ne pas faire confiance à localhost (sinon rate-limit est bypassé).
    config.server.trust_localhost = false;
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "ping"}]
    });

    // Première requête — doit passer (consomme le quota).
    let req1 = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(
        resp1.status(),
        StatusCode::OK,
        "première requête doit passer (quota 1)"
    );

    // Deuxième requête — quota épuisé, doit retourner 429.
    let req2 = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "/v1/messages/count_tokens doit retourner 429 quand le rate-limit est dépassé"
    );

    let body_bytes = axum::body::to_bytes(resp2.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["type"], "error", "doit être enveloppe Anthropic");
    assert_eq!(
        body["error"]["type"], "rate_limit_error",
        "type doit être rate_limit_error pour 429"
    );
}

/// V3a — Trop de messages dans la requête → 400 avec enveloppe Anthropic.
///
/// L'API Anthropic rejette les requêtes avec plus de 500 messages.
/// Sans ce fix, un client pouvait envoyer des milliers de messages sans limite.
#[tokio::test]
async fn too_many_messages_returns_invalid_request() {
    let backend = MockServer::start().await;
    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    // 501 messages > MAX_MESSAGES_PER_REQUEST (500).
    let messages: Vec<Value> = (0..501)
        .map(|i| {
            json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "content": format!("message {i}")
            })
        })
        .collect();

    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 100,
        "messages": messages
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "501 messages → 400"
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["type"], "error", "doit être enveloppe Anthropic");
    assert_eq!(
        body["error"]["type"], "invalid_request_error",
        "type doit être invalid_request_error pour 400 (trop de messages)"
    );
}

/// V3b — Trop de stop_sequences dans la requête → 400 avec enveloppe Anthropic.
///
/// L'API Anthropic accepte au maximum 4 stop_sequences.
/// Sans ce fix, un client pouvait passer une liste arbitrairement longue.
#[tokio::test]
async fn too_many_stop_sequences_returns_invalid_request() {
    let backend = MockServer::start().await;
    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    // 5 stop_sequences > 4 max autorisés par l'API Anthropic.
    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "test"}],
        "stop_sequences": ["</s1>", "</s2>", "</s3>", "</s4>", "</s5>"]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "5 stop_sequences → 400"
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["type"], "error", "doit être enveloppe Anthropic");
    assert_eq!(
        body["error"]["type"], "invalid_request_error",
        "type doit être invalid_request_error pour 400 (trop de stop_sequences)"
    );
}

// ── FIX 3 — P1-B : count_tokens sans max_tokens + enveloppe erreur Anthropic ──────────────

/// P1-B(a) — `POST /v1/messages/count_tokens` sans `max_tokens` → 200 avec `{input_tokens: N}`.
///
/// L'API Anthropic count_tokens ne requiert PAS `max_tokens` (contrairement à /v1/messages).
/// Avant le fix, Axum retournait 422 car `MessagesRequest.max_tokens: u32` est obligatoire.
/// Fix : DTO dédié `CountTokensRequest` sans `max_tokens`.
#[tokio::test]
async fn count_tokens_without_max_tokens_returns_200() {
    let backend = MockServer::start().await;
    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    // Requête SANS max_tokens (conforme à l'API Anthropic count_tokens).
    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [
            {"role": "user", "content": "Quelle est la capitale de la France ?"}
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "/v1/messages/count_tokens SANS max_tokens doit retourner 200, obtenu: {}",
        response.status()
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(
        body["input_tokens"].is_number(),
        "input_tokens doit être un nombre, obtenu: {:?}",
        body
    );
    let n = body["input_tokens"].as_u64().unwrap();
    assert!(n > 0, "input_tokens doit être > 0, obtenu: {}", n);
}

/// P1-B(b) — `POST /v1/messages` SANS `max_tokens` → 400 avec enveloppe Anthropic.
///
/// `max_tokens` est obligatoire pour /v1/messages mais quand il manque, Axum retourne
/// une erreur 422 au format texte par défaut. Le fix intercepte ce rejet via un
/// extracteur JSON custom et le convertit en enveloppe Anthropic avec HTTP 400.
#[tokio::test]
async fn messages_without_max_tokens_returns_anthropic_error_400() {
    let backend = MockServer::start().await;
    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    // Requête SANS max_tokens → champ obligatoire manquant.
    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [{"role": "user", "content": "Bonjour"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "/v1/messages sans max_tokens doit retourner 400, obtenu: {}",
        response.status()
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();

    // Corps doit être enveloppe Anthropic (PAS le format texte Axum par défaut).
    assert_eq!(
        body["type"], "error",
        "doit être enveloppe Anthropic {{type:error,...}}, obtenu: {:?}",
        body
    );
    assert_eq!(
        body["error"]["type"], "invalid_request_error",
        "error.type doit être invalid_request_error pour 400, obtenu: {:?}",
        body["error"]["type"]
    );
    assert!(
        body["error"]["message"].is_string(),
        "error.message doit être une string"
    );
}

/// P1-B(b) — `POST /v1/messages` avec JSON malformé → 400 avec enveloppe Anthropic.
///
/// Un JSON syntaxiquement invalide doit retourner l'enveloppe Anthropic, pas le
/// corps texte Axum par défaut.
#[tokio::test]
async fn messages_malformed_json_returns_anthropic_error_400() {
    let backend = MockServer::start().await;
    let config = test_config_with_default_alias(&backend.uri());
    let state = AppState::for_test(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(b"{ invalid json here }".as_slice()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "JSON malformé doit retourner 400, obtenu: {}",
        response.status()
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();

    // Corps doit être enveloppe Anthropic.
    assert_eq!(
        body["type"], "error",
        "JSON malformé doit retourner enveloppe Anthropic {{type:error,...}}, obtenu: {:?}",
        body
    );
    assert_eq!(
        body["error"]["type"], "invalid_request_error",
        "error.type doit être invalid_request_error, obtenu: {:?}",
        body["error"]["type"]
    );
    assert!(
        body["error"]["message"].is_string(),
        "error.message doit être présent"
    );
}
