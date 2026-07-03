//! Handler for `POST /v1/messages` and `POST /v1/messages/count_tokens`
//! (Anthropic Messages API inbound).
//!
//! Translates the Anthropic request format → internal `ChatCompletionRequest` → dispatch
//! via the same dispatch path as `handlers::chat`, then re-translates the response
//! into Anthropic format.
//!
//! # Capabilities
//! - Non-streaming text responses.
//! - Full tool use (tools[], tool_choice, tool_use, tool_result, image blocks).
//! - Anthropic SSE streaming (`stream:true`).
//! - Configurable model map, token counting, Anthropic error envelope.
//!
//! # Model routing
//! The alias is resolved via `config.messages.model_map.get(&model)`, falling back to
//! `config.messages.default_alias`. No model name or family is hardcoded in the handler logic.
//!
//! # Authentication
//! The `x-api-key` header (Anthropic convention) is accepted in addition to `Authorization: Bearer`
//! (via the `auth::bearer_auth` middleware).
//!
//! # Errors
//! Errors returned by `/v1/messages` and `/v1/messages/count_tokens`
//! use the Anthropic error envelope `{"type":"error","error":{"type":...,"message":...}}`.
//! Existing OpenAI routes retain their OpenAI error format.

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use axum::{
    Json,
    body::Body,
    extract::{ConnectInfo, Extension, FromRequest, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::future::BoxFuture;
use tracing::instrument;

use crate::{
    AppState,
    anthropic::{
        stream::{StreamDispatch, keepalive_anthropic_sse},
        translate,
    },
    commons::anthropic::{CountTokensRequest, MessagesRequest},
    error::ApiError,
    handlers::chat::{dispatch_stream_with_fallback, dispatch_with_fallback},
    rate_limit::extract_client_ip_from_socket,
    registry::RequestLogEntry,
    vault_aware::{CostAttribution, make_qa_event},
};

/// Période d'émission des `ping` SSE pendant l'attente du premier token de l'engine.
///
/// Maintient la connexion active côté client (Claude Code idle-timeout ~113s) pendant le
/// prefill d'un gros contexte (~90-110s sur Qwen3-VL-30B). 5s laisse une marge confortable
/// vis-à-vis du timeout client et d'éventuels proxies intermédiaires.
const SSE_KEEPALIVE_PERIOD: Duration = Duration::from_secs(5);

// ── Enveloppe erreur Anthropic ────────────────────────────────────────────────

/// Corps d'erreur au format Anthropic Messages API.
///
/// Distinct du format OpenAI `{"error": {...}}` utilisé par les autres routes.
/// Limité aux routes `/v1/messages*`.
#[derive(serde::Serialize)]
struct AnthropicErrorBody {
    #[serde(rename = "type")]
    body_type: &'static str,
    error: AnthropicErrorDetail,
}

#[derive(serde::Serialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
}

/// Mappe un code HTTP vers le `error.type` Anthropic correspondant.
///
/// Partagé entre la réponse d'erreur HTTP ([`anthropic_error_response`]) et l'event SSE
/// `error` émis en cours de stream (branche stream de [`messages_handler_inner`]).
fn anthropic_error_type(status_code: u16) -> &'static str {
    match status_code {
        400 => "invalid_request_error",
        401 => "authentication_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

/// Construit une réponse HTTP avec l'enveloppe d'erreur Anthropic.
///
/// Mapping HTTP status → `error.type` Anthropic :
/// - 400 → `invalid_request_error`
/// - 401 → `authentication_error`
/// - 404 → `not_found_error`
/// - 413 → `request_too_large`
/// - 429 → `rate_limit_error`
/// - 500 → `api_error`
/// - 503/529 → `overloaded_error`
///
/// # Sécurité (V4 — security-reviewer)
/// `ApiError::AliasNotFound` expose la liste complète des alias dans son `Display` —
/// ce serait une fuite d'informations de configuration interne vers le client.
/// La liste n'est PAS transmise au client : seul un message générique est retourné.
/// Le détail complet reste accessible côté serveur via les traces de l'appelant.
fn anthropic_error_response(api_err: ApiError) -> Response {
    let status_code = api_err.status_code();
    // V4 (security-reviewer P1) : ne pas exposer la liste des alias au client.
    // `AliasNotFound::to_string()` inclut `available.join(", ")` — retourner un message
    // générique côté client. Le détail est loggué par l'appelant.
    let message = match &api_err {
        ApiError::AliasNotFound { .. } => "model not found".to_string(),
        other => other.to_string(),
    };
    let error_type = anthropic_error_type(status_code);
    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = AnthropicErrorBody {
        body_type: "error",
        error: AnthropicErrorDetail {
            error_type,
            message,
        },
    };
    (status, Json(body)).into_response()
}

// ── Extracteur JSON Anthropic custom ─────────────────────────────────────────

/// Extracteur JSON custom pour les routes `/v1/messages`.
///
/// Intercepte les erreurs de désérialisation JSON (body invalide, champ manquant)
/// qu'Axum transformerait normalement en réponse 422 au format texte, et les
/// convertit en enveloppe d'erreur Anthropic HTTP 400.
///
/// # Implémentation
/// Utilise `Json<T>` d'Axum en interne pour la désérialisation.
/// En cas d'échec, construit une réponse Anthropic `invalid_request_error`.
///
/// # Sécurité (ADN 5)
/// Le message d'erreur de serde (détail technique) est transmis tel quel car
/// il ne contient aucune information de configuration interne — c'est un diagnostic
/// de parsing du corps de la requête utilisateur.
/// Extracteur JSON Anthropic — utilisé dans les signatures des handlers `pub`.
/// `pub` requis pour satisfaire la règle de visibilité Rust (handler pub + extracteur).
pub struct AnthropicJson<T>(T);

impl<T, S> FromRequest<S> for AnthropicJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    // En cas d'erreur, on retourne une Response directement (enveloppe Anthropic).
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AnthropicJson(value)),
            Err(rejection) => {
                // Construire l'enveloppe Anthropic pour l'erreur de désérialisation.
                let message = rejection.body_text();
                let body = AnthropicErrorBody {
                    body_type: "error",
                    error: AnthropicErrorDetail {
                        error_type: "invalid_request_error",
                        message,
                    },
                };
                Err((StatusCode::BAD_REQUEST, Json(body)).into_response())
            }
        }
    }
}

