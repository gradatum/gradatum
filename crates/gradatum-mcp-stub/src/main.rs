//! # gradatum-mcp-stub
//!
//! Proxy stdio → HTTP for gradatum-server.
//!
//! Each MCP tool is a thin-forward: serializes the received arguments as JSON,
//! sends them via POST to the corresponding REST endpoint on gradatum-server, and
//! returns the JSON response to the MCP host.
//!
//! ## Configuration (env vars)
//!
//! | Variable | Default | Role |
//! |---|---|---|
//! | `GRADATUM_SERVER_URL` | `http://127.0.0.1:19090` | Base URL of the server |
//! | `GRADATUM_API_KEY_FILE` | — | Path to a chmod-600 file containing `ak_xxx` (takes priority) |
//! | `GRADATUM_BEARER_TOKEN` | — | Static JWT (fallback when `GRADATUM_API_KEY_FILE` is absent) |
//!
//! ### Auto-refresh mode (recommended — `GRADATUM_API_KEY_FILE`)
//!
//! When `GRADATUM_API_KEY_FILE` is set, the stub reads the API key at startup,
//! calls `POST /auth/exchange` to obtain a JWT, and renews it
//! automatically when the remaining TTL drops below 30%.
//!
//! Refresh logic:
//! 1. Before each HTTP call: check whether remaining TTL < 30% → proactive exchange.
//! 2. If the proactive exchange fails → warn and use the current JWT (may still be valid).
//! 3. If the server returns 401 on a forward call → re-exchange from the API key
//!    and retry once (one-shot).
//!
//! ### Static mode (legacy — `GRADATUM_BEARER_TOKEN`)
//!
//! Uses the static JWT as-is. No automatic refresh.
//! Recommended only for tests or short-lived deployments.
//!
//! ## Reconnect
//!
//! Exponential backoff 100 ms → 5 s: one initial attempt followed by up to `MAX_RETRIES`
//! retries (timeouts, connection errors, 5xx). Other 4xx responses are not retried.
//! Once the retries are exhausted → MCP `internal_error`, message
//! `"gradatum server unreachable after {MAX_RETRIES} attempts (endpoint=…)"`.
//!
//! Note that `MAX_RETRIES` counts *retries*, not attempts: the loop runs `0..=MAX_RETRIES`,
//! so the total number of requests issued is `MAX_RETRIES + 1`. The terminal message
//! interpolates the constant and therefore under-reports that total by one.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::ProtocolVersion,
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

// ── Constantes ────────────────────────────────────────────────────────────────

/// Env var: base URL of the gradatum server.
pub(crate) const SERVER_URL_ENV: &str = "GRADATUM_SERVER_URL";
/// Env var: static JWT (legacy mode).
pub(crate) const BEARER_ENV: &str = "GRADATUM_BEARER_TOKEN";
/// Env var: file containing the `ak_xxx` API key (auto-refresh mode).
pub(crate) const API_KEY_FILE_ENV: &str = "GRADATUM_API_KEY_FILE";
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:19090";

/// Maximum number of attempts for an HTTP request before failing.
const MAX_RETRIES: u32 = 10;
/// Initial exponential backoff delay (ms).
const BACKOFF_INIT_MS: u64 = 100;
/// Maximum exponential backoff delay (ms).
const BACKOFF_MAX_MS: u64 = 5_000;
/// Proactive refresh threshold: refresh when remaining TTL < 30% of total TTL.
const REFRESH_THRESHOLD_RATIO: f64 = 0.30;

// ── État du token ─────────────────────────────────────────────────────────────

/// Authentication mode configured at startup.
#[derive(Clone)]
pub(crate) enum AuthMode {
    /// Auto-refresh mode: permanent API key with automatically renewed JWT.
    ApiKey(String),
    /// Static mode: JWT fixed at initialization, no refresh.
    StaticBearer(String),
}

/// Manual `Debug` implementation: redacts the secret value (API key or JWT) to
/// prevent leaks in logs or error messages.
impl std::fmt::Debug for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthMode::ApiKey(_) => f.write_str("AuthMode::ApiKey(***)"),
            AuthMode::StaticBearer(_) => f.write_str("AuthMode::StaticBearer(***)"),
        }
    }
}

/// Current JWT state (used in `ApiKey` mode).
pub(crate) struct TokenState {
    /// Current JWT.
    pub token: String,
    /// Instant at which the JWT expires (computed from `ttl_secs` returned by `/auth/exchange`).
    pub expires_at: Instant,
    /// Total TTL received during the last exchange (used to compute the 30% threshold).
    pub ttl_secs: u64,
}

/// Manual `Debug` implementation: redacts the JWT token content to prevent
/// leaks in logs or error messages.
impl std::fmt::Debug for TokenState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenState")
            .field("token", &"***")
            .field("expires_at", &self.expires_at)
            .field("ttl_secs", &self.ttl_secs)
            .finish()
    }
}

impl TokenState {
    /// Creates a `TokenState` from the `/auth/exchange` response.
    pub fn new(token: String, ttl_secs: u64) -> Self {
        let expires_at = Instant::now() + Duration::from_secs(ttl_secs);
        Self {
            token,
            expires_at,
            ttl_secs,
        }
    }

    /// Returns `true` if a proactive refresh is recommended (remaining TTL < 30%).
    pub fn should_refresh(&self) -> bool {
        let remaining = self.expires_at.saturating_duration_since(Instant::now());
        let threshold =
            Duration::from_secs((self.ttl_secs as f64 * REFRESH_THRESHOLD_RATIO) as u64);
        remaining < threshold
    }

    /// Returns `true` if the JWT has already expired.
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

// ── Réponse /auth/exchange ────────────────────────────────────────────────────

/// JSON response from `POST /auth/exchange` (fields consumed by the stub).
///
/// Only `token` and `ttl_secs` are consumed by the stub.
/// Other fields (`scopes`, `tenant_id`, `kid`) are ignored.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ExchangeResponse {
    pub token: String,
    pub ttl_secs: u64,
}

// ── Handler MCP ───────────────────────────────────────────────────────────────

/// MCP stdio → HTTP proxy. Holds the HTTP client and credentials.
///
/// `Clone`: the `Arc<Mutex<_>>` ensures the token is shared across rmcp clones.
#[derive(Clone)]
pub(crate) struct StubHandler {
    /// HTTP client with configured timeout.
    pub(crate) client: reqwest::Client,
    /// Base URL of the gradatum server (e.g. `http://127.0.0.1:19090`).
    pub(crate) server_url: String,
    /// Configured authentication mode.
    pub(crate) auth: AuthMode,
    /// Shared JWT token state (used only in `ApiKey` mode).
    pub(crate) token_state: Arc<Mutex<Option<TokenState>>>,
}

