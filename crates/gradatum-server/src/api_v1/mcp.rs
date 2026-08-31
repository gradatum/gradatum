//! Serveur MCP natif in-process de `gradatum-server`.
//!
//! Expose les outils gradatum via le protocole MCP (Streamable HTTP), en
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
//! # Outils exposés
//!
//! `tool_catalog` est l'autorité unique : c'est la déclaration réellement servie
//! par `list_tools`, et le `match` de `dispatch_tool` en est le miroir exact
//! (invariant tenu par le test `tool_catalog_declares_the_expected_tool_names`).
//! Ni la liste des noms ni leur nombre ne sont recopiés ici — un cardinal ou une
//! énumération écrits à la main se périment en silence à l'outil suivant, et une
//! doc qui ment est pire qu'une doc absente.
//!
//! La relation avec le catalogue du stub `gradatum-mcp-stub` est désormais une propriété
//! mesurée, et non plus une intention : `scripts/mcp-catalog-parity.sh` compare les deux
//! catalogues servis et échoue sur toute divergence, dans les deux sens.
//!
//! Cette relation n'est PAS l'égalité. `job_status` est déclaré ici et absent du stub,
//! parce qu'il est le seul outil de ce catalogue sans jumeau REST : il est servi
//! in-process par [`crate::api_v1::jobs_v2::job_status_mcp`], dont le `JobStatusView` — et son champ
//! décisif `terminal` — n'est sérialisé par aucune route HTTP. Un proxy stdio→REST n'a
//! donc rien à appeler. L'écart est énuméré dans ce gate, avec une garde de péremption qui
//! rougit sur trois signaux : une registration de route dont le chemin OU le handler nomme
//! `job_status`, et l'apparition d'un second producteur de `JobStatusView` dans ce crate.
//! Ce troisième signal est celui qui compte : toutes les routes de jobs étant namespacées
//! `/jobs/{id}/…`, un jumeau REST peut parfaitement ne nommer `job_status` nulle part.
//! La garde ne couvre PAS un jumeau qui servirait la même information sous un autre type —
//! cet angle mort est écrit dans `scripts/mcp-catalog-parity.sh` et relève de la revue.
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
        CallToolResult, Content, Implementation, InitializeRequestParams, ListToolsResult,
        ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
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
    CodeScopeRequest, CreateFeatureCardRequest, JobStatusRequest, LessonsRecallRequest,
    ProactiveRecallFeedbackRequest, ProactiveRecallRequest, VaultArchivesListRequest,
    VaultClassifyRequest, VaultDiffRequest, VaultDowngradeRequest, VaultForgetRequest,
    VaultHistoryGetRequest, VaultHistoryRequest, VaultRestoreRequest,
};

use crate::{
    api_v1::{
        code_scope::{code_scope_impl, validate_code_vault_id},
        compact::{self, CompactBody},
        dto::{
            VaultContextRequest, VaultGraphRequest, VaultLinksRequest, VaultListRequest,
            VaultReadRequest, VaultSearchRequest, VaultTagsRequest, VaultTimelineRequest,
            VaultTraceRequest, VaultWriteRequest,
        },
        forget::vault_forget_mcp_impl,
        logic,
    },
    state::AppState,
};

// ── Type public du service MCP ────────────────────────────────────────────────

/// Service MCP Streamable HTTP exposant les outils gradatum (cf. `tool_catalog`).
///
/// Construit via [`build_mcp_service`] et monté sous `/mcp` dans `main.rs`.
///
/// Le paramètre générique `M` est `LocalSessionManager` (pas `Arc<LocalSessionManager>`) :
/// `StreamableHttpService<S, M>` stocke `session_manager: Arc<M>` en interne.
pub type GradatumMcpService =
    rmcp::transport::StreamableHttpService<GradatumMcpHandler, LocalSessionManager>;

/// Maximum body size for `/mcp` requests (anti-DoS) — 512 KiB.
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

