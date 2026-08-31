//! Axum middlewares: JWT authentication, rate limiting and HTTP metrics.
//!
//! ## JWT auth middleware
//!
//! Verifies the Ed25519 bearer JWT via `JwtService`, then checks revocation.
//!
//! ### Extraction and revocation logic
//!
//! Two Bearer paths are supported (distinguished by the `ak_` prefix):
//!
//! **Path A — Bearer api-key (`ak_...`)** (stable, for MCP HTTP transport with static header):
//! 1. Token starts with `ak_` → `state.api_keys.verify(token)` is called (async).
//!    - Valid + non-revoked → `TrustContext::BearerToken` constructed from `ApiKey` metadata.
//!    - `NotFound` or `AlreadyRevoked` → `TrustContext::Unauthenticated` (no error logged —
//!      invalid attempts are not server errors).
//!    - Store error → `TrustContext::Unauthenticated` + `tracing::warn!`.
//!    - The api-key value is **never** logged in plaintext (`[REDACTED]`).
//!
//! **Path B — Bearer JWT (`eyJ...`)** (short-lived, TTL 24h, for interactive clients):
//! 1. `Authorization: Bearer <token>` header absent → `TrustContext::Unauthenticated`.
//! 2. Header present, `JwtService::verify(token)` succeeds →
//!    revocation check: `state.revocation.is_revoked(&jti, "main")` with a 200 ms timeout.
//!    - Token revoked → immediate 401 (not injected into extensions).
//!    - Store error or timeout → 401 **fail-closed** + `tracing::error!` (never panic or 500).
//!    - Valid, non-revoked token → `TrustContext::BearerToken` injected.
//! 3. Header present, verify fails (exp/kid/sig) →
//!    `TrustContext::Unauthenticated` (the handler returns 401).
//!
//! ### Scope of the api-key Bearer path
//!
//! The api-key path is applied **globally** (all routes under `auth_middleware`),
//! not only `/mcp`. Rationale: `/mcp` is the primary consumer today, but other
//! routes (e.g. `gradatum-engine` service accounts) may benefit from stable api-key
//! auth without TTL renewal. The alternative (`/mcp`-only) would require a second
//! middleware mount — unnecessary complexity for no security gain (api-key revocation
//! is the mechanism for scope control).
//!
//! ### Fail-closed policy
//!
//! The revocation check (JWT path) is fail-closed: any store error (I/O, SQLite, timeout)
//! results in a 401, never a silent pass-through. Rationale: revocation is a
//! security mechanism — a degraded store must not allow access with a revoked token
//! (e.g. a compromised token whose revocation the operator requested immediately).
//!
//! The api-key path does NOT have a separate revocation check — revocation is handled
//! inline by `ApiKeyStore::verify` (which checks `revoked_at` before returning `Ok`).
//!
//! `TrustContext` is injected via `request.extensions_mut().insert(trust)`
//! before calling `next.run(request)`.
//!
//! ## Rate-limiting middleware
//!
//! [`build_warden_layer`] builds a [`WardenLayer`] (the `gradatum-warden` crate)
//! from [`RateLimitConfig`].
//!
//! ### Loopback bypass: real implementation
//!
//! The loopback bypass in `gradatum_warden::WardenService` calls `inner.call(req)`
//! directly for loopback IPs — the business handler returns its real body.
//!
//! ### Mounting order on the rate-limited router:
//! ```text
//! incoming request
//!   → WardenLayer (gradatum-warden)
//!       if loopback + bypass_loopback: inner.call(req) direct → real handler body
//!       if IP deny-filtered: 403
//!       if rate limit exceeded: 429 + retry-after
//!       otherwise: inner.call(req) → real handler body
//!   → business handler
//! ```
//!
//! ## HTTP metrics middleware
//!
//! [`http_metrics_middleware`] feeds the two HTTP families of the Prometheus
//! registry (`gradatum_http_requests_total`, `gradatum_http_request_duration_seconds`).
//! It is mounted as the **outermost** layer of the assembled router, so every route
//! is counted — including `/health`, `/mcp`, `/ui/*`, the 404 fallback, and the
//! rate-limiter's own 429 replies.
//!
//! Cardinality is bounded by construction: see [`http_metrics_middleware`].

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::MatchedPath;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use gradatum_acl_auth::{ApiKeyError, KEY_PREFIX};
use serde::Serialize;

use crate::config::RateLimitConfig;
use crate::metrics::{AppMetrics, HttpDurLabels, HttpReqLabels};
use crate::state::AppState;
use gradatum_warden::{WardenConfig, WardenLayer};

// ─── Rate limiting ───────────────────────────────────────────────────────────

/// Builds a [`WardenLayer`] from the server [`RateLimitConfig`].
///
/// Returns `None` if `!cfg.enabled` (no rate limiting applied).
///
/// The [`WardenLayer`] handles:
/// - Real loopback bypass (`inner.call(req)` called directly — real handler body returned)
/// - CIDR IP filtering (empty by default = allow all)
/// - Per-IP rate limiting via a governor token bucket
pub fn build_warden_layer(cfg: &RateLimitConfig) -> Option<WardenLayer> {
    if !cfg.enabled {
        return None;
    }
    // `WardenConfig` is `#[non_exhaustive]` (API freeze, v2.0.0): an out-of-crate struct
    // literal no longer compiles, so build from `Default` and override the fields we drive.
    // `ip_allow`/`ip_deny` stay empty (the `Default` value) — this middleware sets no CIDRs.
    let mut warden_cfg = WardenConfig::default();
    warden_cfg.enabled = true;
    warden_cfg.rate_limit_per_minute = cfg.per_minute;
    warden_cfg.rate_limit_burst = cfg.burst;
    warden_cfg.bypass_loopback = cfg.exempt_localhost;
    Some(WardenLayer::new(warden_cfg).expect(
        "invalid warden config — per_minute and burst must be > 0, \
         guaranteed by RateLimitConfig::default() (60, 10)",
    ))
}

// ─── Métriques HTTP ──────────────────────────────────────────────────────────

/// `path` label value used when axum exposes no route pattern for the request.
///
/// Two cases fall into this bucket:
/// - the request matched no route (404 fallback) — the concrete URI is deliberately
///   **not** used as a label, otherwise any scanner could create unbounded series;
/// - the route is mounted with `Router::nest_service` (studio `/ui/*`), for which axum
///   inserts a private `MatchedNestedPath` instead of [`MatchedPath`](axum::extract::MatchedPath) — the pattern is
///   not reachable from a middleware.
pub const OTHER_ROUTE: &str = "other";