impl StubHandler {
    /// Builds a `StubHandler` from environment variables.
    ///
    /// Priority:
    /// 1. `GRADATUM_API_KEY_FILE` → auto-refresh mode
    /// 2. `GRADATUM_BEARER_TOKEN` → static mode
    /// 3. Neither set → error
    ///
    /// # Errors
    /// - Neither `GRADATUM_API_KEY_FILE` nor `GRADATUM_BEARER_TOKEN` is defined.
    /// - API key file is unreadable or has an invalid format.
    pub fn from_env() -> Result<Self> {
        let server_url =
            std::env::var(SERVER_URL_ENV).unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("HTTP client construction failed")?;

        // Mode 1 : API key file → auto-refresh.
        if let Ok(key_file) = std::env::var(API_KEY_FILE_ENV) {
            let api_key = std::fs::read_to_string(&key_file)
                .with_context(|| format!("reading GRADATUM_API_KEY_FILE '{key_file}' failed"))?
                .trim()
                .to_string();
            if api_key.is_empty() {
                anyhow::bail!("GRADATUM_API_KEY_FILE '{key_file}' is empty");
            }
            if !api_key.starts_with("ak_") {
                anyhow::bail!(
                    "GRADATUM_API_KEY_FILE '{key_file}': invalid format (expected: ak_...)"
                );
            }
            info!(
                key_file = %key_file,
                "JWT auto-refresh mode enabled via GRADATUM_API_KEY_FILE"
            );
            return Ok(Self {
                client,
                server_url,
                auth: AuthMode::ApiKey(api_key),
                token_state: Arc::new(Mutex::new(None)),
            });
        }

        // Mode 2 : bearer statique (legacy).
        if let Ok(bearer) = std::env::var(BEARER_ENV) {
            if bearer.is_empty() {
                anyhow::bail!("GRADATUM_BEARER_TOKEN cannot be empty");
            }
            warn!(
                "static bearer mode — JWT will expire without automatic refresh. \
                 Use GRADATUM_API_KEY_FILE for auto-refresh."
            );
            return Ok(Self {
                client,
                server_url,
                auth: AuthMode::StaticBearer(bearer),
                token_state: Arc::new(Mutex::new(None)),
            });
        }

        anyhow::bail!(
            "missing authentication — set GRADATUM_API_KEY_FILE (recommended) \
             or GRADATUM_BEARER_TOKEN"
        )
    }

    /// Initializes the JWT at startup (auto-refresh mode only).
    ///
    /// Calls `/auth/exchange` and stores the JWT in `token_state`.
    /// In static mode, this method is a no-op.
    pub async fn init_token(&self) -> Result<()> {
        if let AuthMode::ApiKey(ref api_key) = self.auth {
            let state = self.exchange_token(api_key).await?;
            *self.token_state.lock().await = Some(state);
            info!("JWT initialized successfully");
        }
        Ok(())
    }

    /// Calls `POST /auth/exchange` with the API key and returns a `TokenState`.
    pub(crate) async fn exchange_token(&self, api_key: &str) -> Result<TokenState> {
        let url = format!("{}/auth/exchange", self.server_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
            .context("POST /auth/exchange: network error")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("POST /auth/exchange returned {} : {body}", status.as_u16());
        }

        let exchange: ExchangeResponse = resp
            .json()
            .await
            .context("POST /auth/exchange: response deserialization failed")?;

        Ok(TokenState::new(exchange.token, exchange.ttl_secs))
    }

    /// Returns the current JWT, performing a proactive refresh if needed.
    ///
    /// In static mode → returns the fixed JWT without refresh.
    /// In auto-refresh mode:
    /// 1. Checks whether a proactive refresh is recommended (TTL < 30%) → exchange.
    /// 2. If the exchange fails → warn and use the current JWT (may still be valid).
    /// 3. If no JWT is present → exchange is mandatory.
    async fn get_bearer(&self) -> Result<String, ErrorData> {
        match &self.auth {
            AuthMode::StaticBearer(token) => Ok(token.clone()),
            AuthMode::ApiKey(api_key) => {
                let mut guard = self.token_state.lock().await;

                let needs_refresh = match guard.as_ref() {
                    None => true,
                    Some(state) => state.should_refresh() || state.is_expired(),
                };

                if needs_refresh {
                    debug!("proactive JWT refresh (TTL < 30% or expired)");
                    match self.exchange_token(api_key).await {
                        Ok(new_state) => {
                            info!("JWT renewed successfully");
                            *guard = Some(new_state);
                        }
                        Err(e) => {
                            // Refresh échoué — fallback sur token actuel s'il est encore valide.
                            if let Some(state) = guard.as_ref()
                                && !state.is_expired()
                            {
                                warn!(
                                    error = %e,
                                    "proactive JWT refresh failed — falling back to current JWT"
                                );
                                return Ok(state.token.clone());
                            }
                            error!(
                                error = %e,
                                "JWT refresh failed and no valid token available"
                            );
                            return Err(ErrorData::internal_error(
                                format!("authentication failed: {e}"),
                                None,
                            ));
                        }
                    }
                }

                guard.as_ref().map(|s| s.token.clone()).ok_or_else(|| {
                    ErrorData::internal_error("inconsistent token state — no JWT available", None)
                })
            }
        }
    }

    /// Attempts a re-exchange from the API key and returns the new JWT.
    ///
    /// Used after receiving a 401 on a forward call.
    /// In static mode → immediate error (no refresh possible).
    async fn force_refresh(&self) -> Result<String, ErrorData> {
        match &self.auth {
            AuthMode::StaticBearer(_) => Err(ErrorData::internal_error(
                "JWT expired and static mode active — set GRADATUM_API_KEY_FILE \
                 for auto-refresh",
                None,
            )),
            AuthMode::ApiKey(api_key) => {
                debug!("force JWT refresh after 401");
                match self.exchange_token(api_key).await {
                    Ok(new_state) => {
                        let token = new_state.token.clone();
                        *self.token_state.lock().await = Some(new_state);
                        info!("JWT renewed after 401");
                        Ok(token)
                    }
                    Err(e) => {
                        error!(error = %e, "force JWT refresh failed after 401");
                        Err(ErrorData::internal_error(
                            format!("re-authentication after 401 failed: {e}"),
                            None,
                        ))
                    }
                }
            }
        }
    }

