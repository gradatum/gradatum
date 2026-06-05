//! Handler POST /v1/embeddings
//!
//! Deux modes de dispatch selon les champs de la requête :
//!
//! Mode 1 — Local (gradatum-embed, prioritaire si `use_local_embedder: true` ou
//!            si `model` correspond à l'alias local configuré dans `AppState.local_embed_alias`) :
//!   - Utilise `Arc<dyn Embedder>` depuis `AppState.embedder`
//!   - Format de retour : EmbeddingResponse OpenAI-compat
//!   - Avantage : pas de réseau, latence ~17ms sur CPU local
//!
//! Mode 2 — Remote (pass-through HTTP, comportement hérité de llm-free-gateway-v2) :
//!   - Résolution alias → provider → forward HTTP vers `{endpoint}/v1/embeddings`
//!   - Rate limit par IP (F-MAJ-3 : ConnectInfo socket TCP)
//!   - Circuit breaker par provider
//!   - Fallback automatique : si primary répond 5xx / timeout / erreur réseau ET
//!     `fallback_provider` est configuré dans l'alias → tentative sur le fallback.
//!     Si `fallback_provider` est absent → comportement inchangé (erreur propagée).
//!
//! F-MAJ-3 : rate limit basé sur `ConnectInfo<SocketAddr>` (adresse TCP réelle).
//!
//! Codes d'erreur :
//! - 400 : alias inconnu
//! - 429 : rate limit dépassé
//! - 500 : provider absent de config
//! - 502 : erreur backend — tous fallbacks épuisés
//! - 503 : circuit breaker ouvert — tous fallbacks épuisés

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{ConnectInfo, Extension, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use tracing::instrument;

use crate::{
    commons::{
        embeddings::{EmbeddingData, EmbeddingRequest, EmbeddingResponse, EmbeddingUsage},
        error::LlmError,
    },
    error::ApiError,
    rate_limit::extract_client_ip_from_socket,
    registry::RequestLogEntry,
    AppState,
};

/// Handler POST /v1/embeddings
///
/// Axum 0.8 (axum-core 0.5) : utilise `Option<Extension<ConnectInfo<SocketAddr>>>` — voir chat.rs.
#[instrument(skip(state, connect_info, body), fields(model))]
pub async fn handler(
    State(state): State<AppState>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(body): Json<EmbeddingRequest>,
) -> Result<Response, ApiError> {
    // F-MAJ-3 : rate limit basé sur l'IP socket TCP réelle.
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

    let model_alias_owned: String = body.model.clone().unwrap_or_default();
    tracing::Span::current().record("model", model_alias_owned.as_str());

    // Mode local : si un Embedder est configuré ET que l'alias correspond.
    if let Some(embedder) = &state.embedder {
        let use_local = state
            .local_embed_alias
            .as_deref()
            .map(|la| la == model_alias_owned.as_str())
            .unwrap_or(false);

        if use_local {
            return embed_local(embedder, &body, &model_alias_owned).await;
        }
    }

    // Mode remote : résolution alias → provider HTTP.
    embed_remote(&state, body, &model_alias_owned, client_ip).await
}

/// Embedding local via `Arc<dyn Embedder>`.
async fn embed_local(
    embedder: &std::sync::Arc<dyn gradatum_embed::Embedder>,
    body: &EmbeddingRequest,
    model_alias: &str,
) -> Result<Response, ApiError> {
    // Collecter les textes depuis l'input.
    let texts: Vec<String> = match &body.input {
        crate::commons::embeddings::EmbeddingInput::Single(s) => vec![s.clone()],
        crate::commons::embeddings::EmbeddingInput::Batch(v) => v.clone(),
    };

    let count = texts.len();
    let texts_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();

    let embeddings = embedder.embed_batch(&texts_refs).await.map_err(|e| {
        ApiError::Backend(LlmError::Custom {
            message: format!("erreur embedder local: {}", e),
        })
    })?;

    // Calcul de l'usage approximatif.
    let total_chars: usize = texts.iter().map(|s| s.len()).sum();
    let prompt_tokens: u64 = (total_chars / 4) as u64;

    let data: Vec<EmbeddingData> = embeddings
        .into_iter()
        .enumerate()
        .map(|(i, emb)| EmbeddingData {
            object: "embedding".to_owned(),
            embedding: emb,
            index: i as u32,
        })
        .collect();

    let prompt_tokens_u32 = prompt_tokens.min(u32::MAX as u64) as u32;

    let response = EmbeddingResponse {
        object: "list".to_owned(),
        data,
        model: model_alias.to_owned(),
        usage: EmbeddingUsage {
            prompt_tokens: prompt_tokens_u32,
            total_tokens: prompt_tokens_u32,
        },
    };

    tracing::debug!(
        count = count,
        model = model_alias,
        "embeddings locaux générés"
    );

    Ok((StatusCode::OK, Json(response)).into_response())
}

/// Embedding remote via forward HTTP, avec fallback automatique.
///
/// Résout l'alias → dispatch sur le primary provider.
/// Si le primary échoue (5xx / timeout / erreur réseau) ET que l'alias déclare
/// un `fallback_provider` → tentative sur le fallback.
/// Si `fallback_provider` absent → comportement inchangé (erreur propagée, rétro-compat).
async fn embed_remote(
    state: &AppState,
    body: EmbeddingRequest,
    model_alias: &str,
    client_ip: std::net::IpAddr,
) -> Result<Response, ApiError> {
    let alias = state
        .config
        .aliases
        .get(model_alias)
        .ok_or_else(|| {
            let mut available: Vec<String> = state.config.aliases.keys().cloned().collect();
            available.sort();
            tracing::warn!(
                consumer = %client_ip,
                model = %model_alias,
                "alias inconnu (embeddings) — requête rejetée HTTP 404"
            );
            ApiError::AliasNotFound {
                alias: model_alias.to_owned(),
                available,
            }
        })?
        .clone();

    let start = Instant::now();

    let (result, effective_provider, effective_model) =
        embed_dispatch_with_fallback(state, body, &alias).await;

    let latency = start.elapsed();

    // Enregistrement circuit breaker sur le provider effectif.
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

    // Journalisation asynchrone.
    let registry = state.registry.clone();
    let alias_str = model_alias.to_owned();
    let error_msg = result.as_ref().err().map(|e| e.to_string());
    // Cloner avant le move dans le spawn pour conserver effective_provider pour record_request.
    let provider_for_log = effective_provider.clone();

    tokio::spawn(async move {
        let entry = RequestLogEntry {
            model_alias: alias_str,
            provider_real: provider_for_log,
            real_model: effective_model,
            route: "/v1/embeddings".to_owned(),
            latency_ms: Some(latency.as_millis() as u64),
            status_code,
            streamed: false,
            error_message: error_msg,
        };
        if let Err(e) = registry.log_request(entry).await {
            tracing::warn!("erreur journalisation requête embeddings: {}", e);
        }
    });

    state.metrics.record_request(
        "/v1/embeddings",
        model_alias,
        &effective_provider,
        status_code,
        Some(latency),
    );

    result
}

/// Dispatch une requête embeddings vers le primary provider, avec fallback automatique.
///
/// Retourne `(résultat, nom_provider_effectif, model_effectif)`.
///
/// Miroir de `dispatch_with_fallback` (chat.rs) adapté au chemin embeddings :
/// - Pas de streaming (embeddings toujours synchrones)
/// - Rétro-compat : si `alias.fallback_provider` est absent, erreur primary propagée sans changement.
async fn embed_dispatch_with_fallback(
    state: &AppState,
    body: EmbeddingRequest,
    alias: &crate::config::AliasTarget,
) -> (Result<Response, ApiError>, String, String) {
    let primary_result =
        try_embed_provider(state, body.clone(), &alias.provider, &alias.model).await;

    match primary_result {
        Ok(resp) => (Ok(resp), alias.provider.clone(), alias.model.clone()),
        // Erreur non-backend (ex : ProviderNotFound, alias invalide) — pas de fallback.
        Err(ref e) if !is_embed_backend_error(e) => (
            Err(primary_result.unwrap_err()),
            alias.provider.clone(),
            alias.model.clone(),
        ),
        Err(primary_err) => {
            if let ApiError::Backend(ref llm_err) = primary_err {
                state
                    .providers
                    .circuit_breakers
                    .record_failure(&alias.provider, llm_err);
            }

            let Some(fb_provider) = &alias.fallback_provider else {
                // Pas de fallback configuré → comportement inchangé (rétro-compat).
                return (
                    Err(primary_err),
                    alias.provider.clone(),
                    alias.model.clone(),
                );
            };

            tracing::warn!(
                primary = %alias.provider,
                fallback = %fb_provider,
                error = %primary_err,
                "primary embed provider échoué — tentative fallback"
            );

            let fb_model = alias.fallback_model.as_deref().unwrap_or(&alias.model);
            let fb_result = try_embed_provider(state, body, fb_provider, fb_model).await;

            match fb_result {
                Ok(resp) => {
                    tracing::info!(fallback = %fb_provider, "fallback embed provider OK");
                    (Ok(resp), fb_provider.clone(), fb_model.to_string())
                }
                Err(fb_err) => {
                    tracing::warn!(
                        fallback = %fb_provider,
                        error = %fb_err,
                        "fallback embed provider également échoué"
                    );
                    (Err(fb_err), fb_provider.clone(), fb_model.to_string())
                }
            }
        }
    }
}

/// Retourne `true` si l'erreur est une erreur backend (5xx / timeout / réseau)
/// qui justifie un fallback. Les erreurs de validation (4xx) ne déclenchent pas de fallback.
fn is_embed_backend_error(e: &ApiError) -> bool {
    matches!(e, ApiError::Backend(_))
}

/// Tente un appel embeddings vers un provider spécifique.
///
/// Gère le circuit breaker (vérification d'entrée) et l'appel HTTP.
/// L'enregistrement des succès/échecs circuit breaker est laissé à l'appelant
/// pour permettre au dispatch de choisir le bon provider à enregistrer.
async fn try_embed_provider(
    state: &AppState,
    body: EmbeddingRequest,
    provider_name: &str,
    model: &str,
) -> Result<Response, ApiError> {
    if !state.providers.circuit_breakers.should_allow(provider_name) {
        tracing::warn!(
            provider = %provider_name,
            "circuit breaker ouvert — requête embeddings rejetée"
        );
        return Err(ApiError::Backend(LlmError::ProviderUnavailable {
            provider: provider_name.to_string(),
            reason: "circuit breaker ouvert".to_string(),
        }));
    }

    let provider_cfg = state
        .config
        .providers
        .get(provider_name)
        .ok_or_else(|| ApiError::ProviderNotFound(provider_name.to_string()))?
        .clone();

    let embed_url = format!(
        "{}/v1/embeddings",
        provider_cfg.endpoint.trim_end_matches('/')
    );

    let client = state.providers.http_client();

    let mut forward_body = body;
    forward_body.model = Some(model.to_string());

    let mut req = client.post(&embed_url).json(&forward_body);
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
    let status_code = status.as_u16();

    if status.is_server_error() {
        // Propager comme ApiError::Backend pour déclencher le fallback.
        let body_bytes = response.bytes().await.unwrap_or_default();
        return Err(ApiError::Backend(LlmError::UpstreamError {
            status: status_code,
            message: String::from_utf8_lossy(&body_bytes).into_owned(),
        }));
    }

    if !status.is_success() {
        // Erreur 4xx : pas de fallback (erreur de validation), retourner telle quelle.
        let body_bytes = response.bytes().await.unwrap_or_default();
        return Ok(Response::builder()
            .status(status)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .body(Body::from(body_bytes))
            .map_err(|e| {
                ApiError::Backend(LlmError::Custom {
                    message: format!("erreur construction réponse passthrough: {}", e),
                })
            })?
            .into_response());
    }

    let body_bytes = response.bytes().await.map_err(|e| {
        ApiError::Backend(LlmError::Network {
            source: Box::new(e),
        })
    })?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .body(Body::from(body_bytes))
        .map_err(|e| {
            ApiError::Backend(LlmError::Custom {
                message: format!("erreur construction réponse embeddings: {}", e),
            })
        })?
        .into_response())
}
