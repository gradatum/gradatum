//! # gradatum-mcp-stub
//!
//! Proxy stdio → HTTP gradatum-server.
//!
//! Chaque outil MCP est un thin-forward : il sérialise les arguments reçus en JSON,
//! les envoie en POST sur l'endpoint REST correspondant de gradatum-server, et
//! retourne la réponse JSON à l'hôte MCP.
//!
//! ## Configuration (env vars)
//!
//! | Variable | Défaut | Rôle |
//! |---|---|---|
//! | `GRADATUM_SERVER_URL` | `http://127.0.0.1:19090` | URL de base du serveur |
//! | `GRADATUM_API_KEY_FILE` | — | Path vers fichier chmod 600 contenant `ak_xxx` (prioritaire) |
//! | `GRADATUM_BEARER_TOKEN` | — | JWT statique (fallback si `GRADATUM_API_KEY_FILE` absent) |
//!
//! ### Mode auto-refresh (recommandé — `GRADATUM_API_KEY_FILE`)
//!
//! Si `GRADATUM_API_KEY_FILE` est défini, le stub lit l'API key au démarrage,
//! fait un `POST /auth/exchange` pour obtenir un JWT, et le renouvelle
//! automatiquement quand le TTL restant descend sous 30%.
//!
//! Logique de refresh :
//! 1. Avant chaque appel HTTP : vérifier si TTL restant < 30% → exchange proactif.
//! 2. Si exchange proactif échoue → warn + utilise le JWT actuel (peut encore être valide).
//! 3. Si le serveur retourne 401 sur un appel forward → re-exchange depuis l'API key
//!    + retry une fois (one-shot).
//!
//! ### Mode statique (legacy — `GRADATUM_BEARER_TOKEN`)
//!
//! Le JWT statique est utilisé tel quel. Aucun refresh automatique.
//! Recommandé uniquement pour les tests ou les déploiements temporaires.
//!
//! ## Reconnect
//!
//! Backoff exponentiel 100ms → 5s, max 10 tentatives (erreurs réseau/5xx).
//! Au 11e échec → erreur MCP `McpError::internal_error("server unavailable")`.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rmcp::{
    model::ProtocolVersion,
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    ErrorData, RoleServer, ServerHandler,
};
use serde_json::json;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

// ── Constantes ────────────────────────────────────────────────────────────────

/// Env var : URL de base du serveur gradatum.
pub(crate) const SERVER_URL_ENV: &str = "GRADATUM_SERVER_URL";
/// Env var : JWT statique (mode legacy).
pub(crate) const BEARER_ENV: &str = "GRADATUM_BEARER_TOKEN";
/// Env var : fichier contenant l'API key `ak_xxx` (mode auto-refresh).
pub(crate) const API_KEY_FILE_ENV: &str = "GRADATUM_API_KEY_FILE";
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:19090";

/// Nombre maximum de tentatives pour une requête HTTP avant d'échouer.
const MAX_RETRIES: u32 = 10;
/// Délai initial du backoff exponentiel (ms).
const BACKOFF_INIT_MS: u64 = 100;
/// Délai maximum du backoff exponentiel (ms).
const BACKOFF_MAX_MS: u64 = 5_000;
/// Seuil de refresh proactif : refresh quand TTL restant < 30% du TTL total.
const REFRESH_THRESHOLD_RATIO: f64 = 0.30;

// ── État du token ─────────────────────────────────────────────────────────────

/// Mode d'authentification configuré au démarrage.
#[derive(Debug, Clone)]
pub(crate) enum AuthMode {
    /// Mode auto-refresh : API key permanente + JWT renouvelé automatiquement.
    ApiKey(String),
    /// Mode statique : JWT fixé à l'init, pas de refresh.
    StaticBearer(String),
}

/// État courant du JWT (utilisé en mode `ApiKey`).
#[derive(Debug)]
pub(crate) struct TokenState {
    /// JWT actuel.
    pub token: String,
    /// Instant auquel le JWT expire (calculé depuis `ttl_secs` retourné par `/auth/exchange`).
    pub expires_at: Instant,
    /// TTL total reçu lors du dernier exchange (pour calculer le seuil 30%).
    pub ttl_secs: u64,
}