/// Handler pour `POST /v1/messages`.
///
/// Deux branches :
/// - `stream:true`  → SSE Anthropic via `dispatch_stream_with_fallback` + `chunks_to_anthropic_sse`.
/// - `stream:false` → JSON Anthropic via `dispatch_with_fallback` + `chat_to_anthropic`.
///
/// Le routage modèle est 100% configurable via `[messages] model_map` + `default_alias`
/// dans le TOML — aucun nom de modèle n'est codé en dur ici.
///
/// Les erreurs sont retournées au format Anthropic `{"type":"error","error":{...}}`
/// (distinct du format OpenAI des autres routes).
///
/// # Errors
/// - `ApiError::InvalidBody` : corps JSON invalide ou désérialisation impossible.
///   → 400 enveloppe Anthropic via `AnthropicJson` extracteur.
/// - Toutes les erreurs de dispatch (Backend, AliasNotFound, etc.).
#[instrument(skip(state, connect_info, _headers, body), fields(model))]
pub async fn handler(
    State(state): State<AppState>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    _headers: axum::http::HeaderMap,
    AnthropicJson(body): AnthropicJson<MessagesRequest>,
) -> Response {
    match messages_handler_inner(state, connect_info, body).await {
        Ok(r) => r,
        Err(e) => anthropic_error_response(e),
    }
}