/// Handler MCP — implémente [`ServerHandler`] pour les outils gradatum.
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

        // P1-1 (reviewer) : call site UNIQUE, AVANT le match.
        // La map fermée de `McpToolCounters` filtre les noms inconnus (no-op) — pas
        // besoin d'un site séparé par arm. Cardinalité bornée garantie par cette map.
        self.state.mcp_tool_counters.record(name);

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
                let req: VaultTagsRequest = deserialize_args(args)?;
                let res = logic::vault_tags_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            // ── Outils read avec paramètres ───────────────────────────────────
            "vault_search" => {
                let req: VaultSearchRequest = deserialize_args(args)?;
                let want_compact = req.compact;
                // Capture the query before `req` is moved — only on the compact path,
                // where it feeds the absence-hint form check.
                let compact_query = want_compact.then(|| req.query.clone());
                let res = logic::vault_search_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                if want_compact {
                    to_mcp_content(CompactBody {
                        compact: compact::render_search(
                            &res,
                            compact_query.as_deref().unwrap_or_default(),
                        ),
                    })
                } else {
                    to_mcp_content(res)
                }
            }
            "vault_read" => {
                let req: VaultReadRequest = deserialize_args(args)?;
                let want_compact = req.compact;
                let res = logic::vault_read_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                if want_compact {
                    to_mcp_content(CompactBody {
                        compact: compact::render_read(&res),
                    })
                } else {
                    to_mcp_content(res)
                }
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
                let want_compact = req.compact;
                let res = logic::vault_timeline_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                if want_compact {
                    to_mcp_content(CompactBody {
                        compact: compact::render_timeline(&res),
                    })
                } else {
                    to_mcp_content(res)
                }
            }
            "vault_lessons_recall" => {
                let req: LessonsRecallRequest = deserialize_args(args)?;
                let want_compact = req.compact;
                let class = req.class.clone();
                let res = logic::lessons_recall_impl(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                if want_compact {
                    to_mcp_content(CompactBody {
                        compact: compact::render_recall(&res, &class),
                    })
                } else {
                    to_mcp_content(res)
                }
            }
            // ── Outils write ──────────────────────────────────────────────────
            "vault_write" => {
                let req: VaultWriteRequest = deserialize_args(args)?;
                // L'author vient du credential (`trust.subject()`), appliqué par
                // `effective_author` dans `vault_write_impl` — JAMAIS de l'en-tête
                // `X-Gradatum-Agent` (v2.0.0, Task 9 : suppression du repli d'en-tête).
                let request_id = ulid::Ulid::generate().to_string();
                let res = logic::vault_write_impl(
                    &self.state,
                    &trust,
                    req,
                    &request_id,
                    logic::FeatureWriteAuthority::External,
                )
                .await
                .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "create_feature_card" => {
                let req: CreateFeatureCardRequest = deserialize_args(args)?;
                let request_id = ulid::Ulid::generate().to_string();
                let res = logic::create_feature_card_impl(&self.state, &trust, req, &request_id)
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
            // ── Listing archives (F-100 1.6 — LECTURE SEULE) ──────────────────
            //
            // Seul l'accès en lecture au cycle delete/archive est exposé en MCP :
            // l'agent PRÉPARE les commandes CLI opérateur. delete/restore/purge
            // (mutations) ne sont JAMAIS ici (invariant fondateur F-100).
            "vault_archives_list" => {
                let req: VaultArchivesListRequest = deserialize_args(args)?;
                let res = logic::vault_archives_list_impl(&self.state, &trust, req)
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
                            "vault '{}' invalid for code_scope: must start with 'code-'",
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
            // ── F-46 Proactive recall (B' in-process) ─────────────────────────
            "vault_proactive_recall" => {
                let req: ProactiveRecallRequest = deserialize_args(args)?;
                let res = crate::proactive_recall::proactive_recall(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                to_mcp_content(res)
            }
            "vault_proactive_recall_feedback" => {
                let req: ProactiveRecallFeedbackRequest = deserialize_args(args)?;
                crate::proactive_recall::proactive_recall_feedback(&self.state, &trust, req)
                    .await
                    .map_err(gradatum_error_to_mcp)?;
                // Feedback retourne Ok(()) — on émet un objet JSON vide (pattern 204→MCP).
                Ok(CallToolResult::success(vec![Content::text("{}")]))
            }
            // ── Job introspection (F-63 « tout MCP natif ») ───────────────────
            // Expose l'état terminal réel d'un job async (`vault_write` rend 202 =
            // enqueué, PAS écrit) — dernier maillon manquant pour confirmer une
            // écriture sans retomber sur du curl. Lecture seule, instant T.
            "job_status" => {
                let req: JobStatusRequest = deserialize_args(args)?;
                let view = crate::api_v1::jobs_v2::job_status_mcp(&self.state, &trust, &req.job_id)
                    .await
                    .map_err(job_status_error_to_mcp)?;
                to_mcp_content(view)
            }
            _ => Err(ErrorData::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                format!("unknown tool: {name}"),
                None,
            )),
        }
    }

    /// Loads the caller's **own** soul body from vault section `identity/<subject>`, if present.
    ///
    /// The agent identity is taken **from the credential** — `trust.subject()`, i.e. the JWT
    /// `sub` / api-key owner, a server-side value — never from a client-supplied header
    /// (the `X-Gradatum-Agent` header fallback is removed entirely). An unauthenticated caller,
    /// or one whose trust tier carries no subject (`Studio` / `Mtls`), resolves to **no soul**:
    /// the service never falls back to a default identity (R2, fail-closed).
    ///
    /// Returns `None` on **any** failure (no subject, missing note, vault KO) so that
    /// [`initialize`] degrades gracefully to bootstrap-only without breaking the MCP handshake
    /// (ADN 1 — never panics, never returns an error to the caller).
    ///
    /// The returned body is injected byte-stable into `InitializeResult.instructions` (C8):
    /// no dynamic field is added — the vault content is returned as-is. Reading a caller's own
    /// soul is always permitted by the identity read guard in [`logic::vault_read_impl`]
    /// (`caller_sub == target_agent`), so no separate authorisation check is needed here.
    pub(crate) async fn soul_instructions(&self, trust: &TrustContext) -> Option<String> {
        if !trust.is_authenticated() {
            return None;
        }
        // L'identité vient du credential — jamais d'un paramètre/en-tête client. Sujet absent
        // (`Studio` / `Mtls`) → aucune âme, aucun repli sur une identité par défaut (R2).
        let agent = trust.subject().map(gradatum_core::scope::AgentId::as_str)?;
        let mut req = VaultReadRequest::new(format!("identity/{agent}"));
        req.section = Some("identity".to_string());
        req.tenant_id = Some(gradatum_core::scope::TenantId::new("main"));
        match logic::vault_read_impl(&self.state, trust, req).await {
            Ok(resp) if !resp.content.is_empty() => {
                tracing::debug!(agent = %agent, "mcp::initialize: soul loaded");
                Some(resp.content)
            }
            Ok(_) => {
                tracing::debug!(
                    agent = %agent,
                    "mcp::initialize: empty soul note — degraded bootstrap"
                );
                None
            }
            Err(e) => {
                tracing::debug!(
                    agent = %agent,
                    error = %e,
                    "mcp::initialize: vault_read soul read failed — degraded bootstrap"
                );
                None
            }
        }
    }

    /// Négocie la version de protocole MCP à renvoyer au client (spec MCP « Lifecycle /
    /// Version Negotiation »).
    ///
    /// - Version demandée **servable** par le SDK ([`ProtocolVersion::KNOWN_VERSIONS`]) →
    ///   renvoyée à l'identique : client et serveur s'accordent sur cette version.
    /// - Version **inconnue / non servable** → repli sur [`ProtocolVersion::LATEST`], la
    ///   version la plus récente que le serveur sait servir (la spec impose de répondre
    ///   « another protocol version it supports », « SHOULD be the latest »). Le repli est
    ///   tracé en `WARN` — observable, jamais silencieux — puis le client décide de
    ///   poursuivre ou de se déconnecter. Le handshake n'est **jamais** cassé (ADN 1).
    ///
    /// # Pourquoi cette négociation vit ici, et pas dans `rmcp`
    ///
    /// Le transport **Streamable HTTP** (celui exposé par gradatum sous `/mcp`) renvoie la
    /// `protocol_version` du handler **verbatim** — il ne renégocie pas (contrairement au
    /// transport stdio `serve_server`, qui, lui, ajuste la version après coup). Comme
    /// [`Self::get_info`] annonce inconditionnellement `LATEST`, sans cette négociation tout
    /// client parlant une version supportée mais **antérieure** à `LATEST` (ex. Claude
    /// Desktop en `2025-06-18`) était rejeté définitivement. C'est donc ici l'unique point
    /// de négociation côté serveur HTTP.
    #[must_use]
    fn negotiate_protocol_version(requested: &ProtocolVersion) -> ProtocolVersion {
        if ProtocolVersion::KNOWN_VERSIONS.contains(requested) {
            requested.clone()
        } else {
            tracing::warn!(
                requested = %requested,
                served = %ProtocolVersion::LATEST,
                "mcp::initialize: unsupported protocol version — falling back to the latest \
                 version served (the client may disconnect, MCP Version Negotiation spec)"
            );
            ProtocolVersion::LATEST
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

    /// Overrides the default MCP `initialize` handshake to inject the soul (F-34 v0.7.3).
    ///
    /// Reads `identity/<subject>` from vault section `identity` — the agent is the credential
    /// subject, never the `X-Gradatum-Agent` header — and sets
    /// `InitializeResult.instructions` to the body byte-stable (C8).
    ///
    /// # Degraded mode
    ///
    /// Any failure (unauthenticated, no subject, missing note, vault KO) produces
    /// `instructions = None` — the agent runs on CLAUDE.md bootstrap alone.
    /// The MCP handshake is **never broken** regardless of vault availability (ADN 1).
    #[instrument(skip(self, context), fields(agent))]
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ServerInfo, ErrorData> {
        // 1. Construire l'info de base (statique, toujours valide).
        let mut info = self.get_info();

        // 1bis. Négocier la version de protocole avec le client (spec MCP Lifecycle).
        // `get_info()` annonce inconditionnellement `LATEST` ; le transport Streamable HTTP
        // renvoie cette version verbatim (aucune renégociation côté rmcp), donc sans cet
        // ajustement un client sur une version supportée mais antérieure serait rejeté.
        info =
            info.with_protocol_version(Self::negotiate_protocol_version(&request.protocol_version));

        // 2. L'identité vient du credential (v2.0.0, Task 12) — l'en-tête X-Gradatum-Agent
        //    n'est plus lu. Le span porte le sujet du token pour l'observabilité.
        let trust = Self::trust_from_ctx(&context);
        let agent = trust
            .subject()
            .map(gradatum_core::scope::AgentId::as_str)
            .unwrap_or("");
        tracing::Span::current().record("agent", agent);

        // 3. Charger l'âme du sujet — auth + ACL vérifiés en interne ; toute erreur → None.
        if let Some(soul_body) = self.soul_instructions(&trust).await {
            info = info.with_instructions(soul_body);
        }

        // 4. Préserver la danse peer_info du défaut rmcp (miroir de l'impl par défaut).
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }

        Ok(info)
    }

    // `tool_count` est déclaré vide puis RENSEIGNÉ depuis le catalogue réellement
    // servi. Il portait un `26` écrit en dur : un compte gravé dans un champ de
    // trace ment au premier outil ajouté, en silence, dans les logs — et un champ
    // de trace qui ment est pire qu'un champ absent. Le dériver ne coûte rien ici :
    // `tool_catalog()` est de toute façon appelé dans le corps.
    // Non renseigné sur le chemin non-authentifié : aucun catalogue n'y est
    // construit, donc rien à compter — un champ absent y est la mesure honnête.
    #[instrument(skip(self, ctx), fields(tool_count))]
    async fn list_tools(
        &self,
        _params: Option<rmcp::model::PaginatedRequestParams>,
        ctx: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // F-01 : `list_tools` divulguerait sinon le catalogue complet (noms +
        // schémas JSON) à tout client LAN non authentifié. Même garde que `call_tool`.
        let trust = Self::trust_from_ctx(&ctx);
        if !trust.is_authenticated() {
            return Err(ErrorData::new(
                rmcp::model::ErrorCode::INVALID_REQUEST,
                "not authenticated",
                None,
            ));
        }

        let tools = tool_catalog();
        tracing::Span::current().record("tool_count", tools.len());

        Ok(ListToolsResult {
            meta: None,
            tools,
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
                "not authenticated",
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

/// Catalogue des outils MCP déclarés par `list_tools` — SSOT de la surface exposée.
///
/// Extrait de `list_tools` pour être appelable depuis un test : la déclaration est
/// entièrement statique, seule la garde d'authentification qui la précède ne l'est pas.
/// Sans cette extraction, toute assertion sur la liste d'outils exige un `AppState` et un
/// `RequestContext` complets — c'est ce coût qui avait fait dériver le test de parité vers
/// une tautologie (un tableau écrit à la main, comparé à lui-même, vert quoi que le serveur
/// expose).
fn tool_catalog() -> Vec<Tool> {
    vec![
        tool_def::<VaultSearchRequest>("vault_search", "Semantic and BM25 search in the vault"),
        tool_def::<VaultReadRequest>("vault_read", "Read a note by ID or locus"),
        tool_def::<VaultListRequest>("vault_list", "List notes with filters and pagination"),
        tool_def_no_params("vault_status", "Vault status (health, counters)"),
        tool_def::<VaultGraphRequest>("vault_graph", "Link graph between notes"),
        tool_def::<VaultLinksRequest>("vault_links", "Inbound/outbound links of a note"),
        tool_def::<VaultTraceRequest>("vault_trace", "Propagation trace of a note"),
        tool_def::<VaultContextRequest>("vault_context", "Extended context of a note"),
        tool_def::<VaultTimelineRequest>("vault_timeline", "Notes timeline"),
        tool_def_no_params("vault_authors", "List vault authors"),
        tool_def::<VaultTagsRequest>(
            "vault_tags",
            "List vault tags (most frequent first, bounded; raise with `limit`)",
        ),
        tool_def::<VaultWriteRequest>("vault_write", "Write or update a note (async 202)"),
        tool_def::<CreateFeatureCardRequest>(
            "create_feature_card",
            "Create a project-map feature card whose F-XX number is assigned by the \
             server. Body carries the 5 non-feature roles (project/status/kind/release/\
             version) and must NOT contain a [[feature:…]] link. Async: returns \
             { feature, number, job_id, note_id, poll_url } — poll job_status.",
        ),
        tool_def::<VaultClassifyRequest>("vault_classify", "Classify a note (heuristic)"),
        tool_def::<VaultDowngradeRequest>("vault_downgrade", "Downgrade a note's status"),
        tool_def::<VaultHistoryRequest>("vault_history", "CoW history of a note"),
        tool_def::<VaultHistoryGetRequest>("vault_history_get", "Retrieve a historical version"),
        tool_def::<VaultRestoreRequest>("vault_restore", "Restore a historical version"),
        tool_def::<VaultDiffRequest>("vault_diff", "Diff between two versions of a note"),
        tool_def::<VaultForgetRequest>("vault_forget", "Semantic forget (dry-run + confirm)"),
        tool_def::<VaultArchivesListRequest>(
            "vault_archives_list",
            "List archived notes (READ-ONLY) — filters + pagination, to prepare operator CLI commands",
        ),
        tool_def::<LessonsRecallRequest>("vault_lessons_recall", "BM25 lessons recall"),
        tool_def::<CodeScopeRequest>("code_scope", "Selective source-code scope"),
        tool_def::<ProactiveRecallRequest>(
            "vault_proactive_recall",
            "Proactive memory recall (F-46)",
        ),
        tool_def::<ProactiveRecallFeedbackRequest>(
            "vault_proactive_recall_feedback",
            "Proactive recall feedback (F-46)",
        ),
        tool_def::<JobStatusRequest>(
            "job_status",
            "State of an async job by job_id. Returns { status, terminal, error, conflict, result_note }: `terminal=true` (Done/DLQ/Cancelled/Conflict) → conclude, `terminal=false` (Pending/Running/Waiting/Failed) → keep polling. Snapshot at instant T — the caller re-polls, no server-side wait.",
        ),
    ]
}

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
        .map_err(|e| ErrorData::invalid_params(format!("invalid arguments: {e}"), None))
}

/// Sérialise un résultat en [`CallToolResult`] avec un unique contenu texte JSON.
///
/// # Errors
///
/// Retourne [`ErrorData::internal_error`] si la sérialisation échoue.
fn to_mcp_content<T: Serialize>(val: T) -> Result<CallToolResult, ErrorData> {
    let json = serde_json::to_string(&val).map_err(|e| {
        tracing::error!(error = %e, "mcp: response serialization failed");
        ErrorData::internal_error("internal error", None)
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
            "not authenticated",
            None,
        ),
        GradatumError::Forbidden(msg) => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_REQUEST,
            format!("access denied: {msg}"),
            None,
        ),
        GradatumError::InvalidInput(msg) => ErrorData::invalid_params(msg, None),
        GradatumError::Validation(e) => ErrorData::invalid_params(e.to_string(), None),
        GradatumError::NoteNotFound(_) => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_PARAMS,
            "note not found",
            None,
        ),
        GradatumError::VaultNotFound(_) => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_PARAMS,
            "vault not found",
            None,
        ),
        GradatumError::Conflict(msg) => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_REQUEST,
            format!("conflict: {msg}"),
            None,
        ),
        GradatumError::InvalidStatusTransition { from, to } => ErrorData::invalid_params(
            format!("invalid status transition: {from:?} → {to:?}"),
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
        | GradatumError::Inference(_)
        // `GradatumError` is `#[non_exhaustive]` (API freeze v2.0.0): any future variant
        // is masked behind the same generic message — fail-safe, anti-leak default.
        | _ => {
            tracing::error!(error = %err, "mcp: internal error");
            ErrorData::internal_error("internal error", None)
        }
    }
}

/// Maps the [`StatusCode`](http::StatusCode) returned by [`jobs_v2::job_status_mcp`] to an
/// MCP [`ErrorData`].
///
/// `job_status_mcp` reuses the HTTP handler's `StatusCode` contract verbatim (zero auth
/// drift with `get_job_v2`); this bridge translates it to MCP error codes without leaking
/// internal detail — `500` and any unexpected code collapse to a generic `"internal error"`.
fn job_status_error_to_mcp(status: http::StatusCode) -> ErrorData {
    match status {
        http::StatusCode::BAD_REQUEST => {
            ErrorData::invalid_params("invalid job_id: not a valid ULID", None)
        }
        http::StatusCode::NOT_FOUND => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_PARAMS,
            "job not found",
            None,
        ),
        http::StatusCode::FORBIDDEN => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_REQUEST,
            "access denied",
            None,
        ),
        http::StatusCode::UNAUTHORIZED => ErrorData::new(
            rmcp::model::ErrorCode::INVALID_REQUEST,
            "not authenticated",
            None,
        ),
        // 500 + tout code inattendu → message générique (anti-fuite, parité gradatum_error_to_mcp).
        _ => ErrorData::internal_error("internal error", None),
    }
}

