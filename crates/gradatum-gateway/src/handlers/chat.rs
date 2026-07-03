//! Handler for `POST /v1/chat/completions`.
//!
//! Dispatches requests using the alias table from config:
//! requested model → alias lookup → `provider_name` + `real_model` → HTTP forward.
//!
//! Features:
//! - `SmartRouter`: default parameters from the alias (`temperature_default`, `max_tokens_default`)
//!   and `AgentAware` overrides keyed by `feature_id` (header `X-Feature-Id`).
//! - `VaultAware` hook: fire-and-forget `QaEvent` to the gradatum event-log.
//! - `AgentAware` params: TOML sections `[gateway."<feature_id>"]`.
//!
//! Security:
//! - Rejects HTTP 400 when `tools.len() > max_tools_per_request`.
//! - Rate limiting based on `ConnectInfo<SocketAddr>` (real TCP socket IP).
//!
//! Modes:
//! - `stream: true`  → SSE passthrough response (`Content-Type: text/event-stream`).
//! - `stream: false` → JSON response (`Content-Type: application/json`).
//!
//! Error codes:
//! - 400: unknown alias, invalid body, too many tools.
//! - 413: token cap exceeded (`input + max_tokens > server.max_total_tokens`).
//! - 429: rate limit exceeded.
//! - 500: provider absent from config.
//! - 502: backend error — all fallbacks exhausted.
//! - 503: circuit breaker open — all fallbacks exhausted.

use std::{net::SocketAddr, sync::Arc, time::Instant};

