//! GET /health — endpoint D1 non authentifié.
//!
//! Retourne un payload JSON avec 10 champs diagnostiques.
//! Accessible sans authentification (RFC-0003 §8).
//!
//! # Status
//!
//! - `"ok"` : état nominal.
//! - `"degraded"` : queue trop profonde (`depth > 1000`) ou trop vieille (`oldest_age_secs > 300`).
//!   Le code HTTP reste 200 dans les deux cas — c'est l'ops qui décide de l'action.
//!
//! # Stubs T10
//!
//! - `tenant_count` / `locus_count` : 0 (stub T8 — T12 câblera les vraies valeurs).
//! - `queue_depth` / `queue_oldest_age_secs` : 0 (queue implémentée en P2.0b).
//! - `sqlite_wal_size_bytes` : taille fichier WAL si présent, sinon 0.
//!
//! # Pas de PII
//!
//! Le payload ne contient aucun path complet, token, IP ou donnée personnelle.
//! `build_sha` est un identifiant de commit (public), pas une donnée sensible.

use std::time::UNIX_EPOCH;

use axum::{extract::State, Json};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::state::AppState;

/// Payload retourné par `GET /health`.
#[derive(Debug, Serialize)]
pub struct HealthPayload {
    /// État du service : `"ok"` | `"degraded"`.
    pub status: &'static str,
    /// Version du binaire (CARGO_PKG_VERSION).
    pub version: &'static str,
    /// SHA du commit de build (env BUILD_SHA, sinon `"unknown"`).
    pub build_sha: &'static str,
    /// Secondes écoulées depuis le démarrage du processus.
    pub uptime_secs: u64,
    /// Nombre de tenants connus (stub 0 en T10 — T12 câble la vraie valeur).
    pub tenant_count: u32,
    /// Nombre de loci connus (stub 0 en T10 — T12 câble la vraie valeur).
    pub locus_count: u32,
    /// Profondeur de la queue de traitement (stub 0 en T10 — P2.0b).
    pub queue_depth: u64,
    /// Âge de l'entrée la plus ancienne en file (secondes, stub 0 en T10 — P2.0b).
    pub queue_oldest_age_secs: u64,
    /// Taille du fichier WAL SQLite en octets (0 si absent ou inaccessible).
    pub sqlite_wal_size_bytes: u64,
    /// Horodatage de démarrage du processus au format RFC3339.
    pub started_at: String,
}

/// Handler `GET /health` — unauthenticated (RFC-0003 §8).
///
/// Aucune vérification de `TrustContext` — appelé directement depuis le routeur
/// AVANT l'application du middleware auth.
///
/// # Effets de bord
/// Lecture taille fichier WAL (syscall `metadata`) — non bloquant sur NVMe/tmpfs.
pub async fn handler(State(s): State<AppState>) -> Json<HealthPayload> {
    // Stubs T10 — queue implémentée en P2.0b.
    let queue_depth: u64 = 0;
    let queue_oldest_age_secs: u64 = 0;

    let status = if queue_depth > 1_000 || queue_oldest_age_secs > 300 {
        "degraded"
    } else {
        "ok"
    };

    let uptime_secs = s.started_at.elapsed().as_secs();

    // Taille WAL : tente la lecture sans panic — retourne 0 si absent ou erreur.
    // Le path WAL est conventionnellement <vault_index_path>-wal (SQLite standard).
    // On exposerait `wal_path` depuis AppState (ajouté T10). En attendant T12, stub à 0.
    let sqlite_wal_size_bytes: u64 = 0;

    // Conversion SystemTime → RFC3339 sans panic.
    // `duration_since(UNIX_EPOCH)` échoue uniquement si la clock système est avant 1970 — impossible.
    let started_at = s
        .started_at_systime
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| {
            DateTime::<Utc>::from_timestamp(d.as_secs() as i64, d.subsec_nanos())
                .map(|dt| dt.to_rfc3339())
        })
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());

    // T2 P2.0c : vrais comptages depuis le registry vault (méthodes async).
    // Fallback à 0 si le vault n'est pas encore initialisé ou inaccessible.
    let tenant_count = s.vault.tenant_count().await.unwrap_or(0);
    let locus_count = s.vault.locus_count().await.unwrap_or(0);

    Json(HealthPayload {
        status,
        version: s.version,
        build_sha: s.build_sha,
        uptime_secs,
        tenant_count,
        locus_count,
        queue_depth,
        queue_oldest_age_secs,
        sqlite_wal_size_bytes,
        started_at,
    })
}