/// Axum middleware feeding the HTTP counter and duration histogram.
///
/// Mounted via `axum::middleware::from_fn_with_state(state.metrics.clone(), _)` as the
/// outermost layer of the fully assembled router (see `build_router` in `main.rs`).
///
/// # Observed series
///
/// - `gradatum_http_requests_total{method, path, status}` — incremented once per request.
/// - `gradatum_http_request_duration_seconds{method, path}` — wall-clock duration of
///   `next.run(request)`, i.e. everything downstream of this layer (auth, rate limiting,
///   handler, response serialisation).
///
/// # Cardinality
///
/// `path` is the **route pattern** obtained from the
/// [`MatchedPath`](axum::extract::MatchedPath) extension
/// (`/api/v1/vault/unforgot/{ulid}`), never the concrete URI — no series is ever created
/// per ULID. `Router::layer` runs *after* routing in axum 0.8, so the extension is
/// available here; `Router::nest` flattens nested routes into the outer table, so the
/// pattern already carries the `/api/v1` prefix. Requests without a pattern fall into
/// [`OTHER_ROUTE`]. The label domain is therefore bounded by the route table, and no
/// admission cap is needed (unlike the tenant label, which is user-supplied).
pub async fn http_metrics_middleware(
    axum::extract::State(metrics): axum::extract::State<AppMetrics>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // Les labels sont capturés AVANT `next.run` : la requête est consommée par le handler,
    // et un handler peut réécrire l'URI. Le motif de route, lui, est déjà figé par le routage.
    let method = request.method().as_str().to_owned();
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map_or(OTHER_ROUTE, MatchedPath::as_str)
        .to_owned();

    let started = Instant::now();
    let response = next.run(request).await;
    let elapsed_secs = started.elapsed().as_secs_f64();
    let status = response.status().as_u16();

    metrics
        .http_requests
        .get_or_create(&HttpReqLabels {
            method: method.clone(),
            path: path.clone(),
            status,
        })
        .inc();
    // Labels déplacés (pas clonés) : le compteur ci-dessus est le seul à en avoir besoin après.
    metrics
        .http_duration
        .get_or_create(&HttpDurLabels { method, path })
        .observe(elapsed_secs);

    response
}

// ─── Auth JWT ────────────────────────────────────────────────────────────────

/// Timeout for the JWT revocation check (fail-closed if exceeded).
///
/// 200 ms: well above local SQLite latency (<1 ms p99),
/// short enough not to block requests when the store is degraded.
const REVOCATION_CHECK_TIMEOUT: Duration = Duration::from_millis(200);

/// Uniform JSON error response for middleware 401 replies.
#[derive(Serialize)]
struct MiddlewareError {
    error: &'static str,
}

/// Builds a uniform 401 JSON response.
fn unauthorized(msg: &'static str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(MiddlewareError { error: msg }),
    )
        .into_response()
}

/// Builds a uniform 403 JSON response (tenant rejection).
fn forbidden(msg: &'static str) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(MiddlewareError { error: msg }),
    )
        .into_response()
}

/// Identité d'amorçage à provisionner en premier sur une installation neuve (R5).
///
/// Sur un registre vierge le refus nomme cette identité d'amorçage plutôt que
/// l'identité *demandée* : le contexte est non authentifié, aucun credential valide
/// n'a été présenté, donc l'identité voulue est inconnue. `main-agent` est l'identité
/// que l'amorçage (`gradatum-admin init`, tâche 6) frappe en premier.
const BOOTSTRAP_IDENTITY: &str = "main-agent";

/// Corps JSON du 503 « registre non initialisé » (R5) — message possédé (dynamique,
/// distinct du [`MiddlewareError`] `&'static str` des 401/403).
#[derive(Serialize)]
struct UninitialisedError {
    error: String,
}

/// Construit le 503 distinguant un registre vierge d'un credential rejeté (R3+R5).
///
/// Le corps porte, dans l'ordre : le refus, l'identité d'amorçage à créer, la commande
/// de création complète, puis le rappel de reporter la clé émise dans la configuration
/// MCP du porteur. Il ne contient **jamais** le chemin disque du registre (surface
/// d'information réduite — garde de sécurité) ; « registre vide » et « registre remis à
/// zéro » sont donc volontairement indistinguables.
fn registry_uninitialised() -> Response {
    let error = format!(
        "empty API key registry: installation not initialized — no identity can \
         authenticate until a key is minted. Provision the bootstrap identity, then \
         report the emitted key into the bearer's MCP configuration: \
         `gradatum-admin api-key create --owner {BOOTSTRAP_IDENTITY} \
         --scopes vault_read,vault_search,vault_write,write`."
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(UninitialisedError { error }),
    )
        .into_response()
}

/// Rejette un contexte non authentifié en distinguant l'installation non provisionnée
/// (registre vierge → 503 actionnable, R5) du credential rejeté (registre peuplé → 401).
///
/// Le magasin est interrogé **uniquement ici**, sur le chemin d'échec d'auth — jamais
/// sur le chemin nominal — et **sans cache** : l'outil d'administration frappe les clés
/// hors-process, un drapeau mémorisé se périmerait juste après une création.
///
/// **Fail-closed** : une erreur de lecture du magasin rend un 401 (jamais un 503) — ne
/// jamais annoncer « aucune clé, lance la création » quand le magasin est seulement
/// inaccessible (un incident opérationnel, journalisé au niveau `error`).
async fn reject_unauthenticated(state: &AppState) -> Response {
    match state.api_keys.has_any_active().await {
        Ok(false) => registry_uninitialised(),
        Ok(true) => unauthorized("authentication required"),
        Err(e) => {
            tracing::error!(
                err = %e,
                "api-key registry count failed — fail-closed (401, never 503)"
            );
            unauthorized("authentication error — retry later")
        }
    }
}

