//! API interne server-to-worker (Wave 2, v0.5.3).
//!
//! ## Architecture
//!
//! Listener HTTP indépendant sur `127.0.0.1:19092` (loopback uniquement).
//! Route prefix : `/internal/v1/*`.
//!
//! ## Authentification
//!
//! Double guard :
//! 1. Rejet IP non-loopback (middleware `internal_auth_middleware`).
//! 2. Vérification constant-time du bearer token `X-Gradatum-Internal: Bearer <token>`
//!    via `subtle::ConstantTimeEq` (ANSSI R23).
//!
//! ## Isolation
//!
//! Le router interne N'EST JAMAIS fusionné au router public.
//! `spawn_internal_listener` démarre un `tokio::TcpListener` séparé.
//!
//! ## Inaccessibilité via Traefik/gateway — preuve (F-100 contrainte #1)
//!
//! Les endpoints admin (`delete`/`archives/*`) sont structurellement injoignables depuis
//! l'extérieur (Traefik, gateway Anthropic `:8436`) — triple barrière :
//!
//! 1. **Bind loopback strict** : le listener interne écoute `127.0.0.1:19092`, jamais
//!    `0.0.0.0`. Une adresse loopback n'est pas routable depuis un autre hôte/conteneur ;
//!    Traefik ne peut pas ouvrir de connexion vers le `127.0.0.1` de l'hôte interne.
//! 2. **Surface publique disjointe** : Traefik/gateway routent vers le port **public**
//!    (`:19090`), qui ne monte aucun routeur interne. `build_internal_router`
//!    n'est jamais mergé au router public (cf. Isolation) → aucun chemin `/internal/*`
//!    n'existe côté public (prouvé par le test structurel
//!    `mutations_absent_from_public_router_present_internal`).
//! 3. **Défense en profondeur** : même si un paquet atteignait `:19092`, le middleware
//!    admin rejette tout peer ≠ `127.0.0.1` AVANT toute logique métier (double guard
//!    ci-dessous), fail-closed si le token admin n'est pas configuré.
//!
//! ## Routes
//!
//! ### Persist
//! - `POST /internal/v1/persist/curated`  — pipeline 5 writes séquentiels
//! - `POST /internal/v1/persist/embedding` — stockage vecteur
//! - `POST /internal/v1/persist/forget`   — marquage oubli sémantique
//! - `POST /internal/v1/persist/distill`  — mise à jour note distillée
//!
//! ### Note
//! - `DELETE /internal/v1/note/{ulid}`     — suppression note (destruction, job Purge/GC)
//!
//! ### Admin (F-100 1.6 — token admin distinct `X-Gradatum-Admin`)
//! - `POST /internal/v1/admin/delete`          — delete on-demand = archivage (CLI opérateur)
//! - `POST /internal/v1/admin/archives/list`   — listing registre d'archives (CLI opérateur)
//! - `POST /internal/v1/admin/archives/purge`  — purge à la demande (dry-run + confirm)
//! - `POST /internal/v1/admin/archives/restore` — restauration en quarantaine (dry-run + confirm)
//!
//! ### Lecture
//! - `GET /internal/v1/note/{ulid}`              — note complète
//! - `GET /internal/v1/note/{ulid}/status`       — statut scopé par vault (index, TOCTOU purge)
//! - `GET /internal/v1/note/{ulid}/embedding`    — vecteur embedding
//! - `GET /internal/v1/note/{ulid}/trust`        — score trust
//! - `GET /internal/v1/title-lookup`             — lookup ULID par titre (H1 Markdown)
//! - `GET /internal/v1/id-lookup`                — lookup existence par ULID (ULID-first)
//! - `GET /internal/v1/notes/by-locus`           — notes par préfixe locus
//! - `GET /internal/v1/notes/count-unprocessed`  — comptage notes live non-processed (F-112)
//! - `GET /internal/v1/notes/by-status`          — notes par statut
//! - `GET /internal/v1/notes/garbage`            — notes Garbage expirées
//! - `GET /internal/v1/forget/search`            — FTS5 pour scope Topic forget
//! - `GET /internal/v1/notes/by-agent`           — notes par agent (scope Agent forget)

pub(crate) mod admin;
pub(crate) mod admin_auth;
pub(crate) mod auth;
pub(crate) mod persist;
pub(crate) mod reads;