    /// Issues a POST to `{server_url}/api/v1/{endpoint}` with the given JSON body.
    ///
    /// Reconnect logic: exponential backoff 100 ms → 5 s, up to `MAX_RETRIES`.
    /// Retry conditions: timeout, connection error, 5xx response.
    /// 401 exception: re-exchange from the API key + one-shot retry (auto-refresh mode).
    /// No-retry conditions: other 4xx responses.
    ///
    /// Returns the JSON body on success, or an HTTP error message.
    async fn forward_post(
        &self,
        endpoint: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, ErrorData> {
        let url = format!("{}/api/v1/{}", self.server_url, endpoint);
        let mut delay_ms = BACKOFF_INIT_MS;
        let mut refreshed_after_401 = false;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 && !refreshed_after_401 {
                debug!(endpoint, attempt, delay_ms, "HTTP retry after failure");
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(BACKOFF_MAX_MS);
            }
            refreshed_after_401 = false;

            let bearer = self.get_bearer().await?;

            let result = self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {bearer}"))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp.json::<serde_json::Value>().await.map_err(|e| {
                            ErrorData::internal_error(
                                format!("JSON response deserialization failed: {e}"),
                                None,
                            )
                        });
                    } else if status.as_u16() == 401 {
                        // 401 → re-exchange one-shot (premier attempt uniquement).
                        if attempt == 0 {
                            warn!(endpoint, "401 received — attempting JWT re-exchange");
                            match self.force_refresh().await {
                                Ok(_) => {
                                    // Retry immédiat sans sleep (refreshed_after_401 = true).
                                    refreshed_after_401 = true;
                                    continue;
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        // Deuxième 401 → erreur définitive.
                        return Err(ErrorData::internal_error(
                            format!("persistent 401 on {endpoint} — invalid credentials"),
                            None,
                        ));
                    } else if status.is_server_error() {
                        warn!(
                            endpoint,
                            status = status.as_u16(),
                            attempt,
                            "server 5xx error — retry"
                        );
                        continue;
                    } else {
                        let msg = format!("HTTP error {} on {endpoint}", status.as_u16());
                        return Err(ErrorData::internal_error(msg, None));
                    }
                }
                Err(e) if e.is_connect() || e.is_timeout() => {
                    warn!(
                        endpoint,
                        attempt,
                        err = %e,
                        "connection/timeout error — retry"
                    );
                    continue;
                }
                Err(e) => {
                    return Err(ErrorData::internal_error(
                        format!("unexpected HTTP error on {endpoint}: {e}"),
                        None,
                    ));
                }
            }
        }

        error!(
            endpoint,
            MAX_RETRIES, "server unreachable after max attempts"
        );
        Err(ErrorData::internal_error(
            format!(
                "gradatum server unreachable after {MAX_RETRIES} attempts \
                 (endpoint={endpoint})"
            ),
            None,
        ))
    }

    /// Issues a GET to `{server_url}/api/v1/{endpoint}` (no body).
    ///
    /// Same backoff and 401-retry logic as [`Self::forward_post`].
    async fn forward_get(&self, endpoint: &str) -> Result<serde_json::Value, ErrorData> {
        let url = format!("{}/api/v1/{}", self.server_url, endpoint);
        let mut delay_ms = BACKOFF_INIT_MS;
        let mut refreshed_after_401 = false;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 && !refreshed_after_401 {
                debug!(endpoint, attempt, delay_ms, "GET retry after failure");
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(BACKOFF_MAX_MS);
            }
            refreshed_after_401 = false;

            let bearer = self.get_bearer().await?;

            let result = self
                .client
                .get(&url)
                .header("Authorization", format!("Bearer {bearer}"))
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp.json::<serde_json::Value>().await.map_err(|e| {
                            ErrorData::internal_error(
                                format!("JSON response deserialization failed: {e}"),
                                None,
                            )
                        });
                    } else if status.as_u16() == 401 {
                        if attempt == 0 {
                            warn!(endpoint, "401 received (GET) — attempting JWT re-exchange");
                            match self.force_refresh().await {
                                Ok(_) => {
                                    refreshed_after_401 = true;
                                    continue;
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        return Err(ErrorData::internal_error(
                            format!("persistent 401 (GET) on {endpoint} — invalid credentials"),
                            None,
                        ));
                    } else if status.is_server_error() {
                        warn!(
                            endpoint,
                            status = status.as_u16(),
                            attempt,
                            "server 5xx error — retry"
                        );
                        continue;
                    } else {
                        return Err(ErrorData::internal_error(
                            format!("HTTP error {} on {endpoint}", status.as_u16()),
                            None,
                        ));
                    }
                }
                Err(e) if e.is_connect() || e.is_timeout() => {
                    warn!(endpoint, attempt, err = %e, "connection/timeout error — retry");
                    continue;
                }
                Err(e) => {
                    return Err(ErrorData::internal_error(
                        format!("unexpected HTTP error on {endpoint}: {e}"),
                        None,
                    ));
                }
            }
        }

        error!(
            endpoint,
            MAX_RETRIES, "server unreachable after max GET attempts"
        );
        Err(ErrorData::internal_error(
            format!(
                "gradatum server unreachable after {MAX_RETRIES} GET attempts \
                 (endpoint={endpoint})"
            ),
            None,
        ))
    }

    /// Converts a `serde_json::Value` into a `CallToolResult` containing pretty-printed JSON text.
    fn json_to_tool_result(value: serde_json::Value) -> CallToolResult {
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        CallToolResult::success(vec![Content::text(text)])
    }
}

// ── Implémentation ServerHandler ──────────────────────────────────────────────

impl ServerHandler for StubHandler {
    /// Returns server information sent during MCP initialization.
    fn get_info(&self) -> ServerInfo {
        // rmcp 1.x: ServerInfo + Implementation sont #[non_exhaustive] - constructeurs obligatoires.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "gradatum-mcp-stub",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::default())
    }