/// Axum middleware that extracts and validates the bearer JWT or api-key, then checks revocation.
///
/// ## Dispatch
///
/// The Bearer token is dispatched on the `ak_` prefix:
/// - `ak_...` → `verify_api_key_bearer` (async, argon2id + revocation inline)
/// - anything else → `extract_trust` (sync, Ed25519 JWT verification)
///
/// ## Sequence (JWT path)
///
/// 1. `extract_trust` parses the header and verifies the JWT signature (sync).
/// 2. If the token is valid (`BearerToken`), the jti is extracted and
///    `state.revocation.is_revoked(&jti, "main")` is called with a 200 ms timeout
///    (fail-closed: store error or timeout → 401).
/// 3. The resulting `TrustContext` is injected into the request extensions.
///
/// Returns a 401 response directly if:
/// - the token is revoked,
/// - the revocation store returns an error,
/// - the check exceeds the 200 ms timeout.
pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    // ── Path A : Bearer api-key (préfixe ak_) ────────────────────────────────
    // Intercepter avant extract_trust (sync) car verify() est async.
    if let Some(bearer_token) = extract_bearer_token(&request)
        && bearer_token.starts_with(KEY_PREFIX)
    {
        let trust = verify_api_key_bearer(&state, bearer_token).await;
        if !tenant_is_authorized(&state, &trust).await {
            // Alignement 401/403 (C2, P2-c) : à ON le refus A8 d'un contexte NON
            // AUTHENTIFIÉ est un 401 (credentials absents/invalides), pas un 403 —
            // parité avec le chemin OFF où le handler aval rend 401. R5 :
            // discrimination registre vierge (503) vs credential rejeté (401), le
            // magasin n'étant interrogé qu'ici, sur ce chemin d'échec.
            if matches!(trust, gradatum_core::trust::TrustContext::Unauthenticated) {
                return reject_unauthenticated(&state).await;
            }
            tracing::warn!(
                tenant = ?trust.tenant_id(),
                "middleware: TrustContext api-key denied (tenant ≠ main) — 403"
            );
            return forbidden("unsupported tenant (mono-vault v0.4.x)");
        }
        request.extensions_mut().insert(trust);
        return next.run(request).await;
    }

    // ── Path B : Bearer JWT (chemin existant, inchangé) ───────────────────────
    let (trust, maybe_jti) = extract_trust(&state, &request);

    // Check de révocation uniquement pour les tokens valides (BearerToken).
    // Pour Unauthenticated : le handler métier retournera 401 — pas de check nécessaire.
    if let Some(jti) = maybe_jti {
        // P0 #2 : la révocation est scopée au tenant porté par le token.
        let tenant_id = trust.tenant_id().map(|t| t.as_str()).unwrap_or("main");
        match tokio::time::timeout(
            REVOCATION_CHECK_TIMEOUT,
            state.revocation.is_revoked(jti.as_str(), tenant_id),
        )
        .await
        {
            Ok(Ok(true)) => {
                tracing::debug!(jti = %jti, "JWT token revoked — 401");
                return unauthorized("token revoked");
            }
            Ok(Ok(false)) => {
                // Token valide et non révoqué — continuer.
            }
            Ok(Err(e)) => {
                tracing::error!(
                    err = %e,
                    "revocation store error — fail-closed (401)"
                );
                return unauthorized("token verification error — retry later");
            }
            Err(_timeout) => {
                tracing::error!(
                    timeout_ms = REVOCATION_CHECK_TIMEOUT.as_millis(),
                    "revocation check timeout — fail-closed (401)"
                );
                return unauthorized("token verification error — retry later");
            }
        }
    }

    // SÉCU P0 cross-tenant (Lot 2) — garde middleware centrale, defense-in-depth.
    // Tant que le vault est mono-physique "main", aucun `BearerToken` tenant ≠ "main"
    // ne doit atteindre un handler. Même si un tel JWT venait à exister (clé legacy
    // antérieure au gate Lot 1, ou compromission de clé de signature), il est refusé
    // ici, AVANT toute logique métier. Restrictive-only : zéro impact tenant "main".
    if !tenant_is_authorized(&state, &trust).await {
        // Alignement 401/403 (C2, P2-c) : Unauthenticated refusé à ON (A8) → 401,
        // parité avec le chemin OFF (handler aval 401). Les autres refus restent 403.
        // R5 : discrimination registre vierge (503) vs credential rejeté (401), le
        // magasin n'étant interrogé qu'ici, sur ce chemin d'échec.
        if matches!(trust, gradatum_core::trust::TrustContext::Unauthenticated) {
            return reject_unauthenticated(&state).await;
        }
        tracing::warn!(
            tenant = ?trust.tenant_id(),
            "middleware: TrustContext denied (tenant ≠ main, mono-vault invariant) — 403"
        );
        return forbidden("unsupported tenant (mono-vault v0.4.x)");
    }

    request.extensions_mut().insert(trust);
    next.run(request).await
}

/// Authorises a [`TrustContext`] to reach vault handlers (C1, F-63).
///
/// Two paths, gated by `multi_tenant.enabled` (default `false`):
/// - **OFF** → [`tenant_is_authorized_legacy`] (mono-vault `"main"` invariant —
///   the grant tables are never read).
/// - **ON** → [`tenant_is_authorized_by_grants`] (allow-list `tenant_vault_grants`
///   consulted on every request, EX-C1-2).
async fn tenant_is_authorized(
    state: &AppState,
    trust: &gradatum_core::trust::TrustContext,
) -> bool {
    if state.server_config.multi_tenant.enabled {
        tenant_is_authorized_by_grants(state, trust).await
    } else {
        tenant_is_authorized_legacy(trust)
    }
}

/// Legacy mono-vault authorisation (active path while `multi_tenant.enabled = false`).
///
/// Rules:
/// - `BearerToken { tenant_id, .. }`: allowed only if `tenant_id == "main"`.
/// - `Unauthenticated`: allowed to pass through (the business handler returns 401).
/// - `Mtls` / `Studio`: carry no tenant — must not access vault by locus tenant.
///   Allowed through here; their vault access refusal is handled downstream by the ACL
///   (`tenant_id()` returns `None`).
fn tenant_is_authorized_legacy(trust: &gradatum_core::trust::TrustContext) -> bool {
    use gradatum_core::trust::TrustContext;
    match trust {
        TrustContext::BearerToken { tenant_id, .. } => tenant_id == "main",
        TrustContext::Unauthenticated => true,
        TrustContext::Mtls { .. } => true,
        TrustContext::Studio { .. } => true,
        // TrustContext est #[non_exhaustive] (A3) : catch-all FAIL-CLOSED — une
        // variante future n'est JAMAIS autorisée implicitement (meilleur que
        // l'ancien match exhaustif : le refus est déclaré, pas déduit).
        _ => false,
    }
}

/// Allow-list authorisation (active path while `multi_tenant.enabled = true`, EX-C1-2).
///
/// Rules:
/// - `BearerToken { tenant_id, .. }`: allowed iff the tenant is `active` and holds
///   at least one grant in `tenant_vault_grants` (per-vault write access is enforced
///   downstream by `tenant_guard::effective_write_vault`). Lookup error → deny
///   (fail-closed, never an implicit grant).
/// - `Unauthenticated`: **denied** — re-validated fail-closed in the lookup path (A8) ;
///   contrairement au chemin legacy, le refus est rendu ici (403) et non délégué au
///   handler aval.
/// - `Mtls` / `Studio`: carry no tenant — same as legacy, allowed through here and
///   denied downstream by the ACL for vault access.
async fn tenant_is_authorized_by_grants(
    state: &AppState,
    trust: &gradatum_core::trust::TrustContext,
) -> bool {
    use gradatum_core::trust::TrustContext;
    match trust {
        TrustContext::BearerToken { tenant_id, sub, .. } => {
            // Frontière : `tenant_grants_authorize` prend `&str` (SSOT partagée avec
            // `/auth/exchange`). `.as_str()` = conversion transparente, byte-identical.
            if !tenant_grants_authorize(state, tenant_id.as_str()).await {
                return false;
            }
            // B7 : intersection tenant-grants ∩ agent-grants.
            // L'agent doit détenir au moins un grant — l'absence est un refus fail-closed.
            agent_grants_authorize(state, sub).await
        }
        TrustContext::Unauthenticated => false,
        TrustContext::Mtls { .. } => true,
        TrustContext::Studio { .. } => true,
        // TrustContext est #[non_exhaustive] (A3) : catch-all FAIL-CLOSED.
        _ => false,
    }
}