use std::net::SocketAddr;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use tracing::info;

use crate::state::AppState;

/// Préfixe des vaults du registre de CODE (`code_vault`) — lot REG.
///
/// Un `vault_id` portant ce préfixe relève du registre dérivé de git (régénéré à chaque
/// refresh, hors lifecycle) et jamais du registre de données (`tenants`, itéré par les
/// crons). Les deux registres sont disjoints : `tenants` ∩ `code_vault` = ∅.
pub(crate) const CODE_VAULT_PREFIX: &str = "code-";

/// Limite globale du corps des requêtes internes : 4 MiB.
///
/// Protège contre les DoS par corps volumineux sur le listener loopback.
/// La route `persist/embedding` applique une limite plus stricte (512 KiB)
/// via un layer individuel — suffisant pour tout vecteur dim≤4096 × 4 octets.
const INTERNAL_BODY_LIMIT: usize = 4 * 1024 * 1024;

/// Limite du corps pour `persist/embedding` : 512 KiB.
///
/// Justification : vecteur dim=4096 × 4 octets = 16 Ko max ; 512 Ko est
/// largement suffisant même avec overhead JSON.
const EMBEDDING_BODY_LIMIT: usize = 512 * 1024;

/// Builds the internal API router (two auth-segregated sub-routers, one loopback listener).
///
/// - **Worker** sub-router : gardé par `internal_auth_middleware` (token `internal_api_token`,
///   header `X-Gradatum-Internal`) — persist/reads/delete-purge consommés par le worker.
/// - **Admin** sub-router (F-100 1.6) : gardé par `admin_auth_middleware` (token
///   `admin_api_token` DISTINCT, header `X-Gradatum-Admin`) — delete/restore/purge opérateur.
///
/// Chaque middleware est appliqué via `route_layer` au périmètre de son sous-routeur : le
/// worker ne peut PAS atteindre la surface admin (il ne détient pas le token admin), et
/// inversement. Séparation des rôles = invariant fondateur F-100.
///
/// ## Security
///
/// The router MUST be served only on the loopback listener — never merged
/// with the public router.
///
/// ## Body limits
///
/// - Global : 4 MiB (`INTERNAL_BODY_LIMIT`) via `DefaultBodyLimit` sur le router.
/// - `persist/embedding` : 512 KiB (`EMBEDDING_BODY_LIMIT`) via layer individuel
///   (surcharge la limite globale pour cette route).
pub fn build_internal_router(state: AppState) -> Router {
    // ── Sous-routeur worker (token worker `X-Gradatum-Internal`) ──
    //
    // `route_layer` applique le middleware d'auth worker UNIQUEMENT aux routes de ce
    // sous-routeur (pas au fallback). L'auth admin est séparée (autre token) sur le
    // sous-routeur admin ci-dessous — le worker ne peut PAS atteindre la surface admin.
    let worker_routes = Router::new()
        // ── Persist ──
        .route(
            "/internal/v1/persist/curated",
            post(persist::handle_persist_curated),
        )
        // Limite réduite sur embedding : vecteur dim≤4096 × 4 octets = 16 Ko max.
        .route(
            "/internal/v1/persist/embedding",
            post(persist::handle_persist_embedding)
                .layer(DefaultBodyLimit::max(EMBEDDING_BODY_LIMIT)),
        )
        .route(
            "/internal/v1/persist/forget",
            post(persist::handle_persist_forget),
        )
        .route(
            "/internal/v1/persist/distill",
            post(persist::handle_persist_distill),
        )
        // ── Note (GET + DELETE sur même path) ──
        .route(
            "/internal/v1/note/{ulid}",
            get(reads::handle_note_read).delete(persist::handle_delete_note),
        )
        // Réparation de la marque d'oubli d'index (A7-bis) — écriture, sans frontmatter.
        .route(
            "/internal/v1/note/{ulid}/forget-resync",
            post(persist::handle_note_forget_resync),
        )
        // ── Lecture ──
        // Statut scopé par vault (C4-1e W3) — re-check TOCTOU purge, source = index.
        .route(
            "/internal/v1/note/{ulid}/status",
            get(reads::handle_note_status),
        )
        .route(
            "/internal/v1/note/{ulid}/embedding",
            get(reads::handle_note_embedding),
        )
        .route(
            "/internal/v1/note/{ulid}/trust",
            get(reads::handle_note_trust),
        )
        // ── Lecture worker-flip (v0.5.3 single-owner DB) ──
        .route("/internal/v1/title-lookup", get(reads::handle_title_lookup))
        // Route fixe AVANT les routes paramétriques — cohérent avec les règles routing.
        .route("/internal/v1/id-lookup", get(reads::handle_id_lookup))
        .route(
            "/internal/v1/notes/by-locus",
            get(reads::handle_notes_by_locus),
        )
        .route(
            "/internal/v1/notes/count-unprocessed",
            get(reads::handle_count_unprocessed),
        )
        // C2 (EX-C2-3) : itération per-vault des crons worker à flag ON.
        .route(
            "/internal/v1/vaults/active",
            get(reads::handle_active_vaults),
        )
        .route(
            "/internal/v1/notes/by-status",
            get(reads::handle_notes_by_status),
        )
        .route(
            "/internal/v1/notes/garbage",
            get(reads::handle_notes_garbage),
        )
        .route(
            "/internal/v1/forget/search",
            get(reads::handle_forget_search),
        )
        .route(
            "/internal/v1/notes/by-agent",
            get(reads::handle_notes_by_agent),
        )
        .route_layer(from_fn_with_state(
            state.clone(),
            auth::internal_auth_middleware,
        ));

    // ── Sous-routeur admin (token admin `X-Gradatum-Admin`, F-100 1.6) ──
    //
    // Delete/restore/purge opérateur. Gardé par un token DISTINCT du worker (le worker
    // ne le détient pas → séparation des rôles, invariant fondateur F-100). Même
    // listener loopback ; jamais monté sur le routeur public ni exposé en MCP.
    let admin_routes = Router::new()
        .route(
            "/internal/v1/admin/delete",
            post(admin::handle_admin_delete),
        )
        .route(
            "/internal/v1/admin/archives/list",
            post(admin::handle_admin_archives_list),
        )
        .route(
            "/internal/v1/admin/archives/purge",
            post(admin::handle_admin_archives_purge),
        )
        .route(
            "/internal/v1/admin/archives/restore",
            post(admin::handle_admin_archives_restore),
        )
        // ── Cycle de vie des vaults (C2, EX-C2-4 — loopback-only) ──
        .route(
            "/internal/v1/admin/vaults/create",
            post(admin::handle_admin_vault_create),
        )
        .route(
            "/internal/v1/admin/vaults/suspend",
            post(admin::handle_admin_vault_suspend),
        )
        .route(
            "/internal/v1/admin/vaults/delete",
            post(admin::handle_admin_vault_delete),
        )
        .route(
            "/internal/v1/admin/vaults/purge",
            post(admin::handle_admin_vault_purge),
        )
        .route_layer(from_fn_with_state(
            state.clone(),
            admin_auth::admin_auth_middleware,
        ));

    Router::new()
        .merge(worker_routes)
        .merge(admin_routes)
        // ── Body limit global 4 MiB — commun aux deux sous-routeurs ──
        .layer(DefaultBodyLimit::max(INTERNAL_BODY_LIMIT))
        .with_state(state)
}

/// Spawns the internal HTTP listener on the given `bind` address.
///
/// ## Safety contract
///
/// `bind` MUST be a loopback address (`127.x.x.x` or `::1`).
/// Fails-fast if the address is non-loopback.
///
/// ## Errors
///
/// Returns an error if:
/// - `bind` is not a loopback address.
/// - The TCP listener cannot bind.
/// - The axum serve loop encounters a fatal error.
pub async fn spawn_internal_listener(bind: SocketAddr, router: Router) -> anyhow::Result<()> {
    // Guard loopback obligatoire — l'API interne ne doit JAMAIS être exposée sur
    // l'interface réseau publique.
    if !bind.ip().is_loopback() {
        anyhow::bail!("internal API: bind {bind} is not loopback — refusing to start (security)");
    }

    let listener = tokio::net::TcpListener::bind(bind).await?;
    let actual = listener.local_addr()?;
    info!(addr = %actual, "internal API listener started");

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("internal API listener stopped with error: {e}"))
}