/// Logique interne du handler `/v1/messages` — retourne `ApiError` pour conversion
/// vers l'enveloppe Anthropic par `handler`.
async fn messages_handler_inner(
    state: AppState,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: MessagesRequest,
) -> Result<Response, ApiError> {
    let client_ip = extract_client_ip_from_socket(&connect_info);

    // Rate limiting — même gate que /v1/chat/completions.
    if !state.rate_limiter.check_and_increment(client_ip) {
        return Ok(axum::http::Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("Retry-After", "60")
            .header(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            )
            .body(axum::body::Body::from(
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limit exceeded"}}"#,
            ))
            .unwrap_or_else(|_| StatusCode::TOO_MANY_REQUESTS.into_response()));
    }

    tracing::Span::current().record("model", body.model.as_str());

    // V1 (security-reviewer P0) — Gate max_tools_per_request.
    let max_tools = state.config.server.max_tools_per_request;
    if max_tools > 0
        && let Some(tools) = &body.tools
        && tools.len() > max_tools
    {
        tracing::warn!(
            count = tools.len(),
            max = max_tools,
            "trop d'outils dans la requête /v1/messages — rejetée HTTP 400"
        );
        return Err(ApiError::TooManyTools {
            count: tools.len(),
            max: max_tools,
        });
    }

    // V2 (security-reviewer P0) — Gate max_total_tokens.
    // Estimation heuristique sur MessagesRequest (chars/4, même logique que count_tokens).
    let cap = state.config.server.max_total_tokens;
    if cap > 0 {
        let total = estimate_tokens_from_messages_request(&body);
        if total > cap {
            tracing::warn!(
                total_tokens = total,
                cap = cap,
                model = %body.model,
                "cap tokens dépassé sur /v1/messages — requête rejetée HTTP 413"
            );
            return Err(ApiError::ContextLengthExceeded { total, cap });
        }
    }

    // V3 (security-reviewer P2) — Bornes sur Vec non bornés.
    // L'API Anthropic impose une limite implicite sur ces dimensions.

    /// Nombre maximal de messages par requête (aligné sur les limites Anthropic observées).
    const MAX_MESSAGES_PER_REQUEST: usize = 500;

    if body.messages.len() > MAX_MESSAGES_PER_REQUEST {
        tracing::warn!(
            count = body.messages.len(),
            max = MAX_MESSAGES_PER_REQUEST,
            "trop de messages dans la requête /v1/messages — rejetée HTTP 400"
        );
        return Err(ApiError::InvalidBody(format!(
            "messages array exceeds limit: {} > {} (maximum allowed)",
            body.messages.len(),
            MAX_MESSAGES_PER_REQUEST,
        )));
    }

    if let Some(stop_seqs) = &body.stop_sequences {
        // L'API Anthropic n'accepte que 4 stop_sequences maximum.
        const MAX_STOP_SEQUENCES: usize = 4;
        if stop_seqs.len() > MAX_STOP_SEQUENCES {
            tracing::warn!(
                count = stop_seqs.len(),
                max = MAX_STOP_SEQUENCES,
                "trop de stop_sequences dans la requête /v1/messages — rejetée HTTP 400"
            );
            return Err(ApiError::InvalidBody(format!(
                "stop_sequences array exceeds limit: {} > {} (maximum allowed)",
                stop_seqs.len(),
                MAX_STOP_SEQUENCES,
            )));
        }
    }

    let is_stream = body.stream == Some(true);

    // Résolution de l'alias via model_map configurable (Slice D — 100% agnostique).
    // Aucun nom de modèle/famille n'est codé en dur ici.
    let resolved_alias_name = state
        .config
        .messages
        .model_map
        .get(&body.model)
        .cloned()
        .unwrap_or_else(|| state.config.messages.default_alias.clone());

    // Vérification que l'alias résolu existe dans la config.
    let alias = state
        .config
        .aliases
        .get(&resolved_alias_name)
        .ok_or_else(|| {
            let mut available: Vec<String> = state.config.aliases.keys().cloned().collect();
            available.sort();
            tracing::warn!(
                model = %body.model,
                resolved = %resolved_alias_name,
                "alias Anthropic résolu en '{}' mais introuvable dans la config",
                resolved_alias_name,
            );
            ApiError::AliasNotFound {
                alias: resolved_alias_name.clone(),
                available,
            }
        })?
        .clone();

    // Traduction Anthropic → interne (Slice B : texte + tools + images tous supportés).
    let model_name = body.model.clone();

    // Détection d'images AVANT de consommer body (pour passer has_image au dispatch).
    let has_image = body.messages.iter().any(|m| {
        use crate::commons::anthropic::AnthropicContent;
        use crate::commons::anthropic::ContentBlock;
        if let AnthropicContent::Blocks(blocks) = &m.content {
            blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
        } else {
            false
        }
    });

    let mut chat_req = translate::anthropic_to_chat(&body, &resolved_alias_name)
        .map_err(|e| ApiError::InvalidBody(e.to_string()))?;

    if is_stream {
        // ── Branche Slice C : streaming SSE Anthropic, ouverture immédiate ────
        //
        // Le dispatch (et donc la décision de fallback primary→cpu-curator) est lancé DANS
        // le corps du stream : la `Response` 200 + `message_start` partent dès t=0, puis des
        // `ping` périodiques maintiennent la connexion pendant le prefill long de l'engine
        // (incident b9780 : idle-timeout client ~113s sinon). Le fallback reste intact — il
        // se décide sur l'échec connexion/headers, pas sur le premier token.
        chat_req.stream = Some(true);

        let message_id = format!("msg_{}", ulid::Ulid::new().to_string().to_lowercase());
        let sse_model = model_name.clone();

        // Données possédées par la tâche de dispatch différée (`'static`).
        let dispatch_state = state.clone();
        let dispatch_alias = alias.clone();
        let provider_name = alias.provider.clone();
        let metrics_model = model_name.clone();

        let dispatch: BoxFuture<'static, StreamDispatch> = Box::pin(async move {
            let start = Instant::now();
            let result = dispatch_stream_with_fallback(
                &dispatch_state,
                chat_req,
                &dispatch_alias,
                has_image,
            )
            .await;
            let latency = start.elapsed();

            // Décompose le résultat en (issue SSE, status, modèle effectif, message d'erreur).
            let (outcome, status_code, real_model, error_message) = match result {
                Ok((chunk_stream, model_used_effective)) => {
                    // FIX 1 : intercepter les erreurs mid-stream pour le circuit breaker.
                    //
                    // record_success a déjà été appelé dans dispatch_stream_with_fallback
                    // dès que la connexion stream est établie (sonde HalfOpen débloquée).
                    // Cette couche capture les ruptures TCP / crashes backend qui surviennent
                    // APRÈS l'établissement du stream, afin qu'elles soient comptabilisées
                    // et ne laissent pas le compteur de succès surestimé.
                    use futures::StreamExt as _;
                    let cb = dispatch_state.providers.circuit_breakers.clone();
                    let pid = provider_name.clone();
                    let wrapped: crate::commons::provider::ChatCompletionStream =
                        Box::pin(chunk_stream.inspect(move |r| {
                            if let Err(e) = r {
                                cb.record_failure(&pid, e);
                            }
                        }));
                    (
                        StreamDispatch::Ready(wrapped),
                        200u16,
                        model_used_effective,
                        None,
                    )
                }
                Err(e) => {
                    let status = e.status_code();
                    let detail = e.to_string();
                    // Détail loggué côté serveur uniquement (V3 information disclosure).
                    tracing::warn!(
                        error = %detail,
                        status,
                        "dispatch stream /v1/messages échoué — event error SSE (stream déjà ouvert)"
                    );
                    (
                        StreamDispatch::Failed {
                            error_type: anthropic_error_type(status),
                            message: "internal backend error".to_string(),
                        },
                        status,
                        metrics_model.clone(),
                        Some(detail),
                    )
                }
            };

            // Métriques + cost attribution (différées : dispatch exécuté dans le stream).
            dispatch_state.metrics.record_request(
                "/v1/messages",
                &metrics_model,
                &provider_name,
                status_code,
                Some(latency),
            );
            dispatch_state.vault_aware.send_event(make_qa_event(
                "/v1/messages",
                &metrics_model,
                &provider_name,
                status_code,
                latency.as_millis() as u64,
                CostAttribution {
                    feature_id: None,
                    model_used: Some(real_model.clone()),
                    usage: None, // inconnu en stream
                    agent_id: None,
                },
            ));

            // Log asynchrone registry.
            {
                let registry = dispatch_state.registry.clone();
                let model_alias = metrics_model.clone();
                let provider_real = provider_name.clone();
                tokio::spawn(async move {
                    let entry = RequestLogEntry {
                        model_alias,
                        provider_real,
                        real_model,
                        route: "/v1/messages".to_owned(),
                        latency_ms: Some(latency.as_millis() as u64),
                        status_code,
                        streamed: true,
                        error_message,
                    };
                    if let Err(e) = registry.log_request(entry).await {
                        tracing::warn!("erreur journalisation requête messages (stream): {}", e);
                    }
                });
            }

            outcome
        });

        let sse_body =
            keepalive_anthropic_sse(dispatch, sse_model, message_id, SSE_KEEPALIVE_PERIOD);

        let response = Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )
            .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))
            .header(header::CONNECTION, HeaderValue::from_static("keep-alive"))
            .body(Body::from_stream(sse_body))
            .map_err(|e| {
                ApiError::Backend(crate::commons::error::LlmError::Custom {
                    message: format!("erreur construction réponse SSE Anthropic: {}", e),
                })
            })?;

        Ok(response)
    } else {
        // ── Branche Slice A+B : non-stream JSON Anthropic ────────────────────
        chat_req.stream = Some(false);
        let start = Instant::now();

        let (result, effective_provider, usage, model_used_effective) = dispatch_with_fallback(
            &state, chat_req, &alias, false, // is_stream
            None,  // slot_id — non applicable ici
            has_image,
        )
        .await;

        let latency = start.elapsed();

        // Enregistrement circuit breaker.
        match &result {
            Ok(_) => state
                .providers
                .circuit_breakers
                .record_success(&effective_provider),
            Err(ApiError::Backend(llm_err)) => {
                state
                    .providers
                    .circuit_breakers
                    .record_failure(&effective_provider, llm_err);
            }
            Err(_) => {}
        }

        let status_code = match &result {
            Ok(resp) => resp.status().as_u16(),
            Err(e) => e.status_code(),
        };

        // Métriques.
        state.metrics.record_request(
            "/v1/messages",
            &model_name,
            &effective_provider,
            status_code,
            Some(latency),
        );

        // VaultAware — même pattern que chat.rs.
        state.vault_aware.send_event(make_qa_event(
            "/v1/messages",
            &model_name,
            &effective_provider,
            status_code,
            latency.as_millis() as u64,
            CostAttribution {
                feature_id: None,
                model_used: Some(model_used_effective.clone()),
                usage: usage.as_ref(),
                agent_id: None,
            },
        ));

        // Log asynchrone registry.
        {
            let registry = state.registry.clone();
            let error_msg = result.as_ref().err().map(|e| e.to_string());
            let effective_provider_clone = effective_provider.clone();
            let model_used_clone = model_used_effective.clone();
            let model_name_clone = model_name.clone();
            tokio::spawn(async move {
                let entry = RequestLogEntry {
                    model_alias: model_name_clone,
                    provider_real: effective_provider_clone,
                    real_model: model_used_clone,
                    route: "/v1/messages".to_owned(),
                    latency_ms: Some(latency.as_millis() as u64),
                    status_code,
                    streamed: false,
                    error_message: error_msg,
                };
                if let Err(e) = registry.log_request(entry).await {
                    tracing::warn!("erreur journalisation requête messages: {}", e);
                }
            });
        }

        // La réponse du dispatch est une `Response` Axum contenant un JSON OpenAI.
        // On la désérialise pour la re-traduire en Anthropic (TODO Slice D : refactor).
        let dispatch_response = result?;

        let body_bytes = axum::body::to_bytes(dispatch_response.into_body(), 4 * 1024 * 1024)
            .await
            .map_err(|e| {
                ApiError::InvalidBody(format!("impossible de lire la réponse backend: {}", e))
            })?;

        let chat_resp: crate::commons::chat::ChatCompletionResponse =
            serde_json::from_slice(&body_bytes).map_err(|e| {
                ApiError::InvalidBody(format!(
                    "impossible de désérialiser la réponse backend: {}",
                    e
                ))
            })?;

        // Traduction interne → Anthropic.
        let anthropic_resp =
            translate::chat_to_anthropic(&chat_resp, &model_name).map_err(|e| {
                ApiError::InvalidBody(format!("erreur traduction réponse vers Anthropic: {}", e))
            })?;

        Ok((StatusCode::OK, Json(anthropic_resp)).into_response())
    }
}