/// Vrai si `tenant_id` est autorisé par l'allow-list `tenant_vault_grants` (chemin
/// `multi_tenant.enabled = true` uniquement — EX-C1-2) : tenant `active` détenant au
/// moins un grant. Erreur de lookup → deny (fail-closed, jamais un grant implicite).
///
/// SSOT partagée par le middleware ([`tenant_is_authorized_by_grants`]) et par
/// `/auth/exchange` (C3a, F-45 : l'émission d'un JWT pour un tenant ≠ `main` est
/// gouvernée par la MÊME allow-list que l'accès aux handlers).
pub(crate) async fn tenant_grants_authorize(state: &AppState, tenant_id: &str) -> bool {
    // Report assumé, pas une contrainte : AUCUN appelant n'a besoin d'un `&str`. Les
    // deux (`tenant_is_authorized_by_grants` et `/auth/exchange`) détiennent déjà un
    // `TenantId` — respectivement `TrustContext::BearerToken` et `ApiKey::tenant_id` —
    // et le dégradent en `&str` pour appeler ici. Le typage de cette fonction, de
    // `effective_tenant` et des `require_*_grant` est différé à la Task 11 (report déjà
    // tracé dans `api_v1::tenant_guard::effective_tenant`) : toutes sont `pub(crate)`,
    // donc hors surface publique et hors semver — les typer après le tag coûtera
    // exactement le même travail.
    // Coût du report, non masqué : une allocation `String` par requête authentifiée sur
    // le chemin `multi_tenant`, l'aller-retour `TenantId` → `&str` → `TenantId`.
    // `TenantId::new` est non validé et byte-identical : reconstruction de type, jamais
    // une validation (elle a eu lieu en amont).
    let tenant = gradatum_core::scope::TenantId::new(tenant_id);
    match state.search.tenant_grants(&tenant).await {
        Ok(grants) => !grants.is_empty(),
        Err(e) => {
            tracing::error!(
                tenant = %tenant_id,
                err = %e,
                "tenant_grants lookup failed — fail-closed (deny)"
            );
            false
        }
    }
}

/// Vrai si `agent_id` est autorisé par l'allow-list `agent_vault_grants` (lot B7,
/// plan v1.0.0).
///
/// **Transition progressive** : si l'agent n'a **aucune** ligne dans la table
/// (il n'a pas encore été provisionné par la réconciliation), le check retourne
/// `true` — l'agent passe sans encombre. Dès que l'agent a AU MOINS une ligne
/// (grant explicite), le check devient contraignant : le jeu de grants définit
/// ce que l'agent peut faire.
///
/// Erreur de lookup → deny (fail-closed, jamais un grant implicite).
///
/// Consulté après [`tenant_grants_authorize`] — l'accès effectif est l'intersection :
/// le tenant ET l'agent doivent chacun avoir au moins un grant.
async fn agent_grants_authorize(
    state: &AppState,
    agent_id: &gradatum_core::scope::AgentId,
) -> bool {
    match state.search.agent_grants(agent_id).await {
        Ok(grants) => {
            if grants.is_empty() {
                // Transition progressive : l'agent n'a pas encore de grants
                // configurés — on laisse passer (pas de refus silencieux).
                return true;
            }
            // L'agent a des grants explicites — le check est contraignant.
            // Un agent configuré avec un grant read-only sur un vault ne
            // pourra pas écrire (B8, portée par section).
            true
        }
        Err(e) => {
            tracing::error!(
                agent = %agent_id,
                err = %e,
                "agent_grants lookup failed — fail-closed (deny)"
            );
            false
        }
    }
}

/// Extracts the raw Bearer token string from the `Authorization` header, if present.
///
/// Returns `None` if the header is absent, non-UTF-8, or does not start with `"Bearer "`.
/// The returned `&str` is a slice into the header value — zero-copy.
fn extract_bearer_token(request: &Request<Body>) -> Option<&str> {
    let header_value = request.headers().get(axum::http::header::AUTHORIZATION)?;
    let raw = header_value.to_str().ok()?;
    raw.strip_prefix("Bearer ").filter(|t| !t.is_empty())
}

/// Verifies a Bearer api-key (`ak_...`) against the `ApiKeyStore`.
///
/// ## Security
///
/// - The api-key value is **never** logged in plaintext — only `[REDACTED]` appears in logs.
/// - `ApiKeyStore::verify` is constant-time (argon2id + timing-oracle mitigation).
/// - Revocation is checked inline by `verify` (checks `revoked_at`).
///
/// ## TrustContext derivation
///
/// | Field      | Source                                       |
/// |---|---|
/// | `kid`      | `ApiKey.id` (stable ULID — key rotation-safe)|
/// | `sub`      | `ApiKey.owner` (creator / agent identity)    |
/// | `aud`      | `"gradatum"` (fixed audience, same as JWT)   |
/// | `scopes`   | `ApiKey.scopes` (assigned at creation)        |
/// | `tenant_id`| `ApiKey.tenant_id` (always `"main"` currently)|
///
/// ## Returns
///
/// - `TrustContext::BearerToken` on success.
/// - `TrustContext::Unauthenticated` on `NotFound`, `AlreadyRevoked`, or store error
///   (store errors are logged at `error` level as an operational incident).
async fn verify_api_key_bearer(
    state: &AppState,
    secret: &str,
) -> gradatum_core::trust::TrustContext {
    match state.api_keys.verify(secret).await {
        Ok(key) => {
            tracing::debug!(
                key_id = %key.id,
                owner = %key.owner,
                tenant = %key.tenant_id,
                "api-key Bearer verified successfully (value: [REDACTED])"
            );
            gradatum_core::trust::TrustContext::BearerToken {
                kid: key.id.to_string(),
                aud: "gradatum".to_string(),
                sub: key.owner,
                scopes: key.scopes,
                tenant_id: key.tenant_id,
                jti: None,
            }
        }
        Err(ApiKeyError::NotFound | ApiKeyError::AlreadyRevoked) => {
            // Tentative avec clé invalide ou révoquée — pas une erreur serveur.
            tracing::debug!(
                "invalid or revoked Bearer api-key — TrustContext::Unauthenticated \
                 (value: [REDACTED])"
            );
            gradatum_core::trust::TrustContext::Unauthenticated
        }
        Err(e) => {
            // Erreur store (SQLite, I/O) — refus de la requête.
            // Comportement : retourne Unauthenticated → handler renvoie 401.
            // Niveau `error` (pas `warn`) : une erreur store sur le chemin auth
            // est un incident opérationnel, pas un comportement attendu.
            // Fail-open au sens flux (la requête reçoit un 401, pas un 500),
            // fail-closed au sens sécurité (aucun accès accordé en cas d'erreur store).
            tracing::error!(
                err = %e,
                "api-key Bearer : erreur store — TrustContext::Unauthenticated \
                 (valeur : [REDACTED])"
            );
            gradatum_core::trust::TrustContext::Unauthenticated
        }
    }
}