use axum::{
    Json,
    body::Body,
    extract::{ConnectInfo, Extension, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use tracing::instrument;

use crate::{
    AppState,
    commons::{
        chat::{ChatCompletionRequest, Usage},
        circuit_breaker::CircuitBreakerRegistry,
        error::LlmError,
    },
    error::ApiError,
    rate_limit::extract_client_ip_from_socket,
    registry::RequestLogEntry,
    slot_passthrough::{extract_slot_id, inject_slot_id_if_needed},
    smart_router,
    token_counter::estimate_total_tokens,
    vault_aware::{CostAttribution, make_qa_event},
};

/// Handler for `POST /v1/chat/completions`.
///
/// Axum 0.8 (`axum-core` 0.5): uses `Option<Extension<ConnectInfo<SocketAddr>>>` instead of
/// `Option<ConnectInfo<SocketAddr>>` because `ConnectInfo<T>` does not implement
/// `OptionalFromRequestParts`. Extraction via `Extension<T>` (which does implement it) is equivalent.
#[instrument(skip(state, connect_info, headers, body), fields(model))]
pub async fn handler(
    State(state): State<AppState>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: axum::http::HeaderMap,
    Json(mut body): Json<ChatCompletionRequest>,
) -> Result<Response, ApiError> {
    // Rate limiting based on the real TCP socket IP (not XFF headers).
    let client_ip = extract_client_ip_from_socket(&connect_info);

    if !state.rate_limiter.check_and_increment(client_ip) {
        return Ok(Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("Retry-After", "60")
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .body(Body::from(
                r#"{"error":{"message":"rate limit exceeded","type":"rate_limit_error","code":"too_many_requests"}}"#,
            ))
            .unwrap_or_else(|_| StatusCode::TOO_MANY_REQUESTS.into_response()));
    }

    tracing::Span::current().record("model", body.model.as_str());

    // Hard cap on the number of tools per request.
    let max_tools = state.config.server.max_tools_per_request;
    if max_tools > 0
        && let Some(tools) = &body.tools
        && tools.len() > max_tools
    {
        tracing::warn!(
            count = tools.len(),
            max = max_tools,
            "trop d'outils dans la requête — rejetée HTTP 400"
        );
        return Err(ApiError::TooManyTools {
            count: tools.len(),
            max: max_tools,
        });
    }

    // Hard token cap.
    let cap = state.config.server.max_total_tokens;
    if cap > 0 {
        let total = estimate_total_tokens(&body);
        if total > cap {
            tracing::warn!(
                consumer = %client_ip,
                total_tokens = total,
                cap = cap,
                model = %body.model,
                "cap tokens dépassé — requête rejetée HTTP 413"
            );
            return Err(ApiError::ContextLengthExceeded { total, cap });
        }
    }

    // Strict alias resolution.
    let alias = state
        .config
        .aliases
        .get(&body.model)
        .ok_or_else(|| {
            let mut available: Vec<String> = state.config.aliases.keys().cloned().collect();
            available.sort();
            tracing::warn!(
                consumer = %client_ip,
                model = %body.model,
                "alias inconnu — requête rejetée HTTP 404"
            );
            ApiError::AliasNotFound {
                alias: body.model.clone(),
                available,
            }
        })?
        .clone();

    // Apply SmartRouter default parameters.
    // Clamp X-Feature-Id to 256 chars (unbounded external input is an abuse vector).
    let feature_id = headers
        .get("x-feature-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(256).collect::<String>());
    // Clamp X-Agent-Id to 256 chars as well (same abuse surface as `feature_id`).
    let agent_id = headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(256).collect::<String>());
    let agent_params = feature_id
        .as_deref()
        .and_then(|fid| state.config.gateway.get(fid));
    let routing = smart_router::apply(&mut body, &alias, agent_params);

    // Resolve the effective alias (potentially overridden by AgentAware).
    let effective_alias = if let Some(ref override_alias) = routing.alias_override {
        state
            .config
            .aliases
            .get(override_alias)
            .ok_or_else(|| ApiError::AliasNotFound {
                alias: override_alias.clone(),
                available: state.config.aliases.keys().cloned().collect(),
            })?
            .clone()
    } else {
        alias.clone()
    };

    // Upstream vision gate: reject image requests sent to a non-`vision_capable` alias.
    //
    // Checks the effective alias (post-SmartRouter override).
    // A complementary downstream gate in `dispatch_with_fallback` skips the fallback
    // with an explicit 503 when the primary vision provider is down — a text-only fallback
    // without an mmproj model must never receive a content-array image (silently wrong output).
    // Logging: only the count of image messages is logged, never the URL value (base64 ~1 MiB).
    let has_image_request = body.messages.iter().any(|m| m.has_image());
    if has_image_request && !effective_alias.vision_capable {
        let image_msg_count = body.messages.iter().filter(|m| m.has_image()).count();
        tracing::warn!(
            alias = %body.model,
            image_message_count = image_msg_count,
            "requête multimodale vers alias non vision_capable — rejetée HTTP 400"
        );
        return Err(ApiError::VisionNotSupported {
            alias: body.model.clone(),
        });
    }

    let slot_id = extract_slot_id(&headers);
    let model_alias = body.model.clone();
    let is_stream = body.stream == Some(true);
    let start = Instant::now();

    // `model_used_effective` is returned by `dispatch_with_fallback` to reflect
    // the provider that actually responded (primary or fallback). Do not capture
    // `effective_alias.model` before dispatch — that value always refers to the primary,
    // even when the fallback responded.
    let (result, effective_provider, usage, model_used_effective) = dispatch_with_fallback(
        &state,
        body,
        &effective_alias,
        is_stream,
        slot_id,
        has_image_request,
    )
    .await;

    let latency = start.elapsed();

    // Record circuit breaker outcome.
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

    // Synchronous metrics (atomic counters, no I/O).
    state.metrics.record_request(
        "/v1/chat/completions",
        &model_alias,
        &effective_provider,
        status_code,
        Some(latency),
    );

    // VaultAware hook: fire-and-forget `QaEvent` with cost attribution.
    // Uses `model_used_effective` (the model from the provider that actually responded),
    // not the primary alias model.
    state.vault_aware.send_event(make_qa_event(
        "/v1/chat/completions",
        &model_alias,
        &effective_provider,
        status_code,
        latency.as_millis() as u64,
        CostAttribution {
            feature_id,
            model_used: Some(model_used_effective.clone()),
            usage: usage.as_ref(),
            agent_id,
        },
    ));

    // Asynchronous SQLite request log — fire and forget.
    let registry = state.registry.clone();
    let alias_name = model_alias.clone();
    // `real_model` = effective model (the provider that responded), not always the primary.
    let real_model = model_used_effective;
    let error_msg = result.as_ref().err().map(|e| e.to_string());

    tokio::spawn(async move {
        let entry = RequestLogEntry {
            model_alias: alias_name,
            provider_real: effective_provider,
            real_model,
            route: "/v1/chat/completions".to_owned(),
            latency_ms: Some(latency.as_millis() as u64),
            status_code,
            streamed: is_stream,
            error_message: error_msg,
        };
        if let Err(e) = registry.log_request(entry).await {
            tracing::warn!("erreur journalisation requête chat: {}", e);
        }
    });

    result
}

/// Dispatches a chat request to the primary provider, with automatic fallback.
///
/// Returns `(result, effective_provider_name, usage_tokens, model_used_effective)`.
///
/// `usage_tokens` is `Some` only for non-streaming requests where the provider returned
/// a `Usage` object in its response. `None` for streaming, the slot-passthrough path
/// (opaque bytes), or when the backend does not expose usage.
///
/// `model_used_effective` is the real model of the provider that actually responded:
/// - primary OK → `alias.model`
/// - fallback OK → `fallback_model` (or `alias.model` if unset)
/// - double failure → last model attempted
///
/// This field ensures correct cost attribution in `QaEvent.model_used`.
/// Capturing `effective_alias.model` before dispatch always returned the primary model
/// even when the fallback had responded — a silent attribution error.
///
/// Vision gate: when `has_image` is `true` and the primary fails, the fallback is
/// deterministically skipped and an explicit 503 is returned.
/// A text-only fallback without an mmproj model would produce silently wrong output
/// on a content-array image. Deterministic skip is the only safe guarantee.
///
/// `pub(crate)` : partagé avec `handlers::messages` pour éviter la duplication
/// de la logique de dispatch (ADN 3 Factorisé).
pub(crate) async fn dispatch_with_fallback(
    state: &AppState,
    body: ChatCompletionRequest,
    alias: &crate::config::AliasTarget,
    is_stream: bool,
    slot_id: Option<u32>,
    has_image: bool,
) -> (Result<Response, ApiError>, String, Option<Usage>, String) {
    let primary_result = try_provider(
        state,
        body.clone(),
        &alias.provider,
        &alias.model,
        is_stream,
        slot_id,
    )
    .await;

    match primary_result {
        Ok((resp, usage)) => (Ok(resp), alias.provider.clone(), usage, alias.model.clone()),
        // Non-backend error (e.g. 400/413 validation, `ProviderNotFound`) — no fallback.
        Err(ref e) if !is_backend_error(e) => {
            let provider = alias.provider.clone();
            (
                Err(primary_result.unwrap_err()),
                provider,
                None,
                alias.model.clone(),
            )
        }
        Err(primary_err) => {
            if let ApiError::Backend(ref llm_err) = primary_err {
                state
                    .providers
                    .circuit_breakers
                    .record_failure(&alias.provider, llm_err);
            }

            // Downstream vision gate: deterministic fallback skip for image requests.
            //
            // The fallback of a `vision_capable` alias points to a non-vision provider
            // (e.g. cpu-curator without mmproj). Sending a content-array image to it would
            // produce silently wrong output. Return an explicit 503 instead.
            if has_image {
                tracing::warn!(
                    primary = %alias.provider,
                    error = %primary_err,
                    "primary vision provider échoué — fallback skipé (requête image)"
                );
                return (
                    Err(ApiError::ServiceUnavailable {
                        message: format!(
                            "vision provider '{}' unavailable, no vision-capable fallback",
                            alias.provider
                        ),
                    }),
                    alias.provider.clone(),
                    None,
                    alias.model.clone(),
                );
            }

            let Some(fb_provider) = &alias.fallback_provider else {
                return (
                    Err(primary_err),
                    alias.provider.clone(),
                    None,
                    alias.model.clone(),
                );
            };

            tracing::warn!(
                primary = %alias.provider,
                fallback = %fb_provider,
                error = %primary_err,
                "primary provider échoué — tentative fallback"
            );

            let fb_model = alias.fallback_model.as_deref().unwrap_or(&alias.model);
            let fb_result =
                try_provider(state, body, fb_provider, fb_model, is_stream, slot_id).await;

            match fb_result {
                Ok((resp, usage)) => {
                    tracing::info!(fallback = %fb_provider, "fallback provider OK");
                    // Return `fb_model` (effective fallback model) for correct cost attribution.
                    (Ok(resp), fb_provider.clone(), usage, fb_model.to_string())
                }
                Err(fb_err) => {
                    tracing::warn!(
                        fallback = %fb_provider,
                        error = %fb_err,
                        "fallback provider également échoué"
                    );
                    // Double failure: return the last model attempted.
                    (Err(fb_err), fb_provider.clone(), None, fb_model.to_string())
                }
            }
        }
    }
}

/// Dispatches an OpenAI chunk stream with fallback — returns the raw `ChatCompletionStream`.
///
/// Unlike `dispatch_with_fallback`, this function returns the chunk stream
/// **before** SSE serialization. It is intended for the `messages.rs` handler,
/// which re-translates chunks into the Anthropic SSE event format.
///
/// # Differences from `dispatch_with_fallback`
/// - Returns `LlmResult<(ChatCompletionStream, String)>` (`stream`, `model_used`)
/// - No slot-passthrough support (opaque format, incompatible with re-translation)
/// - Vision gate included (same semantics as `dispatch_with_fallback`)
///
/// `pub(crate)`: shared with `handlers::messages`.
pub(crate) async fn dispatch_stream_with_fallback(
    state: &AppState,
    body: ChatCompletionRequest,
    alias: &crate::config::AliasTarget,
    has_image: bool,
) -> Result<(crate::commons::provider::ChatCompletionStream, String), ApiError> {
    let primary_result =
        try_provider_stream(state, body.clone(), &alias.provider, &alias.model).await;

    match primary_result {
        Ok(stream) => {
            // FIX 1 : débloquer le circuit breaker HalfOpen sur succès stream primaire.
            state
                .providers
                .circuit_breakers
                .record_success(&alias.provider);
            Ok((stream, alias.model.clone()))
        }
        Err(e) if !is_backend_error(&e) => Err(e),
        Err(primary_err) => {
            if let ApiError::Backend(ref llm_err) = primary_err {
                state
                    .providers
                    .circuit_breakers
                    .record_failure(&alias.provider, llm_err);
            }

            // Vision gate : même sémantique que dispatch_with_fallback.
            if has_image {
                tracing::warn!(
                    primary = %alias.provider,
                    error = %primary_err,
                    "primary vision provider échoué (stream) — fallback skipé"
                );
                return Err(ApiError::ServiceUnavailable {
                    message: format!(
                        "vision provider '{}' unavailable, no vision-capable fallback",
                        alias.provider
                    ),
                });
            }

            let Some(fb_provider) = &alias.fallback_provider else {
                return Err(primary_err);
            };

            let fb_model = alias.fallback_model.as_deref().unwrap_or(&alias.model);
            tracing::warn!(
                primary = %alias.provider,
                fallback = %fb_provider,
                error = %primary_err,
                "primary provider échoué (stream) — tentative fallback"
            );

            let fb_result = try_provider_stream(state, body, fb_provider, fb_model).await;
            match fb_result {
                Ok(stream) => {
                    tracing::info!(fallback = %fb_provider, "fallback provider OK (stream)");
                    // FIX 1 : débloquer le circuit breaker HalfOpen sur succès stream fallback.
                    state.providers.circuit_breakers.record_success(fb_provider);
                    Ok((stream, fb_model.to_string()))
                }
                Err(fb_err) => {
                    tracing::warn!(
                        fallback = %fb_provider,
                        error = %fb_err,
                        "fallback provider également échoué (stream)"
                    );
                    // FIX 1 : comptabiliser l'échec du fallback stream dans le circuit breaker.
                    if let ApiError::Backend(ref llm_err) = fb_err {
                        state
                            .providers
                            .circuit_breakers
                            .record_failure(fb_provider, llm_err);
                    }
                    Err(fb_err)
                }
            }
        }
    }
}

/// Tente un appel streaming à un provider spécifique.
///
/// Retourne le `ChatCompletionStream` brut (avant sérialisation SSE).
/// Ne supporte pas le slot-passthrough (format opaque incompatible).
///
/// # Errors
/// - `ApiError::Backend(LlmError::ProviderUnavailable)` si le circuit breaker est ouvert.
/// - `ApiError::ProviderNotFound` si le provider est absent de la config.
/// - `ApiError::Backend(...)` si le provider retourne une erreur.
async fn try_provider_stream(
    state: &AppState,
    mut body: ChatCompletionRequest,
    provider_name: &str,
    model: &str,
) -> Result<crate::commons::provider::ChatCompletionStream, ApiError> {
    if !state.providers.circuit_breakers.should_allow(provider_name) {
        tracing::warn!(
            provider = %provider_name,
            "circuit breaker ouvert (stream)"
        );
        return Err(ApiError::Backend(LlmError::ProviderUnavailable {
            provider: provider_name.to_string(),
            reason: "circuit breaker ouvert".to_string(),
        }));
    }

    body.model = model.to_string();
    body.stream = Some(true);

    let provider = state
        .providers
        .get(provider_name)
        .ok_or_else(|| ApiError::ProviderNotFound(provider_name.to_string()))?;

    let stream = provider.stream(body).await?;
    Ok(stream)
}

fn is_backend_error(e: &ApiError) -> bool {
    matches!(e, ApiError::Backend(_))
}

/// Attempts a call to a specific provider.
///
/// Returns `(response, usage)`:
/// - `usage` is `Some` only for the standard non-streaming path (provider returns a
///   `ChatCompletionResponse` with `usage` accessible before serialization).
/// - `usage` is `None` for streaming (SSE chunks without an aggregate) and the
///   slot-passthrough path (opaque bytes — re-deserialization is out of scope).
async fn try_provider(
    state: &AppState,
    mut body: ChatCompletionRequest,
    provider_name: &str,
    model: &str,
    is_stream: bool,
    slot_id: Option<u32>,
) -> Result<(Response, Option<Usage>), ApiError> {
    if !state.providers.circuit_breakers.should_allow(provider_name) {
        tracing::warn!(
            provider = %provider_name,
            "circuit breaker ouvert"
        );
        return Err(ApiError::Backend(LlmError::ProviderUnavailable {
            provider: provider_name.to_string(),
            reason: "circuit breaker ouvert".to_string(),
        }));
    }

    body.model = model.to_string();

    let enable_passthrough = state.config.server.enable_slot_passthrough;
    let should_inject = enable_passthrough && slot_id.is_some();

    if should_inject {
        // Slot-passthrough path: opaque bytes, usage cannot be captured.
        let resp = try_provider_with_slot(state, body, provider_name, is_stream, slot_id).await?;
        return Ok((resp, None));
    }

    let provider = state
        .providers
        .get(provider_name)
        .ok_or_else(|| ApiError::ProviderNotFound(provider_name.to_string()))?;

    if is_stream {
        // Streaming: SSE chunks expose no usage aggregate.
        let chunk_stream = provider.stream(body).await?;
        let circuit_breakers = state.providers.circuit_breakers.clone();
        let provider_id = provider_name.to_string();
        let sse_body = sse_stream_from_chunks(chunk_stream, circuit_breakers, provider_id);

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
                ApiError::Backend(LlmError::Custom {
                    message: format!("erreur construction réponse SSE: {}", e),
                })
            })?;

        Ok((response, None))
    } else {
        // Non-streaming: capture usage BEFORE serialization (the only available window).
        let completion = provider.complete(body).await?;
        let usage = completion.usage.clone();
        let response = (StatusCode::OK, Json(completion)).into_response();
        Ok((response, usage))
    }
}

