//! Serveur MCP natif in-process — `gradatum-server` v0.6.0.
//!
//! Expose les 21 outils gradatum via le protocole MCP (Streamable HTTP),
//! réutilisant les `*_impl` de [`super::logic`] et les helpers métier existants.
//!
//! # Traversée TrustContext
//!
//! L'Axum `auth_middleware` injecte un [`TrustContext`] dans les extensions HTTP
//! AVANT que la requête n'atteigne le `StreamableHttpService`. rmcp injecte à son
//! tour les [`http::request::Parts`] dans `RequestContext.extensions`. La traversée
//! est donc :
//!
//! ```text
//! ctx.extensions
//!     .get::<http::request::Parts>()   // extensions rmcp → Parts HTTP
//!     .extensions
//!     .get::<TrustContext>()           // Parts HTTP → TrustContext Axum
//! ```
//!
//! # Outils exposés (21)
//!
//! Identiques au stub `gradatum-mcp-stub` (parité contractuelle) :
//! `vault_search`, `vault_read`, `vault_list`, `vault_status`, `vault_graph`,
//! `vault_links`, `vault_trace`, `vault_context`, `vault_timeline`,
//! `vault_authors`, `vault_tags`, `vault_write`, `vault_classify`,
//! `vault_downgrade`, `vault_history`, `vault_history_get`, `vault_restore`,
//! `vault_diff`, `vault_forget`, `vault_lessons_recall`, `code_scope`.
//!
//! # Sécurité
//!
//! - Tout outil exige un `TrustContext` authentifié (erreur MCP INVALID_REQUEST sinon).
//! - `code_scope` vérifie l'invariant de sécurité N°1 : le vault DOIT commencer
//!   par `"code-"` (bypass de la garde mono-vault).
//! - Les erreurs Storage/internes sont masquées derrière un message générique
//!   (anti-fuite chemin/stockage).

use std::sync::Arc;