// ── Estimation de tokens depuis MessagesRequest ───────────────────────────────

/// Estime le nombre total de tokens d'une `MessagesRequest` Anthropic.
///
/// Heuristique : chars/4 (même logique que `count_tokens_inner`).
/// Utilisée pour la gate `max_total_tokens` (V2 security-reviewer) AVANT dispatch.
///
/// Inclut `max_tokens` (output budgétaire) pour un total conservateur.
fn estimate_tokens_from_messages_request(body: &MessagesRequest) -> u64 {
    let mut char_count: usize = 0;

    if let Some(system) = &body.system {
        use crate::commons::anthropic::SystemContent;
        match system {
            SystemContent::Text(s) => char_count += s.len(),
            SystemContent::Blocks(blocks) => {
                for b in blocks {
                    use crate::commons::anthropic::ContentBlock;
                    if let ContentBlock::Text { text } = b {
                        char_count += text.len();
                    }
                }
            }
        }
    }

    for msg in &body.messages {
        use crate::commons::anthropic::{AnthropicContent, ContentBlock};
        match &msg.content {
            AnthropicContent::Text(s) => char_count += s.len(),
            AnthropicContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => char_count += text.len(),
                        ContentBlock::ToolResult { content, .. } => match content {
                            serde_json::Value::String(s) => char_count += s.len(),
                            serde_json::Value::Array(arr) => {
                                for item in arr {
                                    if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
                                        char_count += s.len();
                                    }
                                }
                            }
                            _ => {}
                        },
                        ContentBlock::Image { .. } => char_count += 256,
                        ContentBlock::ToolUse { name, .. } => char_count += name.len(),
                        ContentBlock::Thinking { .. } => {}
                        ContentBlock::Unknown => {}
                    }
                }
            }
        }
    }

    if let Some(tools) = &body.tools {
        for tool in tools {
            char_count += tool.name.len();
            if let Some(desc) = &tool.description {
                char_count += desc.len();
            }
            if let Ok(schema_str) = serde_json::to_string(&tool.input_schema) {
                char_count += schema_str.len();
            }
        }
    }

    let input_tokens = (char_count as u64).div_ceil(4);
    // Ajouter le budget de sortie (conservateur).
    input_tokens.saturating_add(body.max_tokens as u64)
}