/// Slot-passthrough path: injects `slot_id` and forwards the JSON body via `http_client` directly.
async fn try_provider_with_slot(
    state: &AppState,
    body: ChatCompletionRequest,
    provider_name: &str,
    is_stream: bool,
    slot_id: Option<u32>,
) -> Result<Response, ApiError> {
    use std::time::Duration;

    let provider_cfg = state
        .config
        .providers
        .get(provider_name)
        .ok_or_else(|| ApiError::ProviderNotFound(provider_name.to_string()))?
        .clone();

    let chat_url = format!(
        "{}/v1/chat/completions",
        provider_cfg.endpoint.trim_end_matches('/')
    );

    let body_value = serde_json::to_value(&body)
        .map_err(|e| ApiError::Backend(LlmError::Serialization { source: e }))?;
    let body_value = inject_slot_id_if_needed(
        body_value,
        slot_id,
        state.config.server.enable_slot_passthrough,
    );

    let http_client = state.providers.http_client();
    let mut req = http_client.post(&chat_url).json(&body_value);

    if let Some(Some(key)) = state.providers.resolved_api_keys.get(provider_name) {
        req = req.bearer_auth(key);
    }

    let response = req
        .timeout(Duration::from_secs(provider_cfg.timeout_secs))
        .send()
        .await
        .map_err(|e| {
            let llm_err = if e.is_timeout() {
                LlmError::Timeout {
                    elapsed_secs: provider_cfg.timeout_secs as f64,
                }
            } else {
                LlmError::Network {
                    source: Box::new(e),
                }
            };
            ApiError::Backend(llm_err)
        })?;

    let status = response.status();

    if !status.is_success() {
        let body_bytes = response.bytes().await.unwrap_or_default();
        return Response::builder()
            .status(status)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .body(Body::from(body_bytes))
            .map_err(|e| {
                ApiError::Backend(LlmError::Custom {
                    message: format!("erreur construction réponse erreur: {}", e),
                })
            });
    }

    if is_stream {
        use futures::StreamExt;
        let byte_stream = response
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));

        Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )
            .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))
            .header(header::CONNECTION, HeaderValue::from_static("keep-alive"))
            .body(Body::from_stream(byte_stream))
            .map_err(|e| {
                ApiError::Backend(LlmError::Custom {
                    message: format!("erreur construction réponse SSE slot: {}", e),
                })
            })
    } else {
        let body_bytes = response.bytes().await.map_err(|e| {
            ApiError::Backend(LlmError::Network {
                source: Box::new(e),
            })
        })?;

        Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .body(Body::from(body_bytes))
            .map_err(|e| {
                ApiError::Backend(LlmError::Custom {
                    message: format!("erreur construction réponse JSON slot: {}", e),
                })
            })
    }
}

