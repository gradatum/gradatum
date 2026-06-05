use serde::{Deserialize, Serialize};

use crate::default_main;

/// Requête `vault_downgrade` — rétrogradation d'une note.
///
/// Sérialisée via `bincode::serde::encode_to_vec` pour le payload de la queue.
///
/// POST `/api/v1/vault_downgrade`. Idempotent : downgrader une note déjà
/// downgradée retourne 200 avec le status courant.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Serialize, Deserialize)]
pub struct VaultDowngradeRequest {
    /// Identifiant ULID de la note à rétrograder.
    pub note_id: String,
    /// Raison de la rétrogradation (ex. `"obsolète"`, `"doublon"`, `"révisé"`).
    /// Maximum 500 caractères.
    pub reason: String,
    /// Note de remplacement (ULID, optionnel).
    #[serde(default)]
    pub replaced_by: Option<String>,
    /// Tenant cible (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
}

/// Réponse `vault_downgrade` — Phase 2.1.2 alpha.9.
///
/// Retournée par POST `/api/v1/vault_downgrade` après opération réussie (200).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultDowngradeResponse {
    /// ULID de la note modifiée.
    pub note_id: String,
    /// Status après l'opération : `"downgraded"`.
    pub status: String,
    /// Unix epoch millisecondes du changement de status.
    pub status_changed: i64,
    /// Raison enregistrée.
    pub reason: String,
}

/// Body PATCH `/api/v1/notes/{id}` — Phase 2.1.2 alpha.9.
///
/// Body partiel : tous les champs sont optionnels en sérialisation.
/// Le handler exige qu'au moins un champ soit présent (validation applicative).
///
/// Permet : downgrade, revert downgrade → live, reclasse staging → live, etc.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoteStatusPatch {
    /// Nouveau status : `"live"` | `"staging"` | `"downgraded"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Raison du changement de status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// Note de remplacement (ULID, uniquement pertinent si `status = "downgraded"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_by: Option<String>,
}