// ── count_tokens ─────────────────────────────────────────────────────────────

/// Réponse de `POST /v1/messages/count_tokens`.
#[derive(serde::Serialize)]
struct CountTokensResponse {
    input_tokens: u32,
}

/// Handler pour `POST /v1/messages/count_tokens`.
///
/// Estime le nombre de tokens d'entrée sans appeler le backend.
///
/// # Heuristique (documentée, intentionnellement approchée)
/// L'estimation est basée sur la règle empirique « 4 caractères ≈ 1 token »
/// largement utilisée pour les modèles BPE (GPT-*, Claude-*).
/// Ce n'est pas un tokenizer réel — l'objectif est de fournir un budget indicatif
/// pour Claude Code, pas un compte exact.
///
/// Composantes additionnées :
/// 1. Longueur des textes de tous les messages (system inclus)
/// 2. Longueur des définitions d'outils (nom + description + schema JSON)
///
/// Le résultat est arrondi à l'entier supérieur (pas de plancher à 0 si texte vide).
///
/// # `max_tokens` absent (P1-B fix)
/// Contrairement à `/v1/messages`, `count_tokens` n'exige PAS `max_tokens`.
/// On utilise `CountTokensRequest` (DTO dédié sans `max_tokens`) pour accepter les
/// requêtes conformes à l'API Anthropic count_tokens.
///
/// # Sécurité (V5 — security-reviewer P1)
/// Applique le même gate de rate-limiting que `/v1/messages` — sans ça, un client
/// authentifié pouvait spammer indéfiniment cette route sans contrainte.
///
/// # Errors
/// - `ApiError::InvalidBody` : corps JSON invalide.
///
/// Les erreurs sont retournées au format Anthropic (enveloppe `{"type":"error",...}`).
pub async fn count_tokens_handler(
    State(state): State<AppState>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    AnthropicJson(body): AnthropicJson<CountTokensRequest>,
) -> Response {
    match count_tokens_inner(state, connect_info, body) {
        Ok(r) => r,
        Err(e) => anthropic_error_response(e),
    }
}