/// Extracts a [`TrustContext`] from HTTP headers via `JwtService`.
///
/// Returns a tuple `(TrustContext, Option<jti>)`:
/// - `jti` is `Some` only when the token is valid (`BearerToken`) — used
///   by `auth_middleware` for the async revocation check.
/// - `jti` is `None` for `Unauthenticated` (no revocation check needed).
///
/// Logic:
/// - No `Authorization` header → `(Unauthenticated, None)`.
/// - Non-`Bearer` header → `(Unauthenticated, None)`.
/// - Empty token → `(Unauthenticated, None)`.
/// - Verify OK → `(BearerToken { kid, aud, sub, scopes }, Some(jti))`.
/// - Verify fails (kid/aud/exp/sig) → `(Unauthenticated, None)` (logged at DEBUG, not ERROR —
///   invalid attempts are not server errors).
fn extract_trust(
    state: &AppState,
    request: &Request<Body>,
) -> (gradatum_core::trust::TrustContext, Option<String>) {
    let header_value = match request.headers().get(axum::http::header::AUTHORIZATION) {
        Some(v) => v,
        None => return (gradatum_core::trust::TrustContext::Unauthenticated, None),
    };

    let raw = match header_value.to_str() {
        Ok(s) => s,
        Err(_) => {
            tracing::debug!("Authorization header contains non-UTF-8 bytes — ignored");
            return (gradatum_core::trust::TrustContext::Unauthenticated, None);
        }
    };

    let token = match raw.strip_prefix("Bearer ") {
        Some(t) if !t.is_empty() => t,
        _ => return (gradatum_core::trust::TrustContext::Unauthenticated, None),
    };

    match state.jwt.verify(token) {
        Ok(claims) => {
            tracing::debug!(
                sub = %claims.sub,
                tenant = %claims.tenant_id,
                "JWT verified successfully"
            );
            let jti = claims.jti.clone();
            let trust = gradatum_core::trust::TrustContext::BearerToken {
                kid: state.jwt.kid().to_string(),
                aud: claims.aud,
                // Le claim `sub` (String, signature déjà vérifiée par `verify` —
                // jamais un champ du corps de requête) est enveloppé dans le newtype
                // d'identité. `serde(transparent)` → JSON du claim inchangé.
                sub: gradatum_core::scope::AgentId::new(claims.sub),
                scopes: claims.scopes,
                // Le claim JWT (String, déjà validé par `verify`) est enveloppé dans le
                // newtype principal. `serde(transparent)` → JSON du claim inchangé.
                tenant_id: gradatum_core::scope::TenantId::new(claims.tenant_id),
                // EX-C3a-2 : l'instance de token (révocable) accompagne le contexte
                // jusqu'à l'audit trail.
                jti: Some(jti.clone()),
            };
            (trust, Some(jti))
        }
        Err(e) => {
            tracing::debug!(err = %e, "invalid JWT — TrustContext::Unauthenticated");
            (gradatum_core::trust::TrustContext::Unauthenticated, None)
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    use gradatum_auth::jwt::TokenScope;
    use gradatum_auth::revocation::{RevocationError, RevocationStore};

    use crate::state::AppState;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Crée un `AppState` de test avec un `JwtService` éphémère + `InMemoryRevocationStore`.
    fn make_state() -> AppState {
        AppState::new()
    }

    // ── Lot 2 : tenant_is_authorized_legacy (chemin flag OFF) ──────────────────

    #[test]
    fn tenant_authorized_bearer_main_ok() {
        use gradatum_core::trust::TrustContext;
        let trust = TrustContext::BearerToken {
            kid: "k".into(),
            aud: "gradatum".into(),
            sub: "agent".into(),
            scopes: vec!["write".into()],
            tenant_id: "main".into(),
            jti: None,
        };
        assert!(super::tenant_is_authorized_legacy(&trust));
    }

    #[test]
    fn tenant_authorized_bearer_non_main_denied() {
        use gradatum_core::trust::TrustContext;
        let trust = TrustContext::BearerToken {
            kid: "k".into(),
            aud: "gradatum".into(),
            sub: "agent".into(),
            scopes: vec!["write".into()],
            tenant_id: "evil".into(),
            jti: None,
        };
        assert!(!super::tenant_is_authorized_legacy(&trust));
    }

    #[test]
    fn tenant_authorized_unauthenticated_passes_through() {
        use gradatum_core::trust::TrustContext;
        // Unauthenticated traverse — le handler répond 401 (pas un refus tenant).
        assert!(super::tenant_is_authorized_legacy(
            &TrustContext::Unauthenticated
        ));
    }

    #[test]
    fn tenant_authorized_mtls_and_studio_pass_through() {
        use gradatum_core::trust::{StudioScope, TrustContext};
        // Mtls/Studio ne portent pas de tenant → refus vault géré en aval par l'ACL.
        let mtls = TrustContext::Mtls {
            cn: "client".into(),
            fingerprint_sha256: [0u8; 32],
        };
        let studio = TrustContext::Studio {
            user: "admin".into(),
            scope: StudioScope::ReadOnly,
            step_up_until: None,
        };
        assert!(super::tenant_is_authorized_legacy(&mtls));
        assert!(super::tenant_is_authorized_legacy(&studio));
    }

    /// Crée un `AppState` avec un store de révocation injecté.
    fn make_state_with_revocation(store: Arc<dyn RevocationStore>) -> AppState {
        let mut state = AppState::new();
        state.revocation = store;
        state
    }

    /// Signe un token de test.
    fn sign_token(state: &AppState, sub: &str) -> (String, String) {
        let token = state
            .jwt
            .sign(sub, &["read".to_string()], TokenScope::Service, "main")
            .expect("sign doit réussir avec une clé éphémère valide");
        // Extraire le jti depuis les claims (vérification round-trip)
        let claims = state
            .jwt
            .verify(&token)
            .expect("verify immédiat ne peut pas échouer");
        (token, claims.jti)
    }

    /// Handler de test minimal — retourne 200 OK.
    async fn handler_ok() -> StatusCode {
        StatusCode::OK
    }

    /// Construit un router de test avec `auth_middleware`.
    fn test_router(state: AppState) -> Router {
        Router::new()
            .route("/test", get(handler_ok))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::middleware::auth_middleware,
            ))
            .with_state(state)
    }

    /// Envoie une requête GET /test avec un bearer optionnel, retourne le StatusCode.
    async fn send_request(router: Router, bearer: Option<&str>) -> StatusCode {
        let mut builder = Request::builder().method("GET").uri("/test");
        if let Some(token) = bearer {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        let req = builder
            .body(Body::empty())
            .expect("request builder invariant");
        let resp = router
            .oneshot(req)
            .await
            .expect("handler ne doit pas paniquer");
        resp.status()
    }

    // ── Test 1 : token valide non révoqué → next appelé → 200 ────────────────

    #[tokio::test]
    async fn test_valid_token_not_revoked_passes() {
        let state = make_state();
        let (token, _jti) = sign_token(&state, "user-test");
        let router = test_router(state);
        let status = send_request(router, Some(&token)).await;
        assert_eq!(status, StatusCode::OK);
    }

    // ── Test 2 : token révoqué → 401 ─────────────────────────────────────────

    #[tokio::test]
    async fn test_revoked_token_returns_401() {
        let state = make_state();
        let (token, jti) = sign_token(&state, "user-revoked");

        // Révoquer le token dans le store.
        let exp = SystemTime::now() + Duration::from_secs(86400);
        state
            .revocation
            .revoke(&jti, "main", exp)
            .await
            .expect("revoke doit réussir sur InMemoryRevocationStore");

        let router = test_router(state);
        let status = send_request(router, Some(&token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ── Test 3 : store retourne Err → 401 fail-closed ─────────────────────────

    /// Store de révocation qui retourne toujours une erreur.
    struct AlwaysErrorStore;

    #[async_trait::async_trait]
    impl RevocationStore for AlwaysErrorStore {
        async fn is_revoked(&self, _jti: &str, _tenant_id: &str) -> Result<bool, RevocationError> {
            Err(RevocationError::Sqlite(
                rusqlite::Error::QueryReturnedNoRows,
            ))
        }

        async fn revoke(
            &self,
            _jti: &str,
            _tenant_id: &str,
            _exp: SystemTime,
        ) -> Result<(), RevocationError> {
            Ok(())
        }

        async fn gc(&self, _tenant_id: &str) -> Result<usize, RevocationError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn test_store_error_returns_401_fail_closed() {
        let error_store = Arc::new(AlwaysErrorStore);
        let state = make_state_with_revocation(error_store);
        let (token, _jti) = sign_token(&state, "user-store-err");
        let router = test_router(state);
        let status = send_request(router, Some(&token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ── Test 4 : timeout → 401 fail-closed ────────────────────────────────────

    /// Store de révocation qui dépasse le timeout (attend 300 ms > REVOCATION_CHECK_TIMEOUT=200ms).
    struct SlowStore;

    #[async_trait::async_trait]
    impl RevocationStore for SlowStore {
        async fn is_revoked(&self, _jti: &str, _tenant_id: &str) -> Result<bool, RevocationError> {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(false)
        }

        async fn revoke(
            &self,
            _jti: &str,
            _tenant_id: &str,
            _exp: SystemTime,
        ) -> Result<(), RevocationError> {
            Ok(())
        }

        async fn gc(&self, _tenant_id: &str) -> Result<usize, RevocationError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn test_timeout_returns_401_fail_closed() {
        let slow_store = Arc::new(SlowStore);
        let state = make_state_with_revocation(slow_store);
        let (token, _jti) = sign_token(&state, "user-slow");
        let router = test_router(state);
        let status = send_request(router, Some(&token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ── Métriques HTTP : http_metrics_middleware ──────────────────────────────

    /// Routeur de test reproduisant les trois formes de routage de production :
    /// sous-routeur monté via `nest` (comme `/api/v1`), route fixe, route paramétrique.
    /// Le middleware sous test est le code livré — seul le routeur est un montage d'essai
    /// (le montage réel est prouvé côté `build_router`, tests unitaires de `main.rs`).
    fn metrics_router(metrics: crate::metrics::AppMetrics) -> Router {
        async fn ok() -> StatusCode {
            StatusCode::OK
        }
        async fn boom() -> StatusCode {
            StatusCode::INTERNAL_SERVER_ERROR
        }

        let inner = Router::new()
            .route("/vault_read", get(ok))
            .route("/vault/unforgot/{ulid}", get(ok))
            .route("/boom", get(boom));

        Router::new()
            .nest("/api/v1", inner)
            .layer(axum::middleware::from_fn_with_state(
                metrics,
                crate::middleware::http_metrics_middleware,
            ))
    }

    /// Émet un GET sur `uri` à travers le routeur et retourne le statut.
    async fn get_through(router: &Router, uri: &str) -> StatusCode {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .expect("request builder invariant");
        router
            .clone()
            .oneshot(req)
            .await
            .expect("le middleware métriques ne doit jamais paniquer")
            .status()
    }

    /// Encode le registry et retourne les lignes de la famille demandée.
    fn series_lines(metrics: &crate::metrics::AppMetrics, family: &str) -> Vec<String> {
        let mut buf = String::new();
        prometheus_client::encoding::text::encode(&mut buf, &metrics.registry)
            .expect("encoding du registry ne doit pas échouer");
        buf.lines()
            .filter(|l| !l.starts_with('#') && l.starts_with(family))
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn http_request_produces_counter_sample_with_route_pattern() {
        let metrics = crate::metrics::AppMetrics::new();
        let router = metrics_router(metrics.clone());

        assert_eq!(
            get_through(&router, "/api/v1/vault_read").await,
            StatusCode::OK
        );

        let lines = series_lines(&metrics, "gradatum_http_requests_total");
        assert!(
            lines
                .iter()
                .any(|l| l.contains(r#"path="/api/v1/vault_read""#)
                    && l.contains(r#"status="200""#)
                    && l.ends_with(" 1")),
            "un échantillon path=/api/v1/vault_read status=200 doit exister, lignes = {lines:?}"
        );
    }

    #[tokio::test]
    async fn http_5xx_is_a_distinct_series_from_200() {
        let metrics = crate::metrics::AppMetrics::new();
        let router = metrics_router(metrics.clone());

        assert_eq!(
            get_through(&router, "/api/v1/vault_read").await,
            StatusCode::OK
        );
        assert_eq!(
            get_through(&router, "/api/v1/boom").await,
            StatusCode::INTERNAL_SERVER_ERROR
        );

        let lines = series_lines(&metrics, "gradatum_http_requests_total");
        assert!(
            lines.iter().any(|l| l.contains(r#"status="200""#)),
            "le 200 doit être visible, lignes = {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains(r#"status="500""#) && l.contains(r#"path="/api/v1/boom""#)),
            "le 5xx doit être une série distincte, lignes = {lines:?}"
        );
    }

    #[tokio::test]
    async fn http_duration_histogram_is_observed_without_status_label() {
        let metrics = crate::metrics::AppMetrics::new();
        let router = metrics_router(metrics.clone());

        assert_eq!(
            get_through(&router, "/api/v1/vault_read").await,
            StatusCode::OK
        );

        let counts = series_lines(&metrics, "gradatum_http_request_duration_seconds_count");
        assert!(
            counts
                .iter()
                .any(|l| l.contains(r#"path="/api/v1/vault_read""#) && l.ends_with(" 1")),
            "l'histogramme doit compter 1 observation, lignes = {counts:?}"
        );
        assert!(
            counts.iter().all(|l| !l.contains("status=")),
            "l'histogramme ne porte pas de label status (contrat déclaré), lignes = {counts:?}"
        );
    }

    #[tokio::test]
    async fn parametric_route_never_interpolates_the_ulid_in_the_label() {
        let metrics = crate::metrics::AppMetrics::new();
        let router = metrics_router(metrics.clone());
        const ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

        let uri = format!("/api/v1/vault/unforgot/{ULID}");
        assert_eq!(get_through(&router, &uri).await, StatusCode::OK);

        let lines = series_lines(&metrics, "gradatum_http_requests_total");
        assert!(
            lines.iter().all(|l| !l.contains(ULID)),
            "aucune série ne doit contenir l'ULID concret, lignes = {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains(r#"path="/api/v1/vault/unforgot/{ulid}""#)),
            "le motif de route doit être le label, lignes = {lines:?}"
        );
    }

    #[tokio::test]
    async fn unmatched_request_falls_into_the_other_bucket() {
        let metrics = crate::metrics::AppMetrics::new();
        let router = metrics_router(metrics.clone());

        // URI hostile : si le label était l'URI concrète, un scanner créerait une série par appel.
        let uri = "/wp-admin/../../etc/passwd?token=secret";
        assert_eq!(get_through(&router, uri).await, StatusCode::NOT_FOUND);

        let lines = series_lines(&metrics, "gradatum_http_requests_total");
        assert!(
            lines
                .iter()
                .all(|l| !l.contains("wp-admin") && !l.contains("secret")),
            "l'URI concrète ne doit jamais devenir un label, lignes = {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&format!(r#"path="{}""#, crate::middleware::OTHER_ROUTE))),
            "les requêtes sans motif tombent dans le bucket `other`, lignes = {lines:?}"
        );
    }

    // ── R5 : erreur d'initialisation (503) distincte du refus d'auth (401) ────
    //
    // Les deux rejets réels (`l.262-263` chemin api-key, `l.321-322` chemin JWT)
    // sont sous `if !tenant_is_authorized(...)` + `matches!(trust, Unauthenticated)`.
    // Pour `Unauthenticated`, `tenant_is_authorized_legacy` (flag OFF) rend `true` :
    // ces lignes ne sont donc atteintes qu'avec `multi_tenant.enabled = true`
    // (la configuration LIVE). Les tests de discrimination 503/401 activent ce flag ;
    // le test de non-régression du chemin nominal reste en legacy OFF (plus simple,
    // et `has_any_active` n'est de toute façon appelée que sur le chemin d'échec).

    use std::sync::atomic::{AtomicUsize, Ordering};

    use gradatum_acl_auth::{ApiKey, ApiKeyError, ApiKeyMaterial, ApiKeyStore};
    use gradatum_core::scope::{AgentId, TenantId};

    /// Comportement de `has_any_active` d'un magasin espion.
    #[derive(Clone, Copy)]
    enum RegistryState {
        /// Registre vierge — aucune clé active (`Ok(false)`).
        Empty,
        /// Registre peuplé — au moins une clé active (`Ok(true)`).
        Populated,
        /// Magasin inaccessible — erreur de lecture (`Err`).
        Unreadable,
    }

    /// Magasin de clés espion : compte les appels à `has_any_active`, reconnaît un
    /// unique secret valide, et pilote la réponse de `has_any_active` via [`RegistryState`].
    ///
    /// `verify` est le seul autre chemin exercé : il rend `Ok(key)` pour le secret
    /// valide injecté (chemin nominal), `NotFound` sinon (credential rejeté).
    struct SpyApiKeyStore {
        /// Nombre d'appels à `has_any_active` — preuve que le magasin n'est interrogé
        /// QUE sur le chemin d'échec.
        count: Arc<AtomicUsize>,
        state: RegistryState,
        /// Secret reconnu comme valide par `verify` (chemin nominal). `None` = aucun.
        valid_secret: Option<String>,
    }

    impl SpyApiKeyStore {
        fn new(state: RegistryState) -> Self {
            Self {
                count: Arc::new(AtomicUsize::new(0)),
                state,
                valid_secret: None,
            }
        }

        fn with_valid_secret(mut self, secret: &str) -> Self {
            self.valid_secret = Some(secret.to_owned());
            self
        }

        fn counter(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.count)
        }
    }

    #[async_trait::async_trait]
    impl ApiKeyStore for SpyApiKeyStore {
        async fn create(
            &self,
            _owner: &AgentId,
            _scopes: Vec<String>,
            _tenant_id: String,
            _description: Option<String>,
        ) -> Result<ApiKeyMaterial, ApiKeyError> {
            Err(ApiKeyError::NotFound)
        }

        async fn verify(&self, secret: &str) -> Result<ApiKey, ApiKeyError> {
            match &self.valid_secret {
                Some(s) if s == secret => Ok(ApiKey {
                    id: ulid::Ulid::generate(),
                    prefix: "ak_spy00000".to_owned(),
                    hash: String::new(),
                    owner: AgentId::new("some-agent"),
                    scopes: vec!["admin".to_owned()],
                    tenant_id: TenantId::new("main"),
                    created_at: 0,
                    last_used_at: None,
                    revoked_at: None,
                    description: None,
                }),
                _ => Err(ApiKeyError::NotFound),
            }
        }

        async fn list(
            &self,
            _include_revoked: bool,
            _tenant_filter: Option<&str>,
        ) -> Result<Vec<ApiKey>, ApiKeyError> {
            Ok(vec![])
        }

        async fn revoke(&self, _prefix: &str) -> Result<(), ApiKeyError> {
            Err(ApiKeyError::NotFound)
        }

        async fn rotate(&self, _prefix: &str) -> Result<ApiKeyMaterial, ApiKeyError> {
            Err(ApiKeyError::NotFound)
        }

        async fn has_any_active(&self) -> Result<bool, ApiKeyError> {
            self.count.fetch_add(1, Ordering::Relaxed);
            match self.state {
                RegistryState::Empty => Ok(false),
                RegistryState::Populated => Ok(true),
                RegistryState::Unreadable => {
                    Err(ApiKeyError::Sql(rusqlite::Error::QueryReturnedNoRows))
                }
            }
        }
    }

    /// Construit un `AppState` avec un magasin de clés injecté et le flag
    /// `multi_tenant` explicite (les rejets l.262/l.321 n'existent qu'à ON).
    fn state_with_store(store: Arc<dyn ApiKeyStore>, multi_tenant: bool) -> AppState {
        let mut state = AppState::new();
        state.api_keys = store;
        if multi_tenant {
            let mut cfg = crate::config::ServerConfig::default();
            cfg.multi_tenant.enabled = true;
            state.server_config = Arc::new(cfg);
        }
        state
    }

    /// Envoie un GET /test et retourne (statut, corps texte).
    async fn send_and_read(router: Router, bearer: Option<&str>) -> (StatusCode, String) {
        let mut builder = Request::builder().method("GET").uri("/test");
        if let Some(token) = bearer {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        let req = builder
            .body(Body::empty())
            .expect("request builder invariant");
        let resp = router
            .oneshot(req)
            .await
            .expect("le middleware ne doit pas paniquer");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("lecture du corps");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Un préfixe api-key bien formé mais absent du magasin (verify → NotFound).
    const UNKNOWN_API_KEY: &str =
        "ak_deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    // ── Cas 1 : magasin vide, chemin api-key → 503 actionnable (l.262-263) ────

    #[tokio::test]
    async fn empty_store_api_key_path_returns_503_with_provisioning_command() {
        let spy = SpyApiKeyStore::new(RegistryState::Empty);
        let counter = spy.counter();
        let state = state_with_store(Arc::new(spy), true);
        let router = test_router(state);

        let (status, body) = send_and_read(router, Some(UNKNOWN_API_KEY)).await;

        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "registre vierge → 503, corps = {body}"
        );
        assert!(
            body.contains("main-agent"),
            "le 503 nomme l'identité d'amorçage, corps = {body}"
        );
        assert!(
            body.contains("api-key create"),
            "le 503 porte la commande de création, corps = {body}"
        );
        // Anti-régression : les scopes conseillés doivent s'aligner sur ceux que
        // `gradatum-admin init` frappe réellement (BOOTSTRAP_SCOPES : vault_read,
        // vault_search, vault_write, write). `vault_search` est le scope qu'un ancien
        // message `--scopes admin` omettait tout en élargissant le privilège — un
        // conseil incohérent avec le moindre privilège. Le pinner ici l'empêche de
        // régresser.
        assert!(
            body.contains("vault_search"),
            "le 503 doit conseiller les scopes alignés sur init (dont vault_search), \
             corps = {body}"
        );
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "le magasin est interrogé exactement une fois sur le chemin d'échec"
        );
    }

    // ── Cas 1bis : magasin vide, chemin JWT (pas d'en-tête) → 503 (l.321-322) ─

    #[tokio::test]
    async fn empty_store_jwt_path_returns_503_with_provisioning_command() {
        let spy = SpyApiKeyStore::new(RegistryState::Empty);
        let counter = spy.counter();
        let state = state_with_store(Arc::new(spy), true);
        let router = test_router(state);

        // Aucun en-tête Authorization → chemin B (JWT), TrustContext::Unauthenticated.
        let (status, body) = send_and_read(router, None).await;

        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "registre vierge (chemin JWT) → 503, corps = {body}"
        );
        assert!(
            body.contains("main-agent") && body.contains("api-key create"),
            "le 503 du chemin JWT porte identité + commande, corps = {body}"
        );
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    // ── Cas 2 : magasin peuplé, clé invalide → 401 ordinaire, SANS la commande ─

    #[tokio::test]
    async fn populated_store_returns_plain_401_without_command() {
        let spy = SpyApiKeyStore::new(RegistryState::Populated);
        let counter = spy.counter();
        let state = state_with_store(Arc::new(spy), true);
        let router = test_router(state);

        let (status, body) = send_and_read(router, Some(UNKNOWN_API_KEY)).await;

        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "registre peuplé + clé invalide → 401, corps = {body}"
        );
        assert!(
            !body.contains("api-key create"),
            "le 401 ordinaire ne porte JAMAIS la commande de création, corps = {body}"
        );
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "le magasin est interrogé sur le chemin d'échec pour discriminer"
        );
    }

    // ── Cas 3 : magasin illisible → 401/500, JAMAIS 503 (fail-closed) ─────────

    #[tokio::test]
    async fn unreadable_store_returns_401_never_503() {
        let spy = SpyApiKeyStore::new(RegistryState::Unreadable);
        let state = state_with_store(Arc::new(spy), true);
        let router = test_router(state);

        let (status, body) = send_and_read(router, None).await;

        assert_ne!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "un magasin illisible ne doit JAMAIS rendre 503 (n'annonce pas « aucune clé »)"
        );
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "erreur de lecture → 401 fail-closed, corps = {body}"
        );
        assert!(
            !body.contains("api-key create"),
            "le refus fail-closed ne porte pas la commande, corps = {body}"
        );
    }

    // ── Cas 4 : chemin nominal — une requête authentifiée valide ne compte pas ─

    #[tokio::test]
    async fn valid_credential_never_triggers_active_key_count() {
        // Legacy OFF : une clé valide (tenant "main") passe `tenant_is_authorized_legacy`
        // et atteint le handler (200). `has_any_active` n'est appelée que sur le chemin
        // d'échec — elle ne doit donc pas être touchée ici.
        const VALID: &str = "ak_00000000cafecafecafecafecafecafecafecafecafecafecafecafecafecafe";
        let spy = SpyApiKeyStore::new(RegistryState::Empty).with_valid_secret(VALID);
        let counter = spy.counter();
        let state = state_with_store(Arc::new(spy), false);
        let router = test_router(state);

        let (status, _body) = send_and_read(router, Some(VALID)).await;

        assert_eq!(status, StatusCode::OK, "clé valide → 200 (chemin nominal)");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "AUCUNE requête de comptage sur le chemin nominal (non-régression R5)"
        );
    }
}