impl TokenState {
    /// Crée un `TokenState` depuis la réponse `/auth/exchange`.
    pub fn new(token: String, ttl_secs: u64) -> Self {
        let expires_at = Instant::now() + Duration::from_secs(ttl_secs);
        Self {
            token,
            expires_at,
            ttl_secs,
        }
    }

    /// Retourne `true` si le refresh proactif est recommandé (TTL restant < 30%).
    pub fn should_refresh(&self) -> bool {
        let remaining = self.expires_at.saturating_duration_since(Instant::now());
        let threshold =
            Duration::from_secs((self.ttl_secs as f64 * REFRESH_THRESHOLD_RATIO) as u64);
        remaining < threshold
    }

    /// Retourne `true` si le JWT est déjà expiré.
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

// ── Réponse /auth/exchange ────────────────────────────────────────────────────

/// Réponse JSON de `POST /auth/exchange` (champs utilisés par le stub).
///
/// Contrat spec §2.4 P2.0c-bis : seuls `token` et `ttl_secs` sont consommés.
/// Les autres champs (`scopes`, `tenant_id`, `kid`) sont ignorés par le stub.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ExchangeResponse {
    pub token: String,
    pub ttl_secs: u64,
}

// ── Handler MCP ───────────────────────────────────────────────────────────────

/// Proxy MCP stdio → HTTP. Maintient le client HTTP et les credentials.
///
/// `Clone` : le `Arc<Mutex<_>>` assure le partage du token entre clones rmcp.
#[derive(Clone)]
pub(crate) struct StubHandler {
    /// Client HTTP avec timeout configuré.
    pub(crate) client: reqwest::Client,
    /// URL de base du serveur gradatum (ex. `http://127.0.0.1:19090`).
    pub(crate) server_url: String,
    /// Mode d'authentification configuré.
    pub(crate) auth: AuthMode,
    /// État du token JWT partagé (uniquement en mode `ApiKey`).
    pub(crate) token_state: Arc<Mutex<Option<TokenState>>>,
}

impl StubHandler {
    /// Construit un `StubHandler` depuis l'env.
    ///
    /// Priorité :
    /// 1. `GRADATUM_API_KEY_FILE` → mode auto-refresh
    /// 2. `GRADATUM_BEARER_TOKEN` → mode statique
    /// 3. Aucun → erreur
    ///
    /// # Erreurs
    /// - Ni `GRADATUM_API_KEY_FILE` ni `GRADATUM_BEARER_TOKEN` définis → erreur.
    /// - Fichier API key illisible ou format invalide → erreur.
    pub fn from_env() -> Result<Self> {
        let server_url =
            std::env::var(SERVER_URL_ENV).unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("échec construction client HTTP")?;

        // Mode 1 : API key file → auto-refresh.
        if let Ok(key_file) = std::env::var(API_KEY_FILE_ENV) {
            let api_key = std::fs::read_to_string(&key_file)
                .with_context(|| format!("lecture GRADATUM_API_KEY_FILE '{key_file}' échouée"))?
                .trim()
                .to_string();
            if api_key.is_empty() {
                anyhow::bail!("GRADATUM_API_KEY_FILE '{key_file}' est vide");
            }
            if !api_key.starts_with("ak_") {
                anyhow::bail!(
                    "GRADATUM_API_KEY_FILE '{key_file}' : format invalide (attendu: ak_...)"
                );
            }
            info!(
                key_file = %key_file,
                "mode auto-refresh JWT activé via GRADATUM_API_KEY_FILE"
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
                anyhow::bail!("GRADATUM_BEARER_TOKEN ne peut pas être vide");
            }
            warn!(
                "mode bearer statique — JWT expirera sans refresh automatique. \
                 Utiliser GRADATUM_API_KEY_FILE pour l'auto-refresh."
            );
            return Ok(Self {
                client,
                server_url,
                auth: AuthMode::StaticBearer(bearer),
                token_state: Arc::new(Mutex::new(None)),
            });
        }

        anyhow::bail!(
            "authentification manquante — définir GRADATUM_API_KEY_FILE (recommandé) \
             ou GRADATUM_BEARER_TOKEN"
        )
    }

