//! # gradatum-vault
//!
//! Vault domain logic : registry + lifecycle + overrides + drift + effective_note cache.
//!
//! Couche L2 de l'architecture Gradatum — composition au-dessus des couches L1 :
//! - `gradatum-core` : primitives, traits, erreurs.
//! - `gradatum-markdown` : parse + write `.md`.
//! - `gradatum-cache` : `EffectiveNoteCache` moka.
//! - `gradatum-index` : `SqliteIndex` impl `Index` trait.
//! - `gradatum-storage` : `FileStorage` OpenDAL.
//!
//! ## Modules Phase 1
//!
//! - [`registry`] : `Vault::create` / `Vault::open` — layout init, tenant_id, handles.
//! - [`lifecycle`] : `write_note` — ContentHash + persist .md + upsert index.
//! - [`overrides`] : `NoteMetadataOverride` — `Overridable` + `OverridePayload` impl.
//! - [`drift`] : `drift_check` — orchestration Phase A via `gradatum-index::scan_phase_a`.
//! - [`effective_note`] : `get_effective_note` — cache moka avec validation checksum hit.
//! - [`history`] : `NoteHistoryEntry` scaffold Phase 1.
//! - [`error`] : `VaultError` — erreurs typées sans `Box<dyn Error>`.
//!
//! ## Stubs Phase 1
//!
//! `read_note` et `get_effective_note` (cache miss) retournent `NoteNotFound`.
//! Implémentation complète reportée à T12+ (lecture depuis disque + apply overrides).
//!
//! ## Stabilité
//!
//! `0.x` — aucune garantie de stabilité API.
//! Voir [RELEASE-POLICY.md](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod drift;
pub mod effective_note;
pub mod error;
pub mod history;
pub mod lifecycle;
pub mod overrides;
pub mod registry;

pub use error::VaultError;
pub use history::NoteHistoryEntry;
pub use overrides::NoteMetadataOverride;
pub use registry::Vault;

// ── Registry trait (T2 P2.0c) ────────────────────────────────────────────────

/// Trait d'accès registre vault — exposé à `AppState` pour découpler le serveur
/// de l'implémentation concrète `Vault`.
///
/// Méthodes async via `async_trait` — compatible `Arc<dyn Registry>`.
///
/// ## Implémenteurs
///
/// - [`Vault`] : implémentation réelle depuis l'index SQLite.
/// - `PlaceholderRegistry` (dans `gradatum-server`) : stub retournant 0/0
///   pour les constructeurs sync avant injection du chemin vault.
#[async_trait::async_trait]
pub trait Registry: Send + Sync {
    /// Nombre de tenants (vault_id distincts) dans l'index.
    ///
    /// Retourne 0 si le vault est vide ou pas encore initialisé.
    async fn tenant_count(&self) -> Result<u32, gradatum_core::error::GradatumError>;

    /// Nombre de loci distincts (paires vault_id + locus) dans l'index.
    ///
    /// Un locus est l'unité d'organisation sub-tenant.
    /// Retourne 0 si aucune note n'est indexée.
    async fn locus_count(&self) -> Result<u32, gradatum_core::error::GradatumError>;

    /// S'assure qu'un tenant existe dans le registre.
    ///
    /// Idempotent — peut être appelé plusieurs fois sans effet de bord.
    async fn ensure_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<(), gradatum_core::error::GradatumError>;

    /// Lit une note par identifiant ULID (string) depuis le vault.
    ///
    /// T4 P2.0c : implémentation réelle avec cache hit/miss, checksum B22, disk read.
    ///
    /// ## Comportement
    ///
    /// - Cache hit valide → retour immédiat, compteur cache_hits incrémenté.
    /// - Cache miss → `index.get_note` + `storage.read(.md)` + `parse` + insert cache.
    ///
    /// ## Erreurs
    ///
    /// - `GradatumError::NoteNotFound` si l'identifiant est absent de l'index.
    /// - `GradatumError::Storage` si la lecture disque échoue.
    async fn read_note_by_id(
        &self,
        note_id: &str,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError>;
}

#[async_trait::async_trait]
impl Registry for Vault {
    async fn tenant_count(&self) -> Result<u32, gradatum_core::error::GradatumError> {
        self.index.vault_id_count().await
    }

    async fn locus_count(&self) -> Result<u32, gradatum_core::error::GradatumError> {
        self.index.locus_count().await
    }

    async fn ensure_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        self.index.ensure_vault_id(tenant_id).await
    }

    async fn read_note_by_id(
        &self,
        note_id: &str,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;
        use ulid::Ulid;

        let ulid = Ulid::from_string(note_id).map_err(|e| {
            GradatumError::Storage(format!("read_note_by_id : ULID invalide {note_id:?} : {e}"))
        })?;
        let id = gradatum_core::identity::NoteId(ulid);

        self.read_note(id).await.map_err(|e| match e {
            crate::error::VaultError::Core(inner) => inner,
            crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
            crate::error::VaultError::Markdown(msg) => {
                GradatumError::Markdown(format!("read_note_by_id : {msg}"))
            }
        })
    }
}

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
