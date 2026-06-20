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
//! ## Routes
//!
//! ### Persist
//! - `POST /internal/v1/persist/curated`  — pipeline 5 writes séquentiels
//! - `POST /internal/v1/persist/embedding` — stockage vecteur
//! - `POST /internal/v1/persist/forget`   — marquage oubli sémantique
//! - `POST /internal/v1/persist/distill`  — mise à jour note distillée
//!
//! ### Note
//! - `DELETE /internal/v1/note/{ulid}`     — suppression note
//!
//! ### Lecture
//! - `GET /internal/v1/note/{ulid}`              — note complète
//! - `GET /internal/v1/note/{ulid}/embedding`    — vecteur embedding
//! - `GET /internal/v1/note/{ulid}/trust`        — score trust
//! - `GET /internal/v1/title-lookup`             — lookup ULID par titre (H1 Markdown)
//! - `GET /internal/v1/id-lookup`                — lookup existence par ULID (ULID-first)
//! - `GET /internal/v1/notes/by-locus`           — notes par préfixe locus
//! - `GET /internal/v1/notes/by-status`          — notes par statut
//! - `GET /internal/v1/notes/garbage`            — notes Garbage expirées
//! - `GET /internal/v1/forget/search`            — FTS5 pour scope Topic forget
//! - `GET /internal/v1/notes/by-agent`           — notes par agent (scope Agent forget)

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

/// Builds the internal API router.
///
/// The middleware `internal_auth_middleware` is applied to all routes, using
/// `AppState` (which holds `internal_api_token`).
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
    Router::new()
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
        // ── Lecture ──
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
        // ── Body limit global 4 MiB — avant le middleware auth ──
        .layer(DefaultBodyLimit::max(INTERNAL_BODY_LIMIT))
        // ── Auth middleware — state injecté ici ──
        .layer(from_fn_with_state(
            state.clone(),
            auth::internal_auth_middleware,
        ))
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
        anyhow::bail!(
            "API interne : bind {bind} n'est pas loopback — refus de démarrer (sécurité)"
        );
    }

    let listener = tokio::net::TcpListener::bind(bind).await?;
    let actual = listener.local_addr()?;
    info!(addr = %actual, "listener API interne démarré");

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("listener API interne arrêté avec erreur : {e}"))
}