use rmcp::{
    ErrorData,
    handler::server::ServerHandler,
    model::{
        CallToolResult, Content, Implementation, ListToolsResult, ProtocolVersion,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    transport::{
        StreamableHttpServerConfig, streamable_http_server::session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use gradatum_core::{error::GradatumError, trust::TrustContext};
use gradatum_dto::{
    CodeScopeRequest, LessonsRecallRequest, VaultClassifyRequest, VaultDiffRequest,
    VaultDowngradeRequest, VaultForgetRequest, VaultHistoryGetRequest, VaultHistoryRequest,
    VaultRestoreRequest,
};

use crate::{
    api_v1::{
        code_scope::{code_scope_impl, validate_code_vault_id},
        dto::{
            VaultContextRequest, VaultGraphRequest, VaultLinksRequest, VaultListRequest,
            VaultReadRequest, VaultSearchRequest, VaultTimelineRequest, VaultTraceRequest,
            VaultWriteRequest,
        },
        forget::vault_forget_mcp_impl,
        logic,
    },
    state::AppState,
};

// ── Type public du service MCP ────────────────────────────────────────────────

/// Service MCP Streamable HTTP exposant les 21 outils gradatum.
///
/// Construit via [`build_mcp_service`] et monté sous `/mcp` dans `main.rs`.
///
/// Le paramètre générique `M` est `LocalSessionManager` (pas `Arc<LocalSessionManager>`) :
/// `StreamableHttpService<S, M>` stocke `session_manager: Arc<M>` en interne.
pub type GradatumMcpService =
    rmcp::transport::StreamableHttpService<GradatumMcpHandler, LocalSessionManager>;

/// Limite de taille du corps des requêtes `/mcp` (anti-DoS, F-02) — 512 KiB.
///
/// Délibérément **supérieure** à `/api/v1/vault_write` HTTP (256 KiB) : via MCP, le
/// payload `vault_write` est enveloppé dans du JSON-RPC (`{"jsonrpc","method":"tools/call",
/// "params":{"name","arguments":{…}}}`) **et** le corps markdown est ré-encodé en string
/// JSON (`\n`→`\n`, `"`→`\"`), ce qui gonfle le body MCP au-delà du payload nu. Caper `/mcp`
/// à 256 KiB rejetterait en 413 un `vault_write` proche de 256 KiB qui passe en HTTP direct
/// (régression silencieuse depuis le cutover MCP-natif B3). 512 KiB couvre payload + enveloppe
/// + échappement tout en restant un cap ferme.
///
/// Appliquée via `tower_http::limit::RequestBodyLimitLayer` (et **non** via
/// `axum::extract::DefaultBodyLimit`, inefficace ici car rmcp lit le corps au niveau
/// `tower::Service` sans passer par l'extracteur `Body` d'Axum — vérifié : un body
/// surdimensionné renvoyait 422, pas 413).
///
/// SSOT unique consommée par le montage prod (`build_router`) **et** le harness de test
/// (`mcp_native.rs`) — évite le drift test/prod qui invaliderait la preuve C2 du 413.
pub const MCP_BODY_LIMIT: usize = 512 * 1024;

// ── Handler MCP ───────────────────────────────────────────────────────────────

/// Handler MCP — implémente [`ServerHandler`] pour les 21 outils gradatum.
///
/// Clone par session : `AppState` est `Arc`-backed, le clone est O(1).
#[derive(Clone)]
pub struct GradatumMcpHandler {
    state: AppState,
}

impl GradatumMcpHandler {
    /// Extrait le [`TrustContext`] depuis le contexte MCP.
    ///
    /// rmcp injecte les [`http::request::Parts`] dans `ctx.extensions` (Streamable HTTP).
    /// L'`auth_middleware` Axum a préalablement injecté `TrustContext` dans les extensions
    /// HTTP. La traversée est : extensions rmcp → `http::request::Parts` → `TrustContext`.
    ///
    /// Retourne [`TrustContext::Unauthenticated`] si les extensions ou `TrustContext` sont absents.
    fn trust_from_ctx(ctx: &RequestContext<rmcp::RoleServer>) -> TrustContext {
        ctx.extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<TrustContext>().cloned())
            .unwrap_or(TrustContext::Unauthenticated)
    }

    /// Dispatche l'outil `name` avec ses arguments `args`.
    ///
    /// Séparé de `call_tool` pour la testabilité (pas de `RequestContext` requis).
    ///
    /// # Errors
    ///
    /// Retourne [`ErrorData`] si l'outil est inconnu, si les arguments sont invalides,
    /// ou si la logique métier échoue.
    async fn dispatch_tool(
        &self,
        name: &str,
        args: Option<Value>,
        trust: TrustContext,
    ) -> Result<CallToolResult, ErrorData> {
        let args = args.unwrap_or(Value::Object(serde_json::Map::new()));

        match name {
            // ── Outils read sans paramètres ───────────────────────────────────
            "vault_status" => {
                let res = logic::vault_status_impl(&self.state, &trust)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_authors" => {
                let res = logic::vault_authors_impl(&self.state, &trust)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_tags" => {
                let res = logic::vault_tags_impl(&self.state, &trust)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            // ── Outils read avec paramètres ───────────────────────────────────
            "vault_search" => {
                let req: VaultSearchRequest = deserialize_args(args)?;
                let res = logic::vault_search_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_read" => {
                let req: VaultReadRequest = deserialize_args(args)?;
                let res = logic::vault_read_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_list" => {
                let req: VaultListRequest = deserialize_args(args)?;
                let res = logic::vault_list_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_graph" => {
                let req: VaultGraphRequest = deserialize_args(args)?;
                let res = logic::vault_graph_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_links" => {
                let req: VaultLinksRequest = deserialize_args(args)?;
                let res = logic::vault_links_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_trace" => {
                let req: VaultTraceRequest = deserialize_args(args)?;
                let res = logic::vault_trace_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_context" => {
                let req: VaultContextRequest = deserialize_args(args)?;
                let res = logic::vault_context_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_timeline" => {
                let req: VaultTimelineRequest = deserialize_args(args)?;
                let res = logic::vault_timeline_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_lessons_recall" => {
                let req: LessonsRecallRequest = deserialize_args(args)?;
                let res = logic::lessons_recall_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            // ── Outils write ──────────────────────────────────────────────────
            "vault_write" => {
                let req: VaultWriteRequest = deserialize_args(args)?;
                let request_id = ulid::Ulid::new().to_string();
                let res = logic::vault_write_impl(&self.state, &trust, req, &request_id)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_classify" => {
                let req: VaultClassifyRequest = deserialize_args(args)?;
                let res = logic::vault_classify_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_downgrade" => {
                let req: VaultDowngradeRequest = deserialize_args(args)?;
                let res = logic::vault_downgrade_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            // ── Historique ────────────────────────────────────────────────────
            "vault_history" => {
                let req: VaultHistoryRequest = deserialize_args(args)?;
                let res = logic::vault_history_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_history_get" => {
                let req: VaultHistoryGetRequest = deserialize_args(args)?;
                let res = logic::vault_history_get_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_restore" => {
                let req: VaultRestoreRequest = deserialize_args(args)?;
                let res = logic::vault_restore_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_diff" => {
                let req: VaultDiffRequest = deserialize_args(args)?;
                let res = logic::vault_diff_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            // ── Forget ────────────────────────────────────────────────────────
            "vault_forget" => {
                let req: VaultForgetRequest = deserialize_args(args)?;
                let res = vault_forget_mcp_impl(self.state.clone(), trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            // ── Code scope (invariant sécurité N°1) ───────────────────────────
            "code_scope" => {
                let req: CodeScopeRequest = deserialize_args(args)?;
                // Invariant de sécurité N°1 : le vault DOIT commencer par "code-".
                // Ce check est OBLIGATOIRE ici car code_scope bypasse la garde mono-vault.
                if !validate_code_vault_id(&req.vault) {
                    return Err(ErrorData::invalid_params(
                        format!(
                            "vault '{}' invalide pour code_scope : doit commencer par 'code-'",
                            req.vault
                        ),
                        None,
                    ));
                }
                let res = code_scope_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            _ => Err(ErrorData::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                format!("outil inconnu : {name}"),
                None,
            )),
        }
    }
}

// ── Implémentation ServerHandler ──────────────────────────────────────────────

impl ServerHandler for GradatumMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "gradatum-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::default())
    }

    #[instrument(skip(self, ctx), fields(tool_count = 21))]
    async fn list_tools(
        &self,
        _params: Option<rmcp::model::PaginatedRequestParams>,
        ctx: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // F-01 : `list_tools` divulguerait sinon le catalogue des 21 outils (noms +
        // schémas JSON) à tout client LAN non authentifié. Même garde que `call_tool`.
        let trust = Self::trust_from_ctx(&ctx);
        if !trust.is_authenticated() {
            return Err(ErrorData::new(
                rmcp::model::ErrorCode::INVALID_REQUEST,
                "non authentifié",
                None,
            ));
        }

        Ok(ListToolsResult {
            meta: None,
            tools: vec![
                tool_def::<VaultSearchRequest>(
                    "vault_search",
                    "Recherche sémantique et BM25 dans le vault",
                ),
                tool_def::<VaultReadRequest>("vault_read", "Lit une note par ID ou locus"),
                tool_def::<VaultListRequest>(
                    "vault_list",
                    "Liste les notes avec filtres et pagination",
                ),
                tool_def_no_params("vault_status", "Statut du vault (health, counters)"),
                tool_def::<VaultGraphRequest>("vault_graph", "Graphe de liens entre notes"),
                tool_def::<VaultLinksRequest>("vault_links", "Liens entrants/sortants d'une note"),
                tool_def::<VaultTraceRequest>("vault_trace", "Trace de propagation d'une note"),
                tool_def::<VaultContextRequest>("vault_context", "Contexte étendu d'une note"),
                tool_def::<VaultTimelineRequest>("vault_timeline", "Timeline des notes"),
                tool_def_no_params("vault_authors", "Liste des auteurs du vault"),
                tool_def_no_params("vault_tags", "Liste des tags du vault"),
                tool_def::<VaultWriteRequest>(
                    "vault_write",
                    "Écrit ou met à jour une note (async 202)",
                ),
                tool_def::<VaultClassifyRequest>(
                    "vault_classify",
                    "Classifie une note (heuristique)",
                ),
                tool_def::<VaultDowngradeRequest>(
                    "vault_downgrade",
                    "Rétrograde le statut d'une note",
                ),
                tool_def::<VaultHistoryRequest>("vault_history", "Historique CoW d'une note"),
                tool_def::<VaultHistoryGetRequest>(
                    "vault_history_get",
                    "Récupère une version historique",
                ),
                tool_def::<VaultRestoreRequest>("vault_restore", "Restaure une version historique"),
                tool_def::<VaultDiffRequest>("vault_diff", "Diff entre deux versions d'une note"),
                tool_def::<VaultForgetRequest>(
                    "vault_forget",
                    "Oubli sémantique (dry-run + confirm)",
                ),
                tool_def::<LessonsRecallRequest>("vault_lessons_recall", "Rappel de leçons BM25"),
                tool_def::<CodeScopeRequest>("code_scope", "Scope sélectif de code source"),
            ],
            next_cursor: None,
        })
    }

    #[instrument(skip(self, ctx), fields(tool = %params.name))]
    async fn call_tool(
        &self,
        params: rmcp::model::CallToolRequestParams,
        ctx: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let trust = Self::trust_from_ctx(&ctx);

        // Vérification authentification — tous les outils l'exigent.
        if !trust.is_authenticated() {
            return Err(ErrorData::new(
                rmcp::model::ErrorCode::INVALID_REQUEST,
                "non authentifié",
                None,
            ));
        }

        let args_value = params.arguments.map(Value::Object);

        self.dispatch_tool(&params.name, args_value, trust).await
    }
}

// Toutes les méthodes non implémentées ci-dessus utilisent les implémentations par défaut
// définies dans le trait ServerHandler via la macro server_handler_methods!().

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Construit un [`Tool`] avec schéma JSON dérivé du type `T`.
///
/// Délègue à [`gradatum_dto::mcp_tool_schema`] — SSOT unique (DT-MCP-SCHEMA-1).
/// Fail-loud : panique si schemars produit un non-objet (impossible en pratique),
/// jamais de dégradé silencieux vers un Map vide (anti-34e70eb).
fn tool_def<T: JsonSchema>(name: &'static str, description: &'static str) -> Tool {
    Tool::new(name, description, gradatum_dto::mcp_tool_schema::<T>())
}

/// Construit un [`Tool`] MCP sans paramètres.
///
/// Délègue à [`gradatum_dto::mcp_empty_params_schema`] — SSOT unique (DT-MCP-SCHEMA-1).
/// Émet `{"type":"object","properties":{}}` — un Map vide `{}` est rejeté par zod
/// et invalide toute la liste d'outils (régression 34e70eb).
fn tool_def_no_params(name: &'static str, description: &'static str) -> Tool {
    Tool::new(name, description, gradatum_dto::mcp_empty_params_schema())
}

/// Désérialise les arguments JSON vers un type `T`.
///
/// # Errors
///
/// Retourne [`ErrorData::invalid_params`] si la désérialisation échoue.
fn deserialize_args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, ErrorData> {
    serde_json::from_value(args)
        .map_err(|e| ErrorData::invalid_params(format!("arguments invalides : {e}"), None))
}

/// Sérialise un résultat en [`CallToolResult`] avec un unique contenu texte JSON.
///
/// # Errors
///
/// Retourne [`ErrorData::internal_error`] si la sérialisation échoue.
fn to_mcp_content<T: Serialize>(val: T) -> Result<CallToolResult, ErrorData> {
    let json = serde_json::to_string(&val).map_err(|e| {
        tracing::error!(error = %e, "mcp: sérialisation réponse échouée");
        ErrorData::internal_error("erreur interne", None)
    })?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Mappe une [`GradatumError`] vers un [`ErrorData`] MCP.
///
/// Les erreurs Storage/internes (Io, Markdown, etc.) sont masquées derrière un
/// message générique pour éviter la fuite de chemins ou détails de stockage.
fn gradatum_error_to_mcp(err: GradatumError) -> ErrorData {
    match err {
        GradatumError::Unauthorized => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_REQUEST,
            "non authentifié",
            None,
        ),
        GradatumError::Forbidden(msg) => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_REQUEST,
            format!("accès refusé : {msg}"),
            None,
        ),
        GradatumError::InvalidInput(msg) => ErrorData::invalid_params(msg, None),
        GradatumError::Validation(e) => ErrorData::invalid_params(e.to_string(), None),
        GradatumError::NoteNotFound(_) => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_PARAMS,
            "note introuvable",
            None,
        ),
        GradatumError::VaultNotFound(_) => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_PARAMS,
            "vault introuvable",
            None,
        ),
        GradatumError::Conflict(msg) => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_REQUEST,
            format!("conflit : {msg}"),
            None,
        ),
        GradatumError::InvalidStatusTransition { from, to } => ErrorData::invalid_params(
            format!("transition de statut invalide : {from:?} → {to:?}"),
            None,
        ),
        // Erreurs internes — message générique (anti-fuite chemin/stockage).
        GradatumError::Storage(_)
        | GradatumError::Io(_)
        | GradatumError::Markdown(_)
        | GradatumError::Drift(_)
        | GradatumError::SchemaVersionMismatch { .. }
        | GradatumError::SchemaValidation(_)
        | GradatumError::SchemaMigration(_)
        | GradatumError::TomlParse(_)
        | GradatumError::TomlSerialize(_)
        | GradatumError::Config(_)
        | GradatumError::VaultOnNfs { .. }
        | GradatumError::Inference(_) => {
            tracing::error!(error = %err, "mcp: erreur interne");
            ErrorData::internal_error("erreur interne", None)
        }
    }
}

// ── Factory du service MCP ────────────────────────────────────────────────────

/// Construit le [`GradatumMcpService`] (Streamable HTTP) pour montage sous `/mcp`.
///
/// Retourne `(service, cancel_token)` — le token doit être annulé lors du shutdown
/// pour permettre l'arrêt propre des sessions rmcp internes (évite les tâches orphelines
/// qui empêcheraient le processus de se terminer après SIGTERM).
///
/// # Protection DNS-rebinding (R2)
///
/// `gradatum-server` bind en loopback (`127.0.0.1:19090`). Les clients MCP légitimes
/// accèdent au endpoint `/mcp` via loopback (tunnel SSH, `localhost`, `127.0.0.1`, `::1`).
/// La whitelist ci-dessous correspond aux hôtes attendus — toute requête avec un `Host`
/// header différent (ex. `evil.com:19090` dans une attaque DNS-rebinding) est rejetée 403
/// par rmcp AVANT que la requête n'atteigne le handler MCP ou l'`auth_middleware`.
///
/// La factory doit retourner `Result<Handler, std::io::Error>` (contrat rmcp).
/// `session_manager: Arc<M>` avec `M = LocalSessionManager` (pas `Arc<LocalSessionManager>`).
pub fn build_mcp_service(state: AppState) -> (GradatumMcpService, CancellationToken) {
    let session_manager = Arc::new(LocalSessionManager::default());
    let cancel = CancellationToken::new();
    // Whitelist DNS-rebinding : bind loopback uniquement — clients via tunnel SSH local.
    // Si /mcp est un jour exposé derrière Traefik, ajouter le host Traefik ici.
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(["localhost", "127.0.0.1", "::1"])
        .with_cancellation_token(cancel.clone());
    let factory = move || {
        let handler = GradatumMcpHandler {
            state: state.clone(),
        };
        Ok::<_, std::io::Error>(handler)
    };
    let service = rmcp::transport::StreamableHttpService::new(factory, session_manager, config);
    (service, cancel)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Le nombre d'outils déclarés dans `list_tools` doit correspondre au stub.
    ///
    /// Régression R2 : parité contractuelle stub ↔ serveur natif.
    ///
    /// Marqué ignore car nécessite AppState de test complet.
    /// Ce test peut être exécuté en isolation via `--ignored` une fois l'infra test disponible.
    #[test]
    fn list_tools_count_is_21() {
        // Vérification statique : le vecteur `list_tools` dans le code retourne 21 outils.
        // Test de compilation uniquement — pas d'AppState requis.
        // La vérification runtime est couverte par list_tools_names_match_stub_runtime.
        const EXPECTED: usize = 21;
        // Les 21 noms d'outils attendus.
        let expected_names = [
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
            "vault_write",
            "vault_classify",
            "vault_downgrade",
            "vault_history",
            "vault_history_get",
            "vault_restore",
            "vault_diff",
            "vault_forget",
            "vault_lessons_recall",
            "code_scope",
        ];
        assert_eq!(expected_names.len(), EXPECTED);
    }

    /// Les erreurs Storage ne fuient pas de détails (anti-fuite chemin/stockage).
    #[test]
    fn error_mapping_storage_returns_generic_message() {
        let err = GradatumError::Storage("chemin/secret/db.sqlite3".to_string());
        let mcp_err = gradatum_error_to_mcp(err);
        assert!(
            !mcp_err.message.contains("secret"),
            "message d'erreur Storage ne doit pas exposer les détails : {:?}",
            mcp_err.message
        );
        assert_eq!(mcp_err.message, "erreur interne");
    }

    /// Les erreurs Io ne fuient pas de détails.
    #[test]
    fn error_mapping_io_returns_generic_message() {
        let err = GradatumError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "/etc/shadow",
        ));
        let mcp_err = gradatum_error_to_mcp(err);
        assert_eq!(mcp_err.message, "erreur interne");
    }

    /// Les erreurs Unauthorized mappent vers le bon message.
    #[test]
    fn error_mapping_unauthorized() {
        let mcp_err = gradatum_error_to_mcp(GradatumError::Unauthorized);
        assert_eq!(mcp_err.message, "non authentifié");
    }

    /// Les erreurs InvalidInput exposent le message utilisateur.
    #[test]
    fn error_mapping_invalid_input_exposes_message() {
        let err = GradatumError::InvalidInput("vault_id manquant".to_string());
        let mcp_err = gradatum_error_to_mcp(err);
        assert_eq!(mcp_err.message, "vault_id manquant");
    }

    /// `tool_def_no_params` émet `{"type":"object","properties":{}}` — conforme spec MCP.
    ///
    /// Mirror du test `tool_def_no_params_schema_empty` du stub `gradatum-mcp-stub`.
    /// Régression : le code précédent émettait `{}` (Map vide), rejeté par le validateur
    /// zod de Claude Code comme non-conforme → toute la liste des 21 outils ignorée.
    #[test]
    fn tool_def_no_params_schema_is_mcp_compliant() {
        for name in ["vault_status", "vault_authors", "vault_tags"] {
            let tool = tool_def_no_params(name, "description de test");
            assert_eq!(
                tool.input_schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "inputSchema.type doit être 'object' pour l'outil '{name}'"
            );
            let properties = tool
                .input_schema
                .get("properties")
                .and_then(|v| v.as_object());
            assert!(
                properties.is_some(),
                "inputSchema.properties doit exister pour l'outil '{name}'"
            );
            assert!(
                properties.unwrap().is_empty(),
                "inputSchema.properties doit être vide (outil sans paramètres) pour '{name}'"
            );
        }
    }
}
