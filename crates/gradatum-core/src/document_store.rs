//! Contrat de stockage CRUD des notes (documents).
//!
//! [`DocumentStore`] est le trait minimal pour lire et écrire des notes dans un index.
//! Il est un sous-trait du [`Index`](crate::index::Index) historique et est conçu pour
//! être consommé par des crates qui n'ont besoin que des opérations documentaires
//! (`gradatum-vault`, `gradatum-context`), sans dépendre de `gradatum-index`.
//!
//! ## Évolution Silver (v1.0.0)
//!
//! Le type `Note` convergera vers `Document` à Silver — `write_note` sera renommé `write`.
//! L'erreur `GradatumError` convergera vers un `StoreError` dédié (cf. `QueueStore`) à v0.4.0.

use async_trait::async_trait;

use crate::error::GradatumError;
use crate::identity::{ContentHash, NoteId};
use crate::index::NoteRecord;
use crate::note::Note;
use crate::scope::VaultId;
use crate::status::NoteStatus;

/// Contrat de stockage CRUD des notes — async, thread-safe.
///
/// Implémenté par `gradatum-index::SqliteIndex` (Phase 1).
/// Consommé par `gradatum-vault` et les futurs consommateurs (`gradatum-context`)
/// sans dépendre de l'implémentation concrète SQLite.
///
/// ## Stabilité
///
/// `#[stability::unstable]` — l'API peut changer jusqu'à Silver (v1.0.0).
/// Le type `Note` convergera vers `Document` à Silver (v1.0.0) — `write_note`
/// sera renommé `write`. L'erreur `GradatumError` convergera vers `StoreError`
/// dédié (cf. `QueueStore`) à v0.4.0.
///
/// ## Contention
///
/// En v0.3.0, les 3 traits (`DocumentStore`, `IndexStore`, `VectorStore`) partagent
/// un `Arc<Mutex<Connection>>` unique. Toute implémentation doit s'assurer que
/// les MutexGuard ne traversent pas de `.await`. Séparation physique des connexions
/// prévue à v0.4.0.
// AM1 : instabilité documentée ici et dans le module doc.
// `#[stability::unstable]` différé v0.4.0 — nécessite `[features] unstable-storage-traits = []`
// dans gradatum-core/Cargo.toml + opt-in de tous les consommateurs workspace.
// La macro stability n'empêche rien (pas d'E0365) ; sans la feature déclarée elle émettrait
// un deprecated warning sur chaque consommateur.
#[async_trait]
pub trait DocumentStore: Send + Sync {
    /// Écrit ou met à jour une note dans l'index (idempotent / upsert).
    ///
    /// `Note.id` est la clé primaire ULID. Une écriture concurrente avec le même
    /// identifiant ne DOIT pas corrompre l'état — l'implémentation doit être atomique.
    ///
    /// # Effets de bord
    ///
    /// Met à jour la table FTS5 (`notes_fts`) de façon synchrone si l'index le supporte.
    ///
    /// # Note Silver (v1.0.0)
    ///
    /// Sera renommé `write` lors de la migration `Note` → `Document`.
    async fn write_note(&self, note: &Note) -> Result<(), GradatumError>;

    /// Retourne le `ContentHash` stocké pour une note, si elle existe dans l'index.
    ///
    /// Retourne `None` si la note n'est pas encore indexée.
    async fn get_content_hash(&self, id: NoteId) -> Result<Option<ContentHash>, GradatumError>;

    /// Retourne le record complet d'une note par son identifiant ULID.
    ///
    /// Retourne `None` si la note n'existe pas ou est une sentinelle.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue.
    async fn get_note(
        &self,
        tenant_id: &str,
        note_id_ulid: &str,
    ) -> Result<Option<NoteRecord>, GradatumError>;

    /// Liste les notes d'un vault filtrées par statut.
    ///
    /// Résultats triés par `updated DESC NULLS LAST, created DESC`.
    async fn list_by_status(
        &self,
        vault_id: &VaultId,
        status: NoteStatus,
    ) -> Result<Vec<NoteId>, GradatumError>;

    // ── Méthodes promues à l'Étape 0.2a (dyn-wiring) ─────────────────────────

    /// Désactive une note en la passant au statut `downgraded`.
    ///
    /// Met à jour `status`, `status_reason`, `replaced_by`, `status_changed`, `updated`.
    /// Idempotent si la note est déjà downgraded.
    ///
    /// # Erreurs
    ///
    /// - `GradatumError::NoteNotFound` si la note est absente.
    /// - `GradatumError::Storage` en cas d'erreur SQLite.
    async fn downgrade_note(
        &self,
        note_id: &NoteId,
        reason: &str,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError>;

    /// PATCH partiel du statut d'une note (statut, raison, replaced_by).
    ///
    /// Met à jour uniquement les champs fournis (`None` = inchangé).
    /// `status_changed` est mis à jour uniquement si `status` est fourni.
    /// `updated` est toujours mis à jour.
    ///
    /// # Erreurs
    ///
    /// - `GradatumError::NoteNotFound` si aucune note ne correspond à `note_id`.
    /// - `GradatumError::Storage` en cas d'erreur SQLite.
    async fn patch_note_status(
        &self,
        note_id: &NoteId,
        status: Option<&str>,
        status_reason: Option<&str>,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError>;

    /// Met à jour la colonne `title` d'une note existante.
    ///
    /// Idempotent. Best-effort en production : le curator loggue en cas d'erreur mais ne propage pas.
    /// Via ce trait, le résultat est propagé — le contexte appelant décide de l'ignorer ou non.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
    async fn upsert_note_title(&self, note_id: &NoteId, title: &str) -> Result<(), GradatumError>;
}