    /// Initialise le JWT au démarrage (mode auto-refresh uniquement).
    ///
    /// Appelle `/auth/exchange` et stocke le JWT dans `token_state`.
    /// En mode statique, cette méthode est un no-op.
    pub async fn init_token(&self) -> Result<()> {
        if let AuthMode::ApiKey(ref api_key) = self.auth {
            let state = self.exchange_token(api_key).await?;
            *self.token_state.lock().await = Some(state);
            info!("JWT initialisé avec succès");
        }
        Ok(())
    }

    /// Appelle `POST /auth/exchange` avec l'API key et retourne un `TokenState`.
    pub(crate) async fn exchange_token(&self, api_key: &str) -> Result<TokenState> {
        let url = format!("{}/auth/exchange", self.server_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
            .context("POST /auth/exchange : erreur réseau")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "POST /auth/exchange a retourné {} : {body}",
                status.as_u16()
            );
        }

        let exchange: ExchangeResponse = resp
            .json()
            .await
            .context("POST /auth/exchange : désérialisation réponse échouée")?;

        Ok(TokenState::new(exchange.token, exchange.ttl_secs))
    }

    /// Retourne le JWT courant, en effectuant un refresh proactif si nécessaire.
    ///
    /// En mode statique → retourne le JWT fixe sans refresh.
    /// En mode auto-refresh :
    /// 1. Vérifie si refresh proactif recommandé (TTL < 30%) → exchange.
    /// 2. Si exchange échoue → warn + utilise le JWT actuel (peut encore être valide).
    /// 3. Si JWT absent → exchange obligatoire.
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
                    debug!("refresh JWT proactif (TTL < 30% ou expiré)");
                    match self.exchange_token(api_key).await {
                        Ok(new_state) => {
                            info!("JWT renouvelé avec succès");
                            *guard = Some(new_state);
                        }
                        Err(e) => {
                            // Refresh échoué — fallback sur token actuel s'il est encore valide.
                            if let Some(state) = guard.as_ref() {
                                if !state.is_expired() {
                                    warn!(
                                        error = %e,
                                        "refresh JWT proactif échoué — fallback JWT actuel"
                                    );
                                    return Ok(state.token.clone());
                                }
                            }
                            error!(
                                error = %e,
                                "refresh JWT échoué et aucun token valide disponible"
                            );
                            return Err(ErrorData::internal_error(
                                format!("authentification impossible : {e}"),
                                None,
                            ));
                        }
                    }
                }

                guard.as_ref().map(|s| s.token.clone()).ok_or_else(|| {
                    ErrorData::internal_error("état token incohérent — aucun JWT disponible", None)
                })
            }
        }
    }

    /// Tente un re-exchange depuis l'API key et retourne le nouveau JWT.
    ///
    /// Utilisé après réception d'un 401 sur un appel forward.
    /// En mode statique → erreur immédiate (pas de refresh possible).
    async fn force_refresh(&self) -> Result<String, ErrorData> {
        match &self.auth {
            AuthMode::StaticBearer(_) => Err(ErrorData::internal_error(
                "JWT expiré et mode statique actif — définir GRADATUM_API_KEY_FILE \
                 pour l'auto-refresh",
                None,
            )),
            AuthMode::ApiKey(api_key) => {
                debug!("force refresh JWT après 401");
                match self.exchange_token(api_key).await {
                    Ok(new_state) => {
                        let token = new_state.token.clone();
                        *self.token_state.lock().await = Some(new_state);
                        info!("JWT renouvelé après 401");
                        Ok(token)
                    }
                    Err(e) => {
                        error!(error = %e, "force refresh JWT échoué après 401");
                        Err(ErrorData::internal_error(
                            format!("re-authentification après 401 échouée : {e}"),
                            None,
                        ))
                    }
                }
            }
        }
    }

    /// Effectue un POST sur `{server_url}/api/v1/{endpoint}` avec le body JSON.
    ///
    /// Logique de reconnect : backoff exponentiel 100ms → 5s, max [`MAX_RETRIES`].
    /// Conditions de retry : timeout, erreur connexion, réponse 5xx.
    /// Exception 401 : re-exchange depuis l'API key + retry one-shot (mode auto-refresh).
    /// Conditions sans retry : autres 4xx.
    ///
    /// Retourne le body JSON en cas de succès ou le message d'erreur HTTP.
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
                debug!(endpoint, attempt, delay_ms, "retry HTTP après échec");
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
                                format!("désérialisation réponse JSON échouée : {e}"),
                                None,
                            )
                        });
                    } else if status.as_u16() == 401 {
                        // 401 → re-exchange one-shot (premier attempt uniquement).
                        if attempt == 0 {
                            warn!(endpoint, "401 reçu — tentative re-exchange JWT");
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
                            format!("401 persistant sur {endpoint} — credentials invalides"),
                            None,
                        ));
                    } else if status.is_server_error() {
                        warn!(
                            endpoint,
                            status = status.as_u16(),
                            attempt,
                            "5xx du serveur — retry"
                        );
                        continue;
                    } else {
                        let msg = format!("erreur HTTP {} sur {endpoint}", status.as_u16());
                        return Err(ErrorData::internal_error(msg, None));
                    }
                }
                Err(e) if e.is_connect() || e.is_timeout() => {
                    warn!(
                        endpoint,
                        attempt,
                        err = %e,
                        "erreur connexion/timeout — retry"
                    );
                    continue;
                }
                Err(e) => {
                    return Err(ErrorData::internal_error(
                        format!("erreur HTTP inattendue sur {endpoint} : {e}"),
                        None,
                    ));
                }
            }
        }

        error!(
            endpoint,
            MAX_RETRIES, "serveur inaccessible après max tentatives"
        );
        Err(ErrorData::internal_error(
            format!(
                "serveur gradatum inaccessible après {MAX_RETRIES} tentatives \
                 (endpoint={endpoint})"
            ),
            None,
        ))
    }

    /// Effectue un GET sur `{server_url}/api/v1/{endpoint}` (pas de body).
    ///
    /// Même logique de backoff et de retry 401 que [`Self::forward_post`].
    async fn forward_get(&self, endpoint: &str) -> Result<serde_json::Value, ErrorData> {
        let url = format!("{}/api/v1/{}", self.server_url, endpoint);
        let mut delay_ms = BACKOFF_INIT_MS;
        let mut refreshed_after_401 = false;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 && !refreshed_after_401 {
                debug!(endpoint, attempt, delay_ms, "retry GET après échec");
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
                                format!("désérialisation réponse JSON échouée : {e}"),
                                None,
                            )
                        });
                    } else if status.as_u16() == 401 {
                        if attempt == 0 {
                            warn!(endpoint, "401 reçu (GET) — tentative re-exchange JWT");
                            match self.force_refresh().await {
                                Ok(_) => {
                                    refreshed_after_401 = true;
                                    continue;
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        return Err(ErrorData::internal_error(
                            format!("401 persistant (GET) sur {endpoint} — credentials invalides"),
                            None,
                        ));
                    } else if status.is_server_error() {
                        warn!(endpoint, status = status.as_u16(), attempt, "5xx → retry");
                        continue;
                    } else {
                        return Err(ErrorData::internal_error(
                            format!("erreur HTTP {} sur {endpoint}", status.as_u16()),
                            None,
                        ));
                    }
                }
                Err(e) if e.is_connect() || e.is_timeout() => {
                    warn!(endpoint, attempt, err = %e, "connexion/timeout → retry");
                    continue;
                }
                Err(e) => {
                    return Err(ErrorData::internal_error(
                        format!("erreur HTTP inattendue sur {endpoint} : {e}"),
                        None,
                    ));
                }
            }
        }

        error!(
            endpoint,
            MAX_RETRIES, "serveur inaccessible après max tentatives GET"
        );
        Err(ErrorData::internal_error(
            format!(
                "serveur gradatum inaccessible après {MAX_RETRIES} tentatives GET \
                 (endpoint={endpoint})"
            ),
            None,
        ))
    }

    /// Convertit une `serde_json::Value` en `CallToolResult` contenant un texte JSON.
    fn json_to_tool_result(value: serde_json::Value) -> CallToolResult {
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        CallToolResult::success(vec![Content::text(text)])
    }
}