// ── Factory du service MCP ────────────────────────────────────────────────────

/// Construit le [`GradatumMcpService`] (Streamable HTTP) pour montage sous `/mcp`.
///
/// Retourne `(service, cancel_token)` — le token doit être annulé lors du shutdown
/// pour permettre l'arrêt propre des connexions rmcp en cours (évite les tâches
/// orphelines qui empêcheraient le processus de se terminer après SIGTERM).
///
/// # Mode STATELESS
///
/// Le service est configuré en mode stateless (`.with_stateful_mode(false)`) :
/// - Chaque POST `/mcp` est traité de manière autonome (pas de session persistante).
/// - Aucun header `Mcp-Session-Id` n'est émis ni attendu.
/// - Les verbes GET et DELETE ne sont pas supportés (405 Method Not Allowed).
/// - Le [`LocalSessionManager`] reste configuré mais devient inerte — il n'est
///   jamais sollicité par rmcp en mode stateless (zéro changement de type, pas
///   de piège générique).
///
/// Ce mode élimine le décrochage des outils MCP côté Claude Code : les sessions
/// in-memory étaient perdues à chaque redémarrage du serveur, forçant une
/// reconnexion manuelle (`/mcp → Reconnect`). En stateless, chaque POST est
/// indépendant — aucune session n'est maintenue, aucune n'est perdue.
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
        .with_cancellation_token(cancel.clone())
        // Mode stateless : chaque POST est autonome, pas de session in-memory.
        // LocalSessionManager reste inerte (zéro changement de type requis).
        .with_stateful_mode(false);
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
    use std::collections::BTreeSet;

    use super::*;

    /// `tool_catalog` — la déclaration réellement servie par `list_tools` — expose
    /// exactement l'ensemble de noms attendu.
    ///
    /// Compare des **ENSEMBLES**, jamais un cardinal. Un compte gravé ne dit pas QUEL
    /// outil a bougé et se périme à chaque évolution (24 → 25 → 26 en une semaine) ;
    /// pire, il reste vert sur un renommage à effectif constant. Ici un ajout, un
    /// retrait ET un renommage font rougir, et le message nomme le delta des deux côtés.
    ///
    /// Ce test appelle la fonction de production. Le prédécesseur
    /// (`list_tools_count_is_25`) n'appelait rien : il vérifiait qu'un tableau de 25
    /// éléments écrit à la main en contenait 25 — vert quoi que le serveur expose.
    #[test]
    fn tool_catalog_declares_the_expected_tool_names() {
        let declared: BTreeSet<String> =
            tool_catalog().iter().map(|t| t.name.to_string()).collect();

        let expected: BTreeSet<String> = [
            "code_scope",
            "create_feature_card",
            "job_status",
            "vault_archives_list",
            "vault_authors",
            "vault_classify",
            "vault_context",
            "vault_diff",
            "vault_downgrade",
            "vault_forget",
            "vault_graph",
            "vault_history",
            "vault_history_get",
            "vault_lessons_recall",
            "vault_links",
            "vault_list",
            "vault_proactive_recall",
            "vault_proactive_recall_feedback",
            "vault_read",
            "vault_restore",
            "vault_search",
            "vault_status",
            "vault_tags",
            "vault_timeline",
            "vault_trace",
            "vault_write",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let missing: Vec<&String> = expected.difference(&declared).collect();
        let extra: Vec<&String> = declared.difference(&expected).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "surface d'outils MCP divergente — manquants (attendus, non déclarés) : {missing:?} ; \
             en trop (déclarés, non attendus) : {extra:?}. Tout ajout/retrait/renommage d'outil \
             doit être répercuté ici ET dans la parité du stub."
        );
    }

    /// Aucun nom d'outil n'est déclaré deux fois.
    ///
    /// Le test d'ensemble ci-dessus dédoublonne par construction : un doublon y serait
    /// invisible. Cette assertion sur le cardinal du `Vec` brut ferme cet angle mort —
    /// c'est le seul endroit où un compte est légitime, car il est **dérivé**, jamais gravé.
    #[test]
    fn tool_catalog_has_no_duplicate_names() {
        let catalog = tool_catalog();
        let unique: BTreeSet<String> = catalog.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(
            unique.len(),
            catalog.len(),
            "au moins un nom d'outil est déclaré plusieurs fois dans tool_catalog"
        );
    }

    /// GARDE D'INSTRUMENTATION (F-234) — tout outil déclaré dans `tool_catalog()`
    /// (la surface réellement servie par `list_tools`) DOIT avoir une entrée de
    /// compteur dans [`crate::mcp_usage::MCP_TOOL_KEYS`], et réciproquement.
    ///
    /// Compare **DEUX SOURCES DE PRODUCTION** — la surface d'outils et la table
    /// d'instrumentation — jamais un cardinal gravé. Un compte en dur (ce dépôt en
    /// portait deux occurrences : `list_tools_count_is_25` disparu, et
    /// `assert_eq!(MCP_TOOL_KEYS.len(), N)`) dérive au premier ajout et reste vert
    /// sur un renommage à effectif constant. Ici, une capacité exposée mais non
    /// comptée, une clé de compteur orpheline OU un renommage désaligné font
    /// rougir, et le message nomme le delta des deux côtés.
    ///
    /// Rend la dérive IMPOSSIBLE sans ouvrir la map fermée : le bornage de
    /// cardinalité Prometheus (map pré-peuplée, `record` no-op sur nom inconnu)
    /// reste intact — ce qui est prouvé ici est la **parité des deux ensembles
    /// statiques**, au moment du test, pas une mutation runtime de la map.
    ///
    /// Une exclusion délibérée (outil exposé volontairement non compté) est
    /// possible, mais DOIT être matérialisée par une allow-list explicite ici avec
    /// justification écrite — jamais une omission silencieuse. Aujourd'hui : aucune
    /// exclusion (les 26 outils sont comptés).
    #[test]
    fn every_declared_tool_is_instrumented() {
        let declared: BTreeSet<String> =
            tool_catalog().iter().map(|t| t.name.to_string()).collect();
        let instrumented: BTreeSet<String> = crate::mcp_usage::MCP_TOOL_KEYS
            .iter()
            .map(|(tool, _key)| (*tool).to_string())
            .collect();

        let uninstrumented: Vec<&String> = declared.difference(&instrumented).collect();
        let orphan_keys: Vec<&String> = instrumented.difference(&declared).collect();
        assert!(
            uninstrumented.is_empty() && orphan_keys.is_empty(),
            "parité instrumentation/surface MCP rompue — outils exposés SANS compteur \
             (à ajouter dans MCP_TOOL_KEYS, ou à exclure via allow-list justifiée) : \
             {uninstrumented:?} ; clés de compteur orphelines (aucun outil déclaré) : \
             {orphan_keys:?}"
        );
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
        assert_eq!(mcp_err.message, "internal error");
    }

    /// Les erreurs Io ne fuient pas de détails.
    #[test]
    fn error_mapping_io_returns_generic_message() {
        let err = GradatumError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "/etc/shadow",
        ));
        let mcp_err = gradatum_error_to_mcp(err);
        assert_eq!(mcp_err.message, "internal error");
    }

    /// Les erreurs Unauthorized mappent vers le bon message.
    #[test]
    fn error_mapping_unauthorized() {
        let mcp_err = gradatum_error_to_mcp(GradatumError::Unauthorized);
        assert_eq!(mcp_err.message, "not authenticated");
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
        for name in ["vault_status", "vault_authors"] {
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

    // ── Tests télémétrie MCP (Task 2) ──────────────────────────────────────────

    /// Construit un `TrustContext` authentifié minimal pour les tests.
    ///
    /// Tenant = "main", scope = ["read"] — suffisant pour passer `is_authenticated()`.
    /// L'ACL du state de test (preset vide = deny-all) bloque les handlers métier,
    /// mais les incréments de compteurs ont lieu AVANT les vérifications ACL.
    fn make_trust() -> TrustContext {
        TrustContext::BearerToken {
            kid: "test-kid".to_string(),
            aud: "gradatum".to_string(),
            sub: "test-agent".into(),
            scopes: vec!["read".to_string(), "write".to_string()],
            tenant_id: "main".into(),
            jti: None,
        }
    }

    /// `dispatch_tool` d'un outil connu incrémente le compteur MCP correspondant.
    ///
    /// Ici `vault_status` : l'outil peut échouer (ACL deny sur state de test),
    /// mais le compteur `mcp:vault_status` est incrémenté avant le `match name`.
    #[tokio::test]
    async fn dispatch_records_known_tool() {
        use crate::state::AppState;
        let state = AppState::new();
        let handler = GradatumMcpHandler {
            state: state.clone(),
        };
        let trust = make_trust();
        // vault_status peut échouer (ACL deny) — on s'en fiche, seul le compteur compte.
        let _ = handler.dispatch_tool("vault_status", None, trust).await;
        let entries = state.mcp_tool_counters.swap_all_for_test();
        let count = entries
            .iter()
            .find(|(k, _)| *k == "mcp:vault_status")
            .map(|(_, n)| *n)
            .unwrap_or(0);
        assert_eq!(count, 1, "mcp:vault_status doit être incrémenté une fois");
    }

    /// Test normatif P1-4a (reviewer) : double-count assumé sur les read-paths MCP.
    ///
    /// Un appel `vault_search` via MCP incrémente À LA FOIS :
    /// - `mcp_tool_counters["vault_search"]` (compteur MCP, via call site unique en tête de dispatch)
    /// - `read_usage_accumulators.vault_search` (accumulateur read-path, via `vault_search_impl`)
    ///
    /// This double-count is INTENTIONAL: the two counters track distinct semantics.
    #[tokio::test]
    async fn mcp_read_path_call_increments_both_families() {
        use std::sync::atomic::Ordering;

        use crate::state::AppState;
        let state = AppState::new();
        let handler = GradatumMcpHandler {
            state: state.clone(),
        };
        let trust = make_trust();
        let args = serde_json::json!({"query": "test"});
        // vault_search_impl peut échouer (ACL deny) — les incréments ont eu lieu.
        let _ = handler
            .dispatch_tool("vault_search", Some(args), trust)
            .await;

        // Accumulateur read-path existant (vault_search_impl).
        let rp_count = state
            .read_usage_accumulators
            .vault_search
            .load(Ordering::Relaxed);
        assert_eq!(
            rp_count, 1,
            "read_usage_accumulators.vault_search doit être 1"
        );

        // Compteur MCP nouveau (dispatch_tool head).
        let entries = state.mcp_tool_counters.swap_all_for_test();
        let mcp_count = entries
            .iter()
            .find(|(k, _)| *k == "mcp:vault_search")
            .map(|(_, n)| *n)
            .unwrap_or(0);
        assert_eq!(mcp_count, 1, "mcp:vault_search doit être 1");
    }

    /// Un nom d'outil inconnu n'incrémente aucun compteur MCP (anti-cardinalité).
    ///
    /// Test P1-4b complémentaire : preuve que la map fermée bloque les noms orphelins.
    #[tokio::test]
    async fn dispatch_unknown_tool_increments_no_mcp_counter() {
        use crate::state::AppState;
        let state = AppState::new();
        let handler = GradatumMcpHandler {
            state: state.clone(),
        };
        let trust = make_trust();
        // Outil inexistant → erreur METHOD_NOT_FOUND, aucun compteur.
        let result = handler.dispatch_tool("outil_inexistant", None, trust).await;
        assert!(result.is_err(), "outil inconnu doit retourner une erreur");
        let entries = state.mcp_tool_counters.swap_all_for_test();
        assert!(
            entries.iter().all(|(_, n)| *n == 0),
            "aucun compteur ne doit être incrémenté pour un outil inconnu"
        );
    }

    // ── Tests soul_instructions (Task 6′ F-34 ; Task 12 v2.0.0) ─────────────────
    //
    // Stratégie : tester `soul_instructions` directement (fn testable séparée de
    // `initialize`) pour éviter de construire un `RequestContext<RoleServer>` complet
    // (handshake MCP trop lourd en test unitaire). La propriété end-to-end « le sujet du
    // credential gouverne l'âme, l'en-tête n'a aucun effet » est verrouillée par le test de
    // frontière `mcp_initialize_serves_soul_from_credential_subject_not_header`
    // (`tests/mcp_native.rs`) — un test unitaire par fonction ne pouvait pas la prouver.
    //
    // v2.0.0 (Task 12) : `soul_instructions` ne prend plus d'`agent` — il dérive du
    // `trust.subject()` (credential). Un sujet inconnu ne peut donc plus être demandé : la
    // lecture est TOUJOURS celle de sa propre âme.

    /// Non authentifié → `soul_instructions` retourne `None`.
    ///
    /// Vérifie le guard d'authentification avant toute lecture vault.
    #[tokio::test]
    async fn soul_instructions_unauthenticated_returns_none() {
        use crate::state::AppState;
        let handler = GradatumMcpHandler {
            state: AppState::new(),
        };
        let trust = TrustContext::Unauthenticated;
        let result = handler.soul_instructions(&trust).await;
        assert!(
            result.is_none(),
            "not authenticated → soul_instructions doit retourner None"
        );
    }

    /// Trust authentifié mais SANS sujet (`Studio`) → `None` (fail-closed R2).
    ///
    /// Prouve qu'aucune identité par défaut n'est servie quand le credential ne porte pas de
    /// sujet : le service ne se replie sur aucune âme.
    #[tokio::test]
    async fn soul_instructions_no_subject_returns_none() {
        use gradatum_core::trust::StudioScope;

        use crate::state::AppState;
        let handler = GradatumMcpHandler {
            state: AppState::new(),
        };
        // `Studio` est authentifié mais `subject()` == None → aucune âme, aucun repli.
        let trust = TrustContext::Studio {
            user: "admin@example".to_string(),
            scope: StudioScope::Admin,
            step_up_until: None,
        };
        let result = handler.soul_instructions(&trust).await;
        assert!(
            result.is_none(),
            "trust sans sujet (Studio) → None : aucun repli sur une identité par défaut (R2)"
        );
    }

    /// Sujet présent (`main-agent`) mais note absente → `None` (dégradé bootstrap, ADN 1).
    ///
    /// Prouve qu'une lecture vault KO/vide de sa propre âme ne casse pas le handshake MCP.
    #[tokio::test]
    async fn soul_instructions_authorized_note_absent_returns_none() {
        use crate::state::AppState;
        let handler = GradatumMcpHandler {
            state: AppState::new(),
        };
        let trust = TrustContext::BearerToken {
            kid: "test-kid".to_string(),
            aud: "gradatum".to_string(),
            sub: "main-agent".into(),
            scopes: vec!["read".to_string()],
            tenant_id: "main".into(),
            jti: None,
        };
        // main-agent lit identity/main-agent (sa propre âme) mais vault vide → None.
        let result = handler.soul_instructions(&trust).await;
        assert!(
            result.is_none(),
            "note absente doit retourner None (dégradé bootstrap, jamais panic)"
        );
    }

    /// Un sujet quelconque (`backend`) lit sa propre âme, note absente → `None`.
    ///
    /// Prouve que la résolution n'est pas figée sur `main-agent` : l'âme lue est
    /// `identity/<sujet>`, quel que soit le sujet.
    #[tokio::test]
    async fn soul_instructions_own_agent_note_absent_returns_none() {
        use crate::state::AppState;
        let handler = GradatumMcpHandler {
            state: AppState::new(),
        };
        let trust = TrustContext::BearerToken {
            kid: "test-kid".to_string(),
            aud: "gradatum".to_string(),
            sub: "backend".into(),
            scopes: vec!["read".to_string()],
            tenant_id: "main".into(),
            jti: None,
        };
        // backend lit identity/backend (dérivé du sujet), note absente → None.
        let result = handler.soul_instructions(&trust).await;
        assert!(
            result.is_none(),
            "own-agent soul absente doit retourner None (dégradé bootstrap)"
        );
    }

    /// Soul présente avec H1 canonique → `soul_instructions` retourne `Some(body)` non vide.
    ///
    /// Couvre le cas positif différé au smoke LIVE (livrable Tasks 1-4 v0.7.3) :
    /// prouve le chemin complet `soul_instructions` → `vault_read_impl` → `title_lookup`
    /// (match `body_text LIKE '# identity/main-agent\n%'`) → lecture vault → `Some(body)`.
    ///
    /// Condition nécessaire : le body doit commencer par `# identity/<agent>`.
    /// Sans ce H1, `title_lookup` retourne `Ok(None)` → `soul_instructions` retourne `None`
    /// silencieusement (dégradé bootstrap) — l'injection MCP est désactivée sans erreur visible.
    ///
    /// Ce test automatise la preuve qui nécessitait auparavant un smoke LIVE après seed manuel.
    #[tokio::test]
    async fn soul_instructions_with_h1_present_returns_some() {
        use chrono::Utc;
        use gradatum_acl_policy::AclEngine;
        use gradatum_auth::jwt::JwtService;
        use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
        use gradatum_core::scope::VaultId;
        use gradatum_core::section::Section;
        use gradatum_core::status::NoteStatus;
        use gradatum_vault::{Registry, Vault};
        use tempfile::TempDir;

        // ACL permissive : main-agent lit tout — la lecture d'une âme est ici celle du sujet
        // lui-même (identity/main-agent), toujours autorisée par la garde read (own soul).
        const ACL: &str = r#"
[[consumer]]
identity = "main-agent"
read_patterns  = ["**"]
write_patterns = ["**"]
"#;
        // Corps soul valide avec H1 canonique en tête.
        // `validate_soul` l'accepte : `extract_section` cherche `## SECTION` (niveau 2),
        // la ligne `# identity/main` (niveau 1) est ignorée par le parser soul.
        const SOUL_BODY: &str = "\
## INVARIANTS
INV-CANARY | REQUIRED | response.prefix matches ^\\(TODAY\\):
INV-LANG | REQUIRED | response.language == fr

## GATES
GATE-PIPELINE | multi_step OR service_live -> invoke gov-pipeline-agents

## NARRATIVE
Tu es le Général en Chef. Ton: direct, FR.
";

        let tmp = TempDir::new().expect("TempDir — soul_instructions_with_h1_present_returns_some");
        let vault_path = tmp.path().join("vault");
        let vault = Arc::new(
            Vault::create(&vault_path, VaultId::new("main"))
                .await
                .expect("Vault::create — soul_instructions_with_h1_present_returns_some"),
        );

        let acl = AclEngine::from_preset_str(ACL)
            .expect("ACL permissive — soul_instructions_with_h1_present_returns_some");
        let mut state = AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl)
            .with_vault_arc(vault.clone() as Arc<dyn Registry>);
        // Partager l'index interne du vault pour que title_lookup voie les notes écrites.
        state.search = vault.index().clone();

        // Seed l'âme du sujet : body commence par `# identity/main-agent` (sub == owner,
        // lecture de sa propre âme).
        let title = "identity/main-agent";
        let frontmatter = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus: None,
            section: Section::Identity,
            status: NoteStatus::Live,
            status_reason: None,
            status_changed: None,
            tags: Default::default(),
            author: None,
            created: Utc::now(),
            updated: None,
            extra: ExtraFields::empty(),
            provenance: None,
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };
        let body_with_h1 = format!("# {title}\n{SOUL_BODY}");
        let note = vault.write_note(frontmatter, body_with_h1).await.expect(
            "vault.write_note seed soul avec H1 — soul_instructions_with_h1_present_returns_some",
        );
        // upsert_note_title aligne la colonne `title` avec le chemin identity/<agent>.
        state
            .search
            .upsert_note_title(note.frontmatter.vault_id.as_str(), &note.id, title)
            .await
            .expect("upsert_note_title — soul_instructions_with_h1_present_returns_some");

        let handler = GradatumMcpHandler { state };
        let trust = TrustContext::BearerToken {
            kid: "test-kid".to_string(),
            aud: "gradatum".to_string(),
            sub: "main-agent".into(),
            scopes: vec!["read".to_string()],
            tenant_id: "main".into(),
            jti: None,
        };

        let result = handler.soul_instructions(&trust).await;
        assert!(
            result.is_some(),
            "soul présente avec H1 doit retourner Some(body) (path résolu via title_lookup)"
        );
        let body = result.unwrap();
        assert!(
            !body.is_empty(),
            "soul_instructions doit retourner un body non vide quand la note existe: body={body:?}"
        );
    }

    // Les tests `author_for_mcp_write_*` ont été RETIRÉS avec la fonction (Task 9,
    // v2.0.0) : ils asservissaient l'attribution à l'en-tête `X-Gradatum-Agent`, ce que
    // ce lot supprime. La propriété de remplacement (« le sujet du credential gagne sur
    // l'en-tête ») est verrouillée end-to-end par le test de frontière
    // `mcp_vault_write_attributes_author_to_credential_subject_not_header`
    // (`tests/mcp_native.rs`) — un test unitaire par fonction ne pouvait pas la prouver.
}