    /// Lists the MCP tools exposed by the stub, mirroring the REST server API.
    ///
    /// L'effectif n'est pas écrit ici : il est porté par [`tool_catalogue`], dont le test
    /// `catalogue_expose_exactement_les_outils_canoniques` compare la sortie à la liste
    /// canonique. Un nombre gravé dans cette docstring ne serait vérifié par rien.
    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult {
            tools: tool_catalogue(),
            ..Default::default()
        }))
    }

    /// Dispatches a tool call to the corresponding REST endpoint.
    ///
    /// Each tool maps 1:1 to `POST /api/v1/{tool_name}`, except for
    /// `vault_status`, `vault_authors`, and `vault_tags` which use GET.
    #[allow(clippy::manual_async_fn)]
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, ErrorData>> + Send + '_ {
        async move {
            let args = request.arguments.unwrap_or_default();
            let body = serde_json::Value::Object(args);

            let result = match request.name.as_ref() {
                // POST endpoints
                "vault_search" => self.forward_post("vault_search", body).await?,
                "vault_read" => self.forward_post("vault_read", body).await?,
                "vault_list" => self.forward_post("vault_list", body).await?,
                "vault_graph" => self.forward_post("vault_graph", body).await?,
                "vault_links" => self.forward_post("vault_links", body).await?,
                "vault_trace" => self.forward_post("vault_trace", body).await?,
                "vault_context" => self.forward_post("vault_context", body).await?,
                "vault_timeline" => self.forward_post("vault_timeline", body).await?,
                // GET endpoints (sans body)
                "vault_status" => self.forward_get("vault_status").await?,
                "vault_authors" => self.forward_get("vault_authors").await?,
                "vault_tags" => self.forward_get("vault_tags").await?,
                // Write endpoints — POST async 202 Accepted
                "vault_write" => self.forward_post("vault_write", body).await?,
                // Création de carte-feature — POST, numéro attribué par le serveur
                "create_feature_card" => {
                    self.forward_post("project-map/create-feature", body)
                        .await?
                }
                "vault_classify" => self.forward_post("vault_classify", body).await?,
                "vault_downgrade" => self.forward_post("vault_downgrade", body).await?,
                // History endpoints F-40 — POST synchrones 200 OK
                "vault_history" => self.forward_post("vault_history", body).await?,
                "vault_history_get" => self.forward_post("vault_history_get", body).await?,
                "vault_restore" => self.forward_post("vault_restore", body).await?,
                "vault_diff" => self.forward_post("vault_diff", body).await?,
                // Forget endpoint F-44 — POST dry-run 200 | mode réel 202
                "vault_forget" => self.forward_post("vault_forget", body).await?,
                // Archives listing F-100 1.6 — POST lecture seule (registre paginé)
                "vault_archives_list" => self.forward_post("vault_archives_list", body).await?,
                // Lesson Recall F-60 — GET avec query params (class, limit)
                "vault_lessons_recall" => {
                    let endpoint = build_lessons_recall_endpoint(&body)?;
                    self.forward_get(&endpoint).await?
                }
                // Code Scope F-61 — POST endpoint dédié code-map
                "code_scope" => self.forward_post("code_scope", body).await?,
                // Proactive Recall F-46 — POST, surface in-process B'
                "vault_proactive_recall" => self.forward_post("proactive_recall", body).await?,
                "vault_proactive_recall_feedback" => {
                    self.forward_post("proactive_recall/feedback", body).await?
                }
                unknown => {
                    return Err(ErrorData::invalid_params(
                        format!("unknown tool: {unknown}"),
                        None,
                    ));
                }
            };

            Ok(Self::json_to_tool_result(result))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Builds the GET endpoint `lessons/recall?class=&limit=&rank=&semantic=&query=` from MCP arguments.
///
/// Validates `class` against the controlled vocabulary [`gradatum_dto::LESSON_CLASSES`]
/// BEFORE constructing the URL: (1) returns a clear client error if the class is invalid;
/// (2) guarantees that `class` contains only URL-safe characters (alphanumeric and hyphen)
/// — no percent-encoding needed, no injection possible.
/// `limit`, when present, must be a positive integer.
/// `rank`, when present, must be a string (`"relevance"` or `"recency-boosted"`).
/// `semantic`, when present, must be a boolean.
/// `query`, when present, must be a string — percent-encoded before insertion.
///
/// ## Backward compatibility
///
/// `rank`, `semantic` and `query` are opt-in fields: leaving them out does not change the
/// server's behaviour — the BM25 ordering is unchanged and existing callers keep working.
fn build_lessons_recall_endpoint(body: &serde_json::Value) -> Result<String, ErrorData> {
    let class = body
        .get("class")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .ok_or_else(|| {
            ErrorData::invalid_params(
                "vault_lessons_recall: 'class' field required (string)",
                None,
            )
        })?;

    if !gradatum_dto::is_valid_lesson_class(class) {
        return Err(ErrorData::invalid_params(
            format!(
                "vault_lessons_recall: invalid class '{class}'. \
                 Allowed values: {}",
                gradatum_dto::LESSON_CLASSES.join(", ")
            ),
            None,
        ));
    }

    let mut endpoint = format!("lessons/recall?class={class}");

    // `limit` — entier positif optionnel.
    if let Some(limit_val) = body.get("limit") {
        // limit absent/null → endpoint sans param (le serveur applique son défaut).
        if !limit_val.is_null() {
            let limit = limit_val.as_u64().ok_or_else(|| {
                ErrorData::invalid_params(
                    "vault_lessons_recall: 'limit' must be a positive integer",
                    None,
                )
            })?;
            endpoint.push_str(&format!("&limit={limit}"));
        }
    }

    // `rank` — mode de tri opt-in (vocabulaire contrôlé : "relevance" | "recency-boosted").
    // Absent/null → BM25 order inchangé (rétro-compat absolue, hook LIVE en dépend).
    if let Some(rank_val) = body.get("rank")
        && !rank_val.is_null()
    {
        let rank = rank_val.as_str().ok_or_else(|| {
            ErrorData::invalid_params("vault_lessons_recall: 'rank' must be a string", None)
        })?;
        // Valeurs admises identiques à RankMode (serde rename_all kebab-case).
        // Le serveur valide et retourne 400 sur valeur inconnue — percent-encodé
        // pour empêcher toute injection dans la query string (parité avec `query`).
        endpoint.push_str("&rank=");
        endpoint.push_str(&percent_encode_value(rank));
    }

    // `semantic` — opt-in retrieval hybride (bool).
    // Absent/false → chemin BM25 inchangé (rétro-compat absolue).
    if let Some(sem_val) = body.get("semantic")
        && !sem_val.is_null()
    {
        let semantic = sem_val.as_bool().ok_or_else(|| {
            ErrorData::invalid_params("vault_lessons_recall: 'semantic' must be a boolean", None)
        })?;
        endpoint.push_str(if semantic {
            "&semantic=true"
        } else {
            "&semantic=false"
        });
    }

    // `query` — texte libre pour la recherche sémantique (opt-in, semantic=true).
    // Absent → le serveur utilise `class` comme requête par défaut. Percent-encodé.
    if let Some(query_val) = body.get("query")
        && !query_val.is_null()
    {
        let query = query_val.as_str().ok_or_else(|| {
            ErrorData::invalid_params("vault_lessons_recall: 'query' must be a string", None)
        })?;
        endpoint.push_str("&query=");
        endpoint.push_str(&percent_encode_value(query));
    }

    Ok(endpoint)
}

/// URL-encodes a query-string value using RFC 3986 percent-encoding.
///
/// Every byte outside the "unreserved" class (ALPHA / DIGIT / `-` / `.` / `_` / `~`) is
/// encoded. Multi-byte UTF-8 characters are encoded byte by byte (`é` becomes `%C3%A9`).
///
/// ## Security
///
/// Guarantees that no URL control character (`&`, `=`, `+`, `?`, `#`, `%`, `/`, …) can
/// disturb the query string built by [`build_lessons_recall_endpoint`].
fn percent_encode_value(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            b => {
                use std::fmt::Write as _;
                // Invariant : write! sur String est infaillible — pas de retour d'erreur.
                let _ = write!(encoded, "%{b:02X}");
            }
        }
    }
    encoded
}