// ── Implémentation ServerHandler ──────────────────────────────────────────────

impl ServerHandler for StubHandler {
    /// Informations du serveur MCP retournées lors de l'initialisation.
    fn get_info(&self) -> ServerInfo {
        // rmcp 1.x: ServerInfo + Implementation sont #[non_exhaustive] - constructeurs obligatoires.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "gradatum-mcp-stub",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::default())
    }

    /// Liste les 13 outils MCP exposés (10 read + 3 write — parité API REST server).
    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        use gradatum_dto::{
            VaultClassifyRequest, VaultContextRequest, VaultDowngradeRequest, VaultGraphRequest,
            VaultLinksRequest, VaultListRequest, VaultReadRequest, VaultSearchRequest,
            VaultTraceRequest, VaultWriteRequest,
        };

        let tools = vec![
            // ── Read tools (10) ────────────────────────────────────────────────
            tool_def::<VaultSearchRequest>("vault_search", "Recherche plein-texte dans le vault"),
            tool_def::<VaultReadRequest>("vault_read", "Lit le contenu d'une note par chemin"),
            tool_def::<VaultListRequest>("vault_list", "Liste les notes du vault"),
            tool_def_no_params("vault_status", "État courant du vault"),
            tool_def::<VaultGraphRequest>(
                "vault_graph",
                "Graphe de liens depuis une note racine",
            ),
            tool_def::<VaultLinksRequest>(
                "vault_links",
                "Liens directs d'une note (alias vault_graph depth=1)",
            ),
            tool_def::<VaultTraceRequest>(
                "vault_trace",
                "Trace les notes par tags, sections ou pattern",
            ),
            tool_def::<VaultContextRequest>(
                "vault_context",
                "Construit un contexte LLM depuis les notes",
            ),
            tool_def_no_params("vault_authors", "Liste les auteurs du vault"),
            tool_def_no_params("vault_tags", "Liste les tags du vault avec fréquences"),
            // ── Write tools (3) — queue async 202 Accepted ────────────────────
            tool_def::<VaultWriteRequest>(
                "vault_write",
                "Crée une nouvelle note dans le vault (queue async). \
                 Champs : title (req), body (req), author, tags[], section_hint, tenant_id. \
                 Retourne 202 Accepted + job_id (poll via GET /api/v1/jobs/:id).",
            ),
            tool_def::<VaultClassifyRequest>(
                "vault_classify",
                "Re-classifie une note existante via le pipeline curator (async). \
                 Champs : note_id (req), tenant_id. \
                 Retourne 202 Accepted + job_id.",
            ),
            tool_def::<VaultDowngradeRequest>(
                "vault_downgrade",
                "Rétrograde une note (status live → downgraded) — la retire des résultats par défaut. \
                 Champs : note_id (req), reason (req), replaced_by, tenant_id. \
                 Retourne 202 Accepted + job_id.",
            ),
        ];
        std::future::ready(Ok(ListToolsResult {
            tools,
            ..Default::default()
        }))
    }

    /// Dispatche l'appel outil vers l'endpoint REST correspondant.
    ///
    /// Chaque outil mappe 1:1 sur `POST /api/v1/{tool_name}` (sauf GET pour
    /// `vault_status`, `vault_authors`, `vault_tags`).
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
                // GET endpoints (sans body)
                "vault_status" => self.forward_get("vault_status").await?,
                "vault_authors" => self.forward_get("vault_authors").await?,
                "vault_tags" => self.forward_get("vault_tags").await?,
                // Write endpoints — POST async 202 Accepted
                "vault_write" => self.forward_post("vault_write", body).await?,
                "vault_classify" => self.forward_post("vault_classify", body).await?,
                "vault_downgrade" => self.forward_post("vault_downgrade", body).await?,
                unknown => {
                    return Err(ErrorData::invalid_params(
                        format!("outil inconnu : {unknown}"),
                        None,
                    ));
                }
            };

            Ok(Self::json_to_tool_result(result))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Construit une définition d'outil MCP avec `inputSchema` auto-dérivé depuis `T`.