/// Converts a stream of `LlmResult<ChatCompletionChunk>` into a stream of SSE bytes.
fn sse_stream_from_chunks(
    chunks: crate::commons::provider::ChatCompletionStream,
    circuit_breakers: Arc<CircuitBreakerRegistry>,
    provider_id: String,
) -> impl futures::Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> {
    use futures::StreamExt;

    chunks.map(move |result| {
        let line = match result {
            Ok(chunk) => match serde_json::to_string(&chunk) {
                Ok(json) => format!("data: {}\n\n", json),
                Err(e) => {
                    tracing::warn!("erreur sérialisation chunk SSE: {}", e);
                    return Ok(bytes::Bytes::new());
                }
            },
            Err(e) => {
                tracing::error!("erreur stream backend: {}", e);
                circuit_breakers.record_failure(&provider_id, &e);
                "data: [DONE]\n\n".to_string()
            }
        };
        Ok(bytes::Bytes::from(line))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    use crate::AppState;
    use crate::config::{
        AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig, VaultAwareConfig,
    };
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Config de test serveur minimale.
    fn test_server_cfg() -> ServerConfig {
        ServerConfig {
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
        }
    }

    /// Construit un `AppState` de test avec un alias ayant primary + fallback configurés.
    ///
    /// `primary_endpoint` : URL du primary provider (peut être KO pour forcer le fallback).
    /// `fallback_endpoint` : URL du fallback provider.
    /// `fb_model` : modèle déclaré côté fallback dans l'alias.
    fn make_state_with_fallback(
        primary_endpoint: &str,
        fallback_endpoint: &str,
        fb_model: &str,
    ) -> AppState {
        let mut providers = BTreeMap::new();
        providers.insert(
            "primary-provider".to_string(),
            ProviderConfig {
                endpoint: primary_endpoint.to_string(),
                api_key_env: None,
                timeout_secs: 2, // court pour ne pas ralentir les tests
            },
        );
        providers.insert(
            "fallback-provider".to_string(),
            ProviderConfig {
                endpoint: fallback_endpoint.to_string(),
                api_key_env: None,
                timeout_secs: 10,
            },
        );

        let mut aliases = HashMap::new();
        aliases.insert(
            "test-alias".to_string(),
            AliasTarget {
                provider: "primary-provider".to_string(),
                model: "primary-model".to_string(),
                fallback_provider: Some("fallback-provider".to_string()),
                fallback_model: Some(fb_model.to_string()),
                temperature_default: None,
                max_tokens_default: None,
                vision_capable: false,
            },
        );

        let config = Config {
            server: test_server_cfg(),
            providers,
            aliases,
            gateway: HashMap::new(),
            logging: LoggingConfig::default(),
            vault_aware: VaultAwareConfig::default(),
            messages: Default::default(),
        };

        AppState::for_test(config)
    }

    /// P1 : primary KO + fallback OK → `model_used_effective` == modèle du fallback.
    ///
    /// Vérifie que l'attribution de coût dans `QaEvent.model_used` reflète le provider
    /// qui a effectivement répondu, et non le primary pré-sélectionné avant le dispatch.
    #[tokio::test]
    async fn dispatch_fallback_ok_model_used_effective_is_fb_model() {
        // Serveur fallback OK — simule une réponse valide.
        let fallback_server = MockServer::start().await;
        let chat_resp = json!({
            "id": "chatcmpl-fallback",
            "object": "chat.completion",
            "created": 1234567890u64,
            "model": "fallback-model-real",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "réponse fallback"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 8, "completion_tokens": 3, "total_tokens": 11}
        });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&chat_resp))
            .mount(&fallback_server)
            .await;

        let fb_model = "fallback-model-real";
        // Primary → port fermé (connexion refusée → backend error → déclenche le fallback).
        let state = make_state_with_fallback(
            "http://127.0.0.1:1", // port fermé → erreur réseau garantie
            &fallback_server.uri(),
            fb_model,
        );

        let alias = state
            .config
            .aliases
            .get("test-alias")
            .expect("alias test-alias doit exister")
            .clone();

        let body = crate::commons::chat::ChatCompletionRequest {
            model: "test-alias".to_string(),
            messages: vec![crate::commons::chat::Message::user("test")],
            stream: None,
            temperature: None,
            max_tokens: None,
            tools: None,
            tool_choice: None,
            top_p: None,
            stop: None,
            chat_template_kwargs: None,
        };

        let (result, effective_provider, _usage, model_used_effective) =
            // has_image = false : requête texte, le fallback doit être tenté normalement.
            dispatch_with_fallback(&state, body, &alias, false, None, false).await;

        // La requête doit réussir via le fallback.
        assert!(result.is_ok(), "fallback doit répondre OK — primary KO");
        assert_eq!(
            effective_provider, "fallback-provider",
            "provider effectif doit être le fallback"
        );
        assert_eq!(
            model_used_effective, fb_model,
            "P1 BLOQUANT : model_used_effective doit être le modèle du fallback, \
             pas celui du primary"
        );
    }
}