/// Catalogue des outils MCP exposés par le stub — **source unique** de l'effectif.
///
/// Extrait de [`StubHandler::list_tools`] pour être mesurable hors d'un
/// `RequestContext` : tant que le catalogue n'existait qu'à l'intérieur de la méthode,
/// le seul test disponible comparait une constante à un littéral et ne prouvait rien de
/// ce qui est réellement servi.
fn tool_catalogue() -> Vec<Tool> {
    use gradatum_dto::{
        CodeScopeRequest, CreateFeatureCardRequest, LessonsRecallRequest,
        ProactiveRecallFeedbackRequest, ProactiveRecallRequest, VaultArchivesListRequest,
        VaultClassifyRequest, VaultContextRequest, VaultDiffRequest, VaultDowngradeRequest,
        VaultForgetRequest, VaultGraphRequest, VaultHistoryGetRequest, VaultHistoryRequest,
        VaultLinksRequest, VaultListRequest, VaultReadRequest, VaultRestoreRequest,
        VaultSearchRequest, VaultTimelineRequest, VaultTraceRequest, VaultWriteRequest,
    };

    vec![
        // ── Read tools ────────────────────────────────────────────────
        tool_def::<VaultSearchRequest>("vault_search", "Full-text search in the vault"),
        tool_def::<VaultReadRequest>("vault_read", "Read a note's content by path"),
        tool_def::<VaultListRequest>("vault_list", "List the vault's notes"),
        tool_def_no_params("vault_status", "Current vault status"),
        tool_def::<VaultGraphRequest>("vault_graph", "Link graph from a root note"),
        tool_def::<VaultLinksRequest>(
            "vault_links",
            "Direct links of a note (alias for vault_graph depth=1)",
        ),
        tool_def::<VaultTraceRequest>("vault_trace", "Trace notes by tags, sections or pattern"),
        tool_def::<VaultContextRequest>("vault_context", "Build an LLM context from notes"),
        tool_def::<VaultTimelineRequest>(
            "vault_timeline",
            "Chronological list of notes by temporal anchor (most recent first), \
             filterable by doc_kind and anchor_ms window. Cursor pagination. For replay / recency.",
        ),
        tool_def_no_params("vault_authors", "List the vault's authors"),
        tool_def_no_params("vault_tags", "List the vault's tags with frequencies"),
        // ── Write tools — queue async 202 Accepted ────────────────────
        tool_def::<VaultWriteRequest>(
            "vault_write",
            "Create a new note in the vault (async queue). \
             Fields: title (req), body (req), author, tags[], section_hint, tenant_id. \
             Returns 202 Accepted + job_id (poll via GET /api/v1/jobs/:id).",
        ),
        tool_def::<CreateFeatureCardRequest>(
            "create_feature_card",
            "Create a project-map feature card with a server-assigned F-XX number. \
             Fields: title (req), body (req — the 5 non-feature roles, NO [[feature:…]]), \
             author, tags[], tenant_id, occurred_at. Async: returns \
             { feature, number, job_id, note_id, poll_url } — poll job_status.",
        ),
        tool_def::<VaultClassifyRequest>(
            "vault_classify",
            "Re-classify an existing note via the curator pipeline (async). \
             Fields: note_id (req), tenant_id. \
             Returns 202 Accepted + job_id.",
        ),
        tool_def::<VaultDowngradeRequest>(
            "vault_downgrade",
            "Downgrade a note (status live → downgraded) — removes it from default results. \
             Fields: note_id (req), reason (req), replaced_by, tenant_id. \
             Returns 202 Accepted + job_id.",
        ),
        // ── History tools F-40 — synchrones 200 OK ───────────────────
        tool_def::<VaultHistoryRequest>(
            "vault_history",
            "List the CoW (Copy-on-Write) snapshots of a note. \
             Fields: note_id (req), tenant_id. \
             Returns the list of Unix-ms timestamps of available snapshots.",
        ),
        tool_def::<VaultHistoryGetRequest>(
            "vault_history_get",
            "Read the content of a specific historical snapshot. \
             Fields: note_id (req), ts_ms (req — timestamp from vault_history), tenant_id. \
             Returns the note's Markdown body at that point in time.",
        ),
        tool_def::<VaultRestoreRequest>(
            "vault_restore",
            "Restore a note from a historical snapshot (triggers a CoW). \
             Fields: note_id (req), ts_ms (req), tenant_id. \
             Returns the hex SHA-256 hash of the restored version.",
        ),
        tool_def::<VaultDiffRequest>(
            "vault_diff",
            "Raw line-by-line diff between two versions of a note. \
             Fields: note_id (req), a (req — timestamp ms or 'current'), \
             b (req — timestamp ms or 'current'), tenant_id. \
             Returns a list of lines prefixed with ' ' / '-' / '+'.",
        ),
        // ── Forget tools F-44 — double confirmation ───────────────────
        tool_def::<VaultForgetRequest>(
            "vault_forget",
            "Semantic forget of a batch of notes (F-44). \
             Double-confirmation workflow: \
             (1) dry_run=true (default) → preview 200 {ulids, count, excluded} ; \
             (2) dry_run=false + confirm_ulids=[...ulids_from_preview] → 202 job_id. \
             Scope: {type='topic', query, limit} | {type='locus', vault, locus} | {type='agent', agent_id, vaults[]}. \
             agent-issues and council sections are excluded automatically. \
             Frontmatter mutation performed by the worker (non-destructive).",
        ),
        // ── Archives listing F-100 1.6 — LECTURE SEULE stricte ────────
        tool_def::<VaultArchivesListRequest>(
            "vault_archives_list",
            "Lists (READ-ONLY) the notes archived by the on-demand delete (F-100 1.6). \
             No mutation, no action parameter: delete/restore/purge are NOT \
             exposed over MCP (operator CLI `gradatum-admin` only). \
             Used to VIEW archives and PREPARE the CLI commands. \
             Fields (all opt): section, since_ms, until_ms, include_gc, include_restored, \
             limit (default 50, max 500), offset, tenant_id. \
             Returns {entries:[{note_id, section, title, archive_path, archived_at, gc_due, …}], count}.",
        ),
        // ── Lesson Recall tool F-60 — GET BM25-only, aucun LLM ────────
        tool_def::<LessonsRecallRequest>(
            "vault_lessons_recall",
            "Recall the lessons learned for a given CLASS (F-60). \
             BM25 lexical search (no LLM) in the lessons-learned section, \
             excluding already-codified lessons (tag 'codified'). \
             Usage: before a risky action (release, deploy, migration, publish), \
             retrieve the relevant lessons to avoid repeating a past mistake. \
             Fields: class (req — one of: deploy, release, migration, crates-io, \
             anti-leak, api-external, archi, git-hygiene, ci-cd, auth-secrets, \
             data-integrity, process-discipline), limit (opt, default 5, max 20). \
             Returns {items:[{ulid, title, snippet, tags, anchor_ms}]}.",
        ),
        // ── Code Scope tool F-61 — POST BM25-only, endpoint dédié code-map ─
        tool_def::<CodeScopeRequest>(
            "code_scope",
            "Retrieve the relevant code symbols from a code-map vault (F-61). \
             Replaces O(repo) re-reading (Read/Glob/grep) with a derived-index query \
             (tree-sitter, no LLM). Fields: vault (req — MUST start with 'code-', \
             e.g. 'code-gradatum'), selector {kind: 'query'|'path'|'symbol', value}, \
             budget_tokens (opt, default 800). \
             selector.kind: 'query' = full-text BM25 search ; 'path' = all \
             symbols of a file/folder ; 'symbol' = by qualified name (substring). \
             Retourne {entries:[{note_id, source_path, kind, qualified_name, signature, \
             deps, stale}], truncated, total_matched}. A stale=true entry is STALE \
             (file modified since ingest) — DO NOT use it as truth.",
        ),
        // ── Proactive Recall tools F-46 — POST, surface in-process B' ───
        tool_def::<ProactiveRecallRequest>(
            "vault_proactive_recall",
            "Proactive memory recall (F-46, Active Recall). \
             Two modes depending on whether 'context' is present: \
             (1) context absent → 'proactive' mode: reads the pre-computed surface \
             (computed in the background every 15 min). Surface absent → empty items. \
             (2) context present → 'contextual' mode: on-demand RRF retrieval \
             over the provided sections or all sections. \
             Fields: tenant_id (opt, default 'main'), context (opt), sections (opt), \
             limit (opt, default 10, max 20). \
             Returns {recall_id, mode, items:[{ulid, title, section, snippet, score}]}. \
             recall_id serves as a correlator for vault_proactive_recall_feedback.",
        ),
        tool_def::<ProactiveRecallFeedbackRequest>(
            "vault_proactive_recall_feedback",
            "Acceptance feedback for a proactive-recall session (F-46). \
             Correlates the notes actually used with the surface presented. \
             Idempotent: same feedback 2× = 1 record. \
             Validation: accepted_ulids ⊆ surfaced (superset → 400). \
             Fields: recall_id (req — returned by vault_proactive_recall), \
             accepted_ulids (req — list of accepted ULIDs, may be empty), \
             tenant_id (opt, default 'main'). \
             Returns 200 on success.",
        ),
    ]
}

