//! Taxonomie des erreurs Gradatum.
//!
//! See ARCHITECTURE.md for the error model design.
//!
//! ## Hiérarchie
//!
//! - `GradatumError` — erreur top-level pour les consommateurs des crates L1+
//! - `ValidationError` — validation de données entrantes (Tag, VaultId, etc.)
//! - `DriftError` — écart détecté entre source de vérité Markdown et index SQLite
//! - `ConfigError` — chargement/parsing de la config TOML (voir `config.rs`)
//! - `schema_registry::ValidationError` — validation payload override contre schéma
//! - `schema_registry::MigrationError` — migration payload override
//!
//! ## Typage fort
//!
//! Conformément aux règles Rust-grade : pas de `Box<dyn Error>` en lib publique.
//! Toutes les erreurs sont typées via `thiserror`.
//!
//! ## Caveat C11
//!
//! `GradatumError::VaultOnNfs` — le vault ne peut pas être monté sur NFS.
//! Détection via `nix::sys::statfs::statfs` + `STATFS_TYPE == NFS_SUPER_MAGIC`.
//! Vérification déclenchée lors de `gradatum-vault::VaultConfig::validate()`.

use std::path::PathBuf;
use thiserror::Error;

use crate::frontmatter::SchemaVersion;
use crate::identity::{ContentHash, NoteId};
use crate::scope::VaultId;
use crate::status::NoteStatus;

/// Erreur top-level de gradatum-core.
///
/// Produite par les couches L0 (core) et retournée aux consommateurs L1+.
/// Chaque variante mappe une couche spécifique de l'architecture.
#[derive(Debug, Error)]
pub enum GradatumError {
    /// Erreur de validation d'une valeur entrante.
    #[error("erreur de validation : {0}")]
    Validation(#[from] ValidationError),

    /// Drift détecté entre le Markdown sur disque et l'index SQLite.
    #[error("drift détecté : {0}")]
    Drift(#[from] DriftError),

    /// Erreur de stockage (SQLite, OpenDAL, filesystem).
    ///
    /// Les couches de stockage mapent leurs erreurs spécifiques via `GradatumError::Storage`.
    #[error("erreur de stockage : {0}")]
    Storage(String),

    /// Erreur de parsing Markdown.
    #[error("erreur parse Markdown : {0}")]
    Markdown(String),

    /// Note introuvable dans l'index.
    #[error("note introuvable : {0:?}")]
    NoteNotFound(NoteId),

    /// Transition de statut invalide (ne respecte pas la state machine §2.6).
    #[error("transition de statut invalide : {from:?} → {to:?}")]
    InvalidStatusTransition {
        /// Statut source (avant la transition).
        from: NoteStatus,
        /// Statut cible (refusé).
        to: NoteStatus,
    },

    /// Mismatch de version de schéma frontmatter.
    #[error("version de schéma incorrecte : attendu {expected}, trouvé {found}")]
    SchemaVersionMismatch {
        /// Version attendue par le crate courant.
        expected: SchemaVersion,
        /// Version trouvée dans le frontmatter.
        found: SchemaVersion,
    },

    /// Vault introuvable dans la configuration.
    #[error("vault introuvable : {0:?}")]
    VaultNotFound(VaultId),

    /// Vault monté sur NFS — non supporté.
    ///
    /// Caveat C11 — le vault doit être sur un filesystem local.
    /// Détection via `nix::sys::statfs::statfs` + `NFS_SUPER_MAGIC` comparison.
    #[error("vault root sur NFS (NFS_SUPER_MAGIC), non supporté : {path:?}")]
    VaultOnNfs {
        /// Chemin du vault root dont le statfs a retourné NFS_SUPER_MAGIC.
        path: PathBuf,
    },

    /// Erreur de validation du payload override contre le schéma registry.
    #[error("validation schéma override : {0}")]
    SchemaValidation(#[from] crate::schema_registry::ValidationError),

    /// Erreur de migration du payload override.
    #[error("migration schéma override : {0}")]
    SchemaMigration(#[from] crate::schema_registry::MigrationError),

    /// Erreur I/O (lecture/écriture fichier, permissions, etc.).
    #[error("io : {0}")]
    Io(#[from] std::io::Error),

    /// Erreur de parsing TOML.
    #[error("toml parse : {0}")]
    TomlParse(#[from] toml::de::Error),

    /// Erreur de sérialisation TOML.
    #[error("toml serialize : {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    /// Erreur de configuration (chargement TOML, validation champs).
    #[error("config : {0}")]
    Config(#[from] crate::config::ConfigError),

    /// Erreur d'inférence (embedding, reranker, modèle LLM).
    ///
    /// Phase 2.x.2 alpha.11 patch.1 — variant dédié pour les erreurs
    /// d'embedder/reranker. Permet aux handlers (ex: `vault_search`) de
    /// distinguer une panne d'inférence d'une erreur de stockage et de
    /// dégrader gracieusement (fallback BM25 au lieu de 500).
    ///
    /// La conversion `From<EmbedError>` est implémentée côté
    /// `gradatum-embed::error` pour respecter les orphan rules.
    ///
    /// Mapping HTTP recommandé : 503 Service Unavailable (embedder en panne)
    /// — mais les handlers production peuvent préférer un fallback gracieux
    /// (200 + BM25 only) via `tracing::warn!` et un vecteur sémantique vide.
    #[error("inference : {0}")]
    Inference(String),
}

/// Erreur de validation d'une valeur entrante.
///
/// Retournée par les constructeurs qui valident le format des types newtype
/// (ex. `Tag::new()`, `VaultId::validate()`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// Tag mal formé (format invalide ou trop long).
    ///
    /// Format attendu : `^[a-z0-9][a-z0-9-]{0,63}$`
    #[error("tag invalide : {0:?} (format attendu: ^[a-z0-9][a-z0-9-]{{0,63}}$)")]
    InvalidTag(String),

    /// Vault ID mal formé.
    #[error("vault_id invalide : {0:?}")]
    InvalidVaultId(String),

    /// Locus ID mal formé.
    #[error("locus_id invalide : {0:?}")]
    InvalidLocusId(String),

    /// Section invalide.
    #[error("section invalide : {0:?}")]
    InvalidSection(String),

    /// Statut invalide.
    #[error("statut invalide : {0:?}")]
    InvalidStatus(String),

    /// Corps de note vide.
    #[error("corps de note vide")]
    EmptyBody,
}

/// Erreur de détection de drift entre Markdown et index SQLite.
///
/// Produite par `Note::verify_integrity()` et par le drift detector (`gradatum-vault`).
/// Déclenche un re-parse + re-index + re-embed du fichier concerné.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum DriftError {
    /// Le ContentHash recomputed diffère du hash stocké en SQLite.
    ///
    /// Cause probable : édition manuelle du fichier Markdown hors de Gradatum.
    /// Action : re-parse + re-index par le worker.
    #[error("content hash mismatch: stored={stored}, computed={computed}")]
    ContentHashMismatch {
        /// Hash stocké dans SQLite à la dernière écriture connue.
        stored: ContentHash,
        /// Hash recomputed depuis le fichier Markdown actuel.
        computed: ContentHash,
    },

    /// Fichier Markdown absent sur disque pour une note indexée.
    #[error("fichier md absent sur disque : {note_id:?}")]
    NoteMdMissing {
        /// Identifiant de la note dont le fichier `.md` est absent.
        note_id: NoteId,
    },

    /// Fichier Markdown orphelin (pas de note correspondante dans l'index).
    #[error("fichier md orphelin : {0}")]
    OrphanMd(String),
}