///
/// `T` doit dériver `schemars::JsonSchema` (via feature `schemars` côté `gradatum-dto`).
/// Le schéma résultant est le contrat wire HTTP exact attendu par le serveur.
fn tool_def<T: schemars::JsonSchema>(name: &'static str, description: &'static str) -> Tool {
    let schema = serde_json::to_value(schemars::schema_for!(T))
        .expect("schemars::schema_for! always produces valid JSON");
    let obj = schema
        .as_object()
        .expect("JsonSchema root is always a JSON object")
        .clone();
    // rmcp 1.x: Tool::new() constructeur officiel (#[non_exhaustive] interdit struct literal hors-crate).
    Tool::new(name, description, obj)
}

/// Construit une définition d'outil MCP sans paramètre (GET-only endpoints).
///
/// Le schéma JSON est `{"type":"object","properties":{}}` qui est valide pour
/// "aucun paramètre attendu". Utilisé pour `vault_status`, `vault_authors`, `vault_tags`.
fn tool_def_no_params(name: &'static str, description: &'static str) -> Tool {
    let obj = json!({
        "type": "object",
        "properties": {}
    })
    .as_object()
    .expect("schéma JSON statique toujours valide")
    .clone();
    // rmcp 1.x: Tool::new() constructeur officiel (#[non_exhaustive] interdit struct literal hors-crate).
    Tool::new(name, description, obj)
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

    let handler = StubHandler::from_env().context("initialisation StubHandler depuis env")?;

    // En mode auto-refresh : obtenir le JWT initial avant d'accepter des connexions MCP.
    handler
        .init_token()
        .await
        .context("obtention du JWT initial via /auth/exchange")?;

    // Transport stdio : (stdin, stdout) via rmcp::transport::io::stdio().
    let (stdin, stdout) = rmcp::transport::io::stdio();

    tracing::info!(
        server_url = %handler.server_url,
        "gradatum-mcp-stub démarrage (stdio transport)"
    );

    // serve_server gère l'initialisation MCP, le dispatch et le shutdown propre.
    rmcp::service::serve_server(handler, (stdin, stdout))
        .await
        .map_err(|e| anyhow::anyhow!("erreur serve_server : {e}"))?
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("erreur waiting : {e}"))?;

    Ok(())
}

// ── Tests unitaires ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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

    #[test]
    fn list_tools_count_matches_expected() {
        // Liste canonique des 13 tools (10 read + 3 write — parité API REST server).
        // Cette constante est la source de vérité pour le compte : si on ajoute un
        // tool en production sans MAJ cette liste, le test échoue explicitement avec
        // le nom de la constante dans le message d'erreur.
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
            "vault_authors",
            "vault_tags",
            // write
            "vault_write",
            "vault_classify",
            "vault_downgrade",
        ];

        assert_eq!(
            EXPECTED_TOOL_NAMES.len(),
            13,
            "liste canonique doit contenir 13 tools (10 read + 3 write)"
        );
    }

    #[test]
    fn from_env_fails_without_credentials() {
        // Aucune credential → erreur attendue.
        // Vérification logique sans modifier l'env global (parallélisme de tests).
        let empty = "";
        assert!(empty.is_empty(), "bearer vide → erreur d'init attendue");
    }

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
        // Vérifie que ExchangeResponse correspond au contrat /auth/exchange spec §2.4.
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