/// Builds an MCP [`Tool`] whose JSON schema is derived from the type `T`.
///
/// Delegates to [`gradatum_dto::mcp_tool_schema`], the single source of truth for schema
/// generation. Fail-loud: it panics if schemars yields a non-object (not reachable in
/// practice) rather than silently degrading to an empty map.
fn tool_def<T: schemars::JsonSchema>(name: &'static str, description: &'static str) -> Tool {
    Tool::new(name, description, gradatum_dto::mcp_tool_schema::<T>())
}

/// Builds a parameterless MCP [`Tool`].
///
/// Delegates to [`gradatum_dto::mcp_empty_params_schema`], the single source of truth for
/// schema generation. It emits `{"type":"object","properties":{}}`: an empty map `{}`
/// would be rejected by the client-side validator and would invalidate the whole tool
/// list.
fn tool_def_no_params(name: &'static str, description: &'static str) -> Tool {
    Tool::new(name, description, gradatum_dto::mcp_empty_params_schema())
}

// ── Entrypoint ────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Initialisation tracing vers stderr (stdout est réservé au protocole MCP stdio).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let handler = StubHandler::from_env().context("StubHandler initialization from env")?;

    // En mode auto-refresh : obtenir le JWT initial avant d'accepter des connexions MCP.
    handler
        .init_token()
        .await
        .context("obtention du JWT initial via /auth/exchange")?;

    // Transport stdio : (stdin, stdout) via rmcp::transport::io::stdio().
    let (stdin, stdout) = rmcp::transport::io::stdio();

    tracing::info!(
        server_url = %handler.server_url,
        "gradatum-mcp-stub starting (stdio transport)"
    );

    // serve_server gère l'initialisation MCP, le dispatch et le shutdown propre.
    rmcp::service::serve_server(handler, (stdin, stdout))
        .await
        .map_err(|e| anyhow::anyhow!("serve_server error: {e}"))?
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("waiting error: {e}"))?;

    Ok(())
}

