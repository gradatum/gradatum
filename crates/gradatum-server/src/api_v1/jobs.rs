//! Handler poll job — `GET /api/v1/jobs/<id>` (P2.1).
//!
//! Lit l'état réel d'un job depuis `jobs_v2` via `Queue::get`.
//!
//! # Endpoint
//!
//! | Méthode | Path | Auth |
//! |---------|------|------|
//! | GET | `/api/v1/jobs/:id` | Conditionnelle (flag `require_jwt_jobs_endpoint`) |
//!
//! Note sécurité : le job_id est un `i64` AUTOINCREMENT — le client doit posséder
//! l'`EnqueuedResponse` pour connaître l'ID. Pas de liste publique des jobs.
//!
//! **INVARIANT réseau** : par défaut (`require_jwt_jobs_endpoint = false`),
//! cet endpoint est sans auth — déployer derrière un réseau privé (VPN, pare-feu).
//! Passer le flag à `true` pour une exposition sur réseau public (V4 Phase 2.1.1).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};

use crate::api_v1::dto::JobStatusResponse;
use crate::state::AppState;
use gradatum_core::trust::TrustContext;
use gradatum_queue::JobStatus;

/// Mappe une chaîne d'erreur worker (anyhow `to_string()` brut) vers un code opaque.
///
/// Empêche l'info disclosure (chemins FS absolus, ULIDs invalides reflétés,
/// état interne) à un appelant non authentifié — caveat security review V1.
fn sanitize_job_error(raw: &str) -> &'static str {
    if raw.contains("ULID invalide") || raw.contains("invalid character") {
        "invalid_input"
    } else if raw.contains("Vault::") || raw.contains("vault non configuré") {
        "vault_error"
    } else if raw.contains("Storage") || raw.contains("sqlx") || raw.contains("SQLite") {
        "storage_error"
    } else {
        "processing_error"
    }
}

/// `GET /api/v1/jobs/:id`
///
/// Retourne le statut courant d'un job de la queue (lecture `jobs_v2`).
///
/// # Auth conditionnelle (V4)
///
/// Si `state.require_jwt_jobs_endpoint == true`, exige un bearer JWT valide
/// (injecté via `Extension<TrustContext>` par `auth_middleware`).
/// Sinon : accessible sans authentification (invariant réseau privé).
///
/// # Retour
///
/// - **200 OK** + JSON [`JobStatusResponse`] — statut du job, `last_error` mappé en code opaque.
/// - **401 Unauthorized** — auth requise mais bearer absent ou invalide.
/// - **404 Not Found** — job inexistant.
/// - **500 Internal Server Error** — erreur SQLite (loggée côté serveur, non exposée).
pub async fn get_job(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Path(id): Path<i64>,
) -> Result<Json<JobStatusResponse>, StatusCode> {
    // P1-4 Phase 4.2bis : flag require_jwt_jobs_endpoint retiré (fantôme inopérant).
    // v0.2.0 Bronze invariant réseau privé — endpoint accessible sans bearer.
    // Auth granulaire F-45 v1.0.0 Gold (spec §11 E-21).
    // Argument trust conservé : injecté par auth_middleware (requis par tous les handlers /api/v1/*).
    let _ = &trust;

    match state.queue.get(id).await {
        Ok(Some(info)) => {
            // Caveat C1 2026-05-08 : signal soft d'incohérence DB potentielle.
            // SqliteQueue::get fait `unwrap_or(Pending)` sur status DB inconnu (silent fallback).
            // Si on observe attempts > 0 et status Pending, c'est suspect (un job traité ne
            // revient normalement pas à pending sans fail explicite).
            if info.status == JobStatus::Pending && info.attempts > 0 {
                tracing::warn!(
                    job_id = id,
                    attempts = info.attempts,
                    "job pending with attempts>0 — possible DB status inconsistency or unknown variant fallback"
                );
            }
            Ok(Json(JobStatusResponse {
                job_id: info.id,
                status: info.status.as_str().to_string(),
                attempts: info.attempts,
                last_error: info
                    .last_error
                    .as_deref()
                    .map(|raw| sanitize_job_error(raw).to_string()),
            }))
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, job_id = id, "queue.get failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_job_error;

    #[test]
    fn ulid_error_maps_invalid_input() {
        assert_eq!(
            sanitize_job_error("ULID invalide abc123: invalid character"),
            "invalid_input"
        );
    }

    #[test]
    fn vault_error_maps_vault_error() {
        assert_eq!(
            sanitize_job_error("Vault::open(/var/lib/gradatum/vault) failed: permission denied"),
            "vault_error"
        );
    }

    #[test]
    fn storage_error_maps_storage_error() {
        assert_eq!(
            sanitize_job_error("Storage(\"sqlx: connection refused\")"),
            "storage_error"
        );
    }

    #[test]
    fn fallback_processing_error() {
        assert_eq!(
            sanitize_job_error("unknown failure mode"),
            "processing_error"
        );
    }
}