fn count_tokens_inner(
    state: AppState,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: CountTokensRequest,
) -> Result<Response, ApiError> {
    // V5 (security-reviewer P1) — Rate limiting identique à messages_handler_inner.
    let client_ip = extract_client_ip_from_socket(&connect_info);
    if !state.rate_limiter.check_and_increment(client_ip) {
        return Ok(axum::http::Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("Retry-After", "60")
            .header(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            )
            .body(axum::body::Body::from(
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limit exceeded"}}"#,
            ))
            .unwrap_or_else(|_| StatusCode::TOO_MANY_REQUESTS.into_response()));
    }
    // Accumulation des longueurs de texte.
    let mut char_count: usize = 0;

    // Champ system.
    if let Some(system) = &body.system {
        use crate::commons::anthropic::SystemContent;
        match system {
            SystemContent::Text(s) => char_count += s.len(),
            SystemContent::Blocks(blocks) => {
                for b in blocks {
                    use crate::commons::anthropic::ContentBlock;
                    if let ContentBlock::Text { text } = b {
                        char_count += text.len();
                    }
                }
            }
        }
    }

    // Messages.
    for msg in &body.messages {
        use crate::commons::anthropic::{AnthropicContent, ContentBlock};
        match &msg.content {
            AnthropicContent::Text(s) => char_count += s.len(),
            AnthropicContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => char_count += text.len(),
                        ContentBlock::ToolResult { content, .. } => {
                            // content est serde_json::Value (String ou tableau de blocs).
                            match content {
                                serde_json::Value::String(s) => char_count += s.len(),
                                serde_json::Value::Array(arr) => {
                                    // Tableau de blocs — extraire les textes.
                                    for item in arr {
                                        if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
                                            char_count += s.len();
                                        }
                                    }
                                }
                                // Null ou autre structure → ignorer.
                                _ => {}
                            }
                        }
                        // Images : approximation par 256 chars (heuristique conservatrice).
                        ContentBlock::Image { .. } => char_count += 256,
                        ContentBlock::ToolUse { name, .. } => char_count += name.len(),
                        // Thinking : ignoré (hors scope MVP).
                        ContentBlock::Thinking { .. } => {}
                        // Variante de sécurité absorbe les types futurs inconnus.
                        ContentBlock::Unknown => {}
                    }
                }
            }
        }
    }

    // Définitions d'outils.
    if let Some(tools) = &body.tools {
        for tool in tools {
            char_count += tool.name.len();
            if let Some(desc) = &tool.description {
                char_count += desc.len();
            }
            // Schema JSON sérialisé.
            if let Ok(schema_str) = serde_json::to_string(&tool.input_schema) {
                char_count += schema_str.len();
            }
        }
    }

    // 4 chars ≈ 1 token (arrondi supérieur, plancher = 1 si au moins 1 char).
    let input_tokens = u32::try_from(char_count.div_ceil(4)).unwrap_or(u32::MAX);
    // Garantir au moins 1 token si contenu non-vide, 0 si tout est vide.
    let input_tokens = if char_count > 0 && input_tokens == 0 {
        1
    } else {
        input_tokens
    };

    Ok((StatusCode::OK, Json(CountTokensResponse { input_tokens })).into_response())
}