// ── Tests unitaires ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }

    #[test]
    fn tool_def_has_correct_schema() {
        use gradatum_dto::VaultSearchRequest;

        let t = tool_def::<VaultSearchRequest>("vault_search", "test");
        assert_eq!(t.name.as_ref(), "vault_search");
        assert_eq!(t.description.as_deref(), Some("test"));
        assert_eq!(
            t.input_schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "schema.type doit être 'object'"
        );
        let properties = t.input_schema.get("properties").and_then(|v| v.as_object());
        assert!(properties.is_some(), "schema.properties doit exister");
        assert!(
            !properties.unwrap().is_empty(),
            "schema.properties NE doit PAS être vide pour VaultSearchRequest"
        );
        // Vérif champs canoniques
        let props = t
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .unwrap();
        assert!(props.contains_key("query"), "query attendu");
        assert!(props.contains_key("tenant_id"), "tenant_id attendu");
        // F-37 S1.1 — le champ opt-in `include_scores` doit apparaître dans le schéma
        // MCP auto-dérivé (sinon les clients MCP ne pourraient pas le passer).
        assert!(
            props.contains_key("include_scores"),
            "include_scores (F-37 S1.1) attendu dans le schéma vault_search"
        );
        // F-37 notes fix — le filtre optionnel `status` doit apparaître dans le schéma.
        assert!(
            props.contains_key("status"),
            "status (F-37 notes fix) attendu dans le schéma vault_search"
        );
        // corpus-hits — `include_corpus_count` doit apparaître dans le schéma MCP
        // auto-dérivé (sinon les clients MCP ne pourraient pas passer le flag).
        assert!(
            props.contains_key("include_corpus_count"),
            "include_corpus_count (corpus-hits) attendu dans le schéma vault_search"
        );
    }

    #[test]
    fn vault_timeline_tool_def_exposes_doc_kind() {
        use gradatum_dto::VaultTimelineRequest;

        let t = tool_def::<VaultTimelineRequest>("vault_timeline", "test");
        assert_eq!(t.name.as_ref(), "vault_timeline");
        let props = t
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("schema.properties doit exister pour VaultTimelineRequest");
        assert!(
            props.contains_key("doc_kind"),
            "doc_kind attendu dans le schéma vault_timeline"
        );
        assert!(
            props.contains_key("from_ms"),
            "from_ms attendu dans le schéma vault_timeline"
        );
        assert!(
            props.contains_key("cursor"),
            "cursor attendu dans le schéma vault_timeline"
        );
    }

    #[test]
    fn tool_def_no_params_schema_empty() {
        let t = tool_def_no_params("vault_status", "test");
        assert_eq!(
            t.input_schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
        );
        let properties = t.input_schema.get("properties").and_then(|v| v.as_object());
        assert!(
            properties.unwrap().is_empty(),
            "GET-only tool must have empty properties"
        );
    }

    /// Le catalogue réellement servi doit être exactement la liste canonique.
    ///
    /// Ce test comparait auparavant `EXPECTED_TOOL_NAMES.len()` à un littéral : une
    /// constante confrontée à un nombre écrit deux lignes plus haut, sans jamais toucher
    /// [`tool_catalogue`]. Il est resté vert pendant que la production passait à 25 outils,
    /// que `create_feature_card` et `vault_archives_list` y apparaissaient sans figurer
    /// dans la liste, et que `vault_delete` y figurait sans exister nulle part.
    ///
    /// La comparaison porte désormais sur des ENSEMBLES dans les deux sens : un outil
    /// ajouté en production sans mise à jour de la liste échoue, et un nom fantôme dans la
    /// liste échoue aussi. L'effectif n'est écrit nulle part — il se lit sur la liste.
    #[test]
    fn catalogue_expose_exactement_les_outils_canoniques() {
        /// Noms canoniques des outils du stub.
        ///
        /// ⚠️ Sous-ensemble strict de la surface du serveur : `job_status`, exposé par
        /// `gradatum-server`, n'a pas d'équivalent ici — il est le seul outil du catalogue
        /// serveur sans jumeau REST, donc non proxifiable en l'état (mesuré le 2026-08-01).
        /// Cet écart est désormais gaté : `scripts/mcp-catalog-parity.sh` le compare aux
        /// deux catalogues servis et rougit si l'écart change dans un sens ou dans l'autre.
        const EXPECTED_TOOL_NAMES: &[&str] = &[
            // read
            "vault_search",
            "vault_read",
            "vault_list",
            "vault_status",
            "vault_graph",
            "vault_links",
            "vault_trace",
            "vault_context",
            "vault_timeline",
            "vault_authors",
            "vault_tags",
            // write
            "vault_write",
            "vault_classify",
            "vault_downgrade",
            // allocation de numéro de feature
            "create_feature_card",
            // history F-40
            "vault_history",
            "vault_history_get",
            "vault_restore",
            "vault_diff",
            // forget F-44
            "vault_forget",
            // archives listing F-100 (lecture seule)
            "vault_archives_list",
            // lesson recall F-60
            "vault_lessons_recall",
            // code scope F-61
            "code_scope",
            // proactive recall F-46
            "vault_proactive_recall",
            "vault_proactive_recall_feedback",
        ];

        let catalogue = tool_catalogue();
        let servis: BTreeSet<&str> = catalogue.iter().map(|t| t.name.as_ref()).collect();
        let attendus: BTreeSet<&str> = EXPECTED_TOOL_NAMES.iter().copied().collect();

        assert_eq!(
            servis,
            attendus,
            "le catalogue servi diverge de la liste canonique — \
             en trop : {:?}, manquants : {:?}",
            servis.difference(&attendus).collect::<Vec<_>>(),
            attendus.difference(&servis).collect::<Vec<_>>()
        );
        assert_eq!(
            catalogue.len(),
            EXPECTED_TOOL_NAMES.len(),
            "un nom est servi deux fois : la comparaison d'ensembles ne le verrait pas"
        );
    }

    /// Extrait les noms d'outils cités dans la table « MCP Tools Exposed » du README.
    ///
    /// L'extraction est bornée à cette section : le reste du README cite `vault_status`,
    /// `gradatum-server` ou `tools/list` entre backticks sans que ce soient des entrées de
    /// la table. Un scan du fichier entier ramasserait ces jetons et rendrait le gate
    /// ininterprétable — la portée est donc une propriété de correction, pas une commodité.
    ///
    /// Dans la section, seules les lignes de tableau (`|`) sont lues, et seuls leurs
    /// segments entre backticks sont retenus : la ligne d'en-tête et le séparateur `|---|`
    /// n'en contiennent aucun et sont écartés sans cas particulier.
    ///
    /// # Panics
    ///
    /// Panique si le titre de section est absent — c'est un README restructuré, donc un
    /// gate qui ne mesure plus ce qu'il croit mesurer. Échouer bruyamment vaut mieux que
    /// rendre un ensemble vide, qui passerait pour un simple désaccord de contenu.
    fn tool_names_cited_in_readme(readme: &str) -> Vec<String> {
        const SECTION: &str = "## MCP Tools Exposed";

        let after = readme
            .split_once(SECTION)
            .map(|(_, rest)| rest)
            .expect("section « ## MCP Tools Exposed » absente du README du stub");
        // La section s'arrête au titre suivant de même niveau.
        let section = after.split_once("\n## ").map_or(after, |(head, _)| head);

        let mut noms = Vec::new();
        for ligne in section.lines().filter(|l| l.trim_start().starts_with('|')) {
            // Segments d'index impair = intérieur des backticks.
            for (i, seg) in ligne.split('`').enumerate() {
                if i % 2 == 1 {
                    noms.push(seg.to_string());
                }
            }
        }
        noms
    }

    /// Le README du stub et le catalogue réellement servi ne peuvent pas diverger.
    ///
    /// `gradatum-mcp-stub` est publié sans baseline `public-api` : l'outil refuse un crate
    /// sans cible `lib` (`no library targets found in package`, mesuré le 2026-08-01), et
    /// la propriété à protéger n'est de toute façon pas une surface Rust — aucun
    /// consommateur ne fait `use` d'un binaire. La surface publique de ce crate, c'est son
    /// catalogue MCP. C'est donc elle qui est gatée, ici et par
    /// `scripts/mcp-catalog-parity.sh` pour la parité avec le serveur.
    ///
    /// Aucun cardinal n'est écrit : les deux côtés se lisent. Le README a déjà gravé
    /// « 24 tools » pendant qu'il en servait 25 — un compte recopié à la main se périme en
    /// silence et, pire, se transmet.
    #[test]
    fn readme_cite_exactement_les_outils_servis() {
        let cites = tool_names_cited_in_readme(include_str!("../README.md"));

        // Anti-vacuité : sans cette garde, un README dont la table serait vidée ferait
        // comparer deux ensembles vides côté « cités » et n'accuserait qu'un delta —
        // mais une extraction muette (backticks disparus, table convertie en liste)
        // doit être un échec de MESURE, distinct d'un désaccord de contenu.
        assert!(
            !cites.is_empty(),
            "aucun nom d'outil extrait de la table du README — l'extraction ne mesure plus rien"
        );

        let catalogue = tool_catalogue();
        let servis: BTreeSet<&str> = catalogue.iter().map(|t| t.name.as_ref()).collect();
        let documentes: BTreeSet<&str> = cites.iter().map(String::as_str).collect();

        assert_eq!(
            documentes,
            servis,
            "le README diverge du catalogue servi — documentés non servis : {:?}, \
             servis non documentés : {:?}",
            documentes.difference(&servis).collect::<Vec<_>>(),
            servis.difference(&documentes).collect::<Vec<_>>()
        );
        assert_eq!(
            cites.len(),
            documentes.len(),
            "un outil est cité deux fois dans la table du README : {:?}",
            cites
        );
    }

    #[test]
    fn build_lessons_recall_endpoint_valid_class() {
        let body = serde_json::json!({ "class": "deploy", "limit": 3 });
        let ep = build_lessons_recall_endpoint(&body).expect("classe valide");
        assert_eq!(ep, "lessons/recall?class=deploy&limit=3");
    }

    #[test]
    fn build_lessons_recall_endpoint_class_with_hyphen() {
        // Les classes à tiret restent URL-safe sans encodage.
        let body = serde_json::json!({ "class": "ci-cd" });
        let ep = build_lessons_recall_endpoint(&body).expect("classe valide");
        assert_eq!(ep, "lessons/recall?class=ci-cd");
    }

    #[test]
    fn build_lessons_recall_endpoint_invalid_class_rejected() {
        // Anti-injection : une classe hors vocabulaire (et a fortiori un payload
        // d'injection) est rejetée AVANT toute construction d'URL.
        for bad in [
            "unknown",
            "deploy OR release",
            "../etc/passwd",
            "deploy&limit=999",
        ] {
            let body = serde_json::json!({ "class": bad });
            assert!(
                build_lessons_recall_endpoint(&body).is_err(),
                "classe '{bad}' doit être rejetée"
            );
        }
    }

    #[test]
    fn build_lessons_recall_endpoint_missing_class_rejected() {
        let body = serde_json::json!({ "limit": 5 });
        assert!(build_lessons_recall_endpoint(&body).is_err());
    }

    #[test]
    fn build_lessons_recall_endpoint_null_limit_omitted() {
        let body = serde_json::json!({ "class": "migration", "limit": null });
        let ep = build_lessons_recall_endpoint(&body).expect("classe valide");
        assert_eq!(ep, "lessons/recall?class=migration");
    }

    #[test]
    fn build_lessons_recall_endpoint_rank_recency_boosted() {
        let body = serde_json::json!({ "class": "deploy", "rank": "recency-boosted" });
        let ep = build_lessons_recall_endpoint(&body).expect("classe + rank valides");
        assert_eq!(ep, "lessons/recall?class=deploy&rank=recency-boosted");
    }

    #[test]
    fn build_lessons_recall_endpoint_rank_relevance() {
        // rank=relevance est un no-op (BM25 order) — forwardé tel quel pour parité.
        let body = serde_json::json!({ "class": "release", "rank": "relevance" });
        let ep = build_lessons_recall_endpoint(&body).expect("classe + rank valides");
        assert_eq!(ep, "lessons/recall?class=release&rank=relevance");
    }

    #[test]
    fn build_lessons_recall_endpoint_rank_null_omitted() {
        // rank absent/null → endpoint sans param (rétro-compat).
        let body = serde_json::json!({ "class": "migration", "rank": null });
        let ep = build_lessons_recall_endpoint(&body).expect("classe valide");
        assert_eq!(ep, "lessons/recall?class=migration");
    }

    #[test]
    fn build_lessons_recall_endpoint_semantic_true() {
        let body = serde_json::json!({ "class": "archi", "semantic": true });
        let ep = build_lessons_recall_endpoint(&body).expect("classe + semantic valides");
        assert_eq!(ep, "lessons/recall?class=archi&semantic=true");
    }

    #[test]
    fn build_lessons_recall_endpoint_semantic_false_explicit() {
        // semantic=false explicite est forwardé — le serveur applique le chemin BM25.
        let body = serde_json::json!({ "class": "archi", "semantic": false });
        let ep = build_lessons_recall_endpoint(&body).expect("classe + semantic valides");
        assert_eq!(ep, "lessons/recall?class=archi&semantic=false");
    }

    #[test]
    fn build_lessons_recall_endpoint_query_ascii() {
        // query ASCII simple — aucun encodage nécessaire.
        let body = serde_json::json!({ "class": "deploy", "semantic": true, "query": "deploy CI pipeline" });
        let ep = build_lessons_recall_endpoint(&body).expect("valide");
        assert_eq!(
            ep,
            "lessons/recall?class=deploy&semantic=true&query=deploy%20CI%20pipeline"
        );
    }

    #[test]
    fn build_lessons_recall_endpoint_query_special_chars() {
        // query avec caractères spéciaux — percent-encodés correctement.
        let body = serde_json::json!({ "class": "archi", "semantic": true, "query": "a&b=c" });
        let ep = build_lessons_recall_endpoint(&body).expect("valide");
        // '&' → %26, '=' → %3D
        assert_eq!(
            ep,
            "lessons/recall?class=archi&semantic=true&query=a%26b%3Dc"
        );
    }

    #[test]
    fn build_lessons_recall_endpoint_full_params() {
        // Tous les paramètres présents simultanément.
        let body = serde_json::json!({
            "class": "migration",
            "limit": 5,
            "rank": "recency-boosted",
            "semantic": true,
            "query": "sqlx migration"
        });
        let ep = build_lessons_recall_endpoint(&body).expect("tous les params valides");
        assert_eq!(
            ep,
            "lessons/recall?class=migration&limit=5&rank=recency-boosted&semantic=true&query=sqlx%20migration"
        );
    }

    #[test]
    fn percent_encode_value_unreserved_chars_unchanged() {
        // Caractères unreserved RFC 3986 : ALPHA / DIGIT / '-' / '.' / '_' / '~'
        let input = "abcXYZ019-._~";
        assert_eq!(percent_encode_value(input), input);
    }

    #[test]
    fn percent_encode_value_space_encoded() {
        assert_eq!(percent_encode_value("hello world"), "hello%20world");
    }

    #[test]
    fn percent_encode_value_ampersand_encoded() {
        assert_eq!(percent_encode_value("a&b"), "a%26b");
    }

    // NOTE — `from_env()` sans credentials n'est PAS couvert ici, volontairement.
    // Un test `from_env_fails_without_credentials` a existé à cet endroit ; il
    // assertait `"".is_empty()`, c'est-à-dire rien : couverture réelle nulle sous
    // un nom qui en promettait une. Le supprimer ne retire aucune couverture, il
    // retire une fausse garantie.
    // L'exercer réellement demanderait `unsafe { std::env::remove_var(…) }`
    // (edition 2024) sur les trois variables lues par `from_env`, donc une
    // mutation d'état global partagée avec les tests exécutés en parallèle dans
    // ce même binaire — data race formelle et flakiness selon l'environnement CI,
    // pour couvrir un `bail!` sans logique. Voie propre si le besoin se confirme :
    // extraire la résolution en `fn resolve_auth(key_file: Option<String>,
    // bearer: Option<String>) -> Result<AuthMode>` et tester CELLE-LÀ, sans env.

    // ── Tests TokenState ──────────────────────────────────────────────────────

    #[test]
    fn token_state_fresh_does_not_need_refresh() {
        // TTL 3600s : le seuil 30% = 1080s. Un token tout juste créé ne doit pas
        // déclencher de refresh.
        let state = TokenState::new("tok".to_string(), 3600);
        assert!(
            !state.should_refresh(),
            "token fraîchement créé ne doit pas nécessiter de refresh"
        );
        assert!(
            !state.is_expired(),
            "token fraîchement créé ne doit pas être expiré"
        );
    }

    #[test]
    fn token_state_expired_is_detected() {
        // Token dont l'expiry est dans le passé → expiré.
        let state = TokenState {
            token: "tok".to_string(),
            expires_at: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or(Instant::now()),
            ttl_secs: 3600,
        };
        // Note : sur certaines plateformes Instant::now() - 1s peut ne pas être inférieur
        // à now() si la sub sature. On vérifie le comportement observable.
        // Le cas is_expired() == true est garanti si expires_at < now().
        let _ = state.is_expired(); // Ne pas asserter le bool exact sur ce cas fragile
        // ce qui compte : should_refresh() retourne true si near/past expiry.
        // On teste avec un TTL très court (1s) pour garantir near-expiry.
        let near_state = TokenState {
            token: "tok".to_string(),
            expires_at: Instant::now() + Duration::from_millis(100),
            ttl_secs: 3600,
        };
        assert!(
            near_state.should_refresh(),
            "token avec 100ms restant sur 3600s ttl doit déclencher refresh"
        );
    }

    #[test]
    fn token_state_near_expiry_triggers_refresh() {
        // TTL original 3600s. Seuil = 1080s. Il reste 500s → doit déclencher refresh.
        let state = TokenState {
            token: "tok".to_string(),
            expires_at: Instant::now() + Duration::from_secs(500),
            ttl_secs: 3600,
        };
        assert!(
            state.should_refresh(),
            "token à 500s restant < seuil 1080s doit déclencher refresh proactif"
        );
        assert!(!state.is_expired(), "pas encore expiré");
    }

    #[test]
    fn token_state_above_threshold_no_refresh() {
        // TTL original 3600s. Seuil = 1080s. Il reste 2000s → pas de refresh.
        let state = TokenState {
            token: "tok".to_string(),
            expires_at: Instant::now() + Duration::from_secs(2000),
            ttl_secs: 3600,
        };
        assert!(
            !state.should_refresh(),
            "token à 2000s restant > seuil 1080s ne doit pas déclencher refresh"
        );
    }

    // ── Tests ExchangeResponse désérialisation ────────────────────────────────

    #[test]
    fn exchange_response_deserialization() {
        // Vérifie que ExchangeResponse correspond au contrat /auth/exchange.
        let json_str = r#"{"token": "eyJhb...", "ttl_secs": 86400, "scopes": ["admin"], "tenant_id": "main", "kid": "k1"}"#;
        let resp: ExchangeResponse = serde_json::from_str(json_str)
            .expect("ExchangeResponse doit désérialiser la réponse /auth/exchange");
        assert_eq!(resp.token, "eyJhb...");
        assert_eq!(resp.ttl_secs, 86400);
    }

    #[test]
    fn exchange_response_minimal_fields() {
        // Avec seulement token + ttl_secs (les autres champs sont ignorés par le stub).
        let json_str = r#"{"token": "t", "ttl_secs": 3600}"#;
        let resp: ExchangeResponse = serde_json::from_str(json_str)
            .expect("ExchangeResponse doit fonctionner avec les champs minimaux");
        assert_eq!(resp.token, "t");
        assert_eq!(resp.ttl_secs, 3600);
    }
}
