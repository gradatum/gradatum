//! `FileStorage` — implémentation OpenDAL filesystem du trait `Storage`.
//!
//! ## Fonctionnement
//!
//! Délègue toutes les opérations I/O à un `opendal::Operator` configuré avec
//! le backend `services::Fs`. La racine (root) est fixée à la construction.
//!
//! ## Caveat C11
//!
//! `FileStorage::new()` appelle `ensure_local_filesystem(root)` **avant** de
//! construire l'`Operator`. Si le chemin réside sur NFS, la construction échoue
//! avec `StorageError::Core(GradatumError::VaultOnNfs)`.
//!
//! ## Sécurité — guard path traversal (E-29)
//!
//! OpenDAL Fs 0.51 ne rejette pas les composants `..` nativement. Chaque opération
//! appelle `validate_relative_path()` en entrée — defense in depth obligatoire pour
//! le contrat "Storage abstraction confinée" (Phase 2+ S3/GCS networked).
//! Spec : §11 E-29.
//!
//! ## Features
//!
//! Cette implémentation est disponible via la feature `fs` (activée par défaut).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use opendal::{services, EntryMode, Operator};
use tracing::instrument;

use crate::error::StorageError;
use crate::storage_trait::{Storage, StorageEntry};

/// Implémentation `Storage` via le backend filesystem OpenDAL.
///
/// Thread-safe — `Operator` est `Clone + Send + Sync` en interne.
pub struct FileStorage {
    /// Opérateur OpenDAL configuré sur la racine.
    op: Operator,
    /// Chemin absolu de la racine (conservé pour diagnostic et `root()`).
    root: PathBuf,
}

/// Valide qu'un chemin relatif ne contient pas de composant `..` ni de chemin absolu.
///
/// OpenDAL Fs 0.51 ne rejette pas les composants `..` nativement — ce guard
/// est la seule barrière contre un path traversal hors du root configuré.
///
/// # Erreurs
///
/// - `StorageError::InvalidPath` — chemin absolu (commence par `/`) ou contient `..`.
fn validate_relative_path(path: &str) -> Result<(), StorageError> {
    // Refuser les chemins absolus (commençant par `/`).
    if path.starts_with('/') {
        return Err(StorageError::InvalidPath(PathBuf::from(path)));
    }
    // Rejeter tout composant `..` pour empêcher le path traversal hors du root configuré.
    // Pourquoi explicite plutôt que confier à OpenDAL : FsBackend::read/write/etc. fait
    // `root.join(path)` sans canonicalization post-join — `../x` accède hors root.
    for component in std::path::Path::new(path).components() {
        if component == std::path::Component::ParentDir {
            return Err(StorageError::InvalidPath(PathBuf::from(path)));
        }
    }
    Ok(())
}

impl FileStorage {
    /// Construit un `FileStorage` enraciné à `root`.
    ///
    /// ## Caveat C11
    ///
    /// Appelle `ensure_local_filesystem(root)` en premier. Retourne
    /// `Err(StorageError::Core(GradatumError::VaultOnNfs))` si NFS détecté.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::Core(VaultOnNfs)` — chemin sur NFS.
    /// - `StorageError::InvalidPath` — chemin non convertible en UTF-8.
    /// - `StorageError::OpenDal` — échec de construction de l'`Operator`.
    pub fn new(root: &Path) -> Result<Self, StorageError> {
        // Caveat C11 BLOQUANT — vérifier NFS avant toute construction.
        crate::nfs_check::ensure_local_filesystem(root)?;

        let root_str = root
            .to_str()
            .ok_or_else(|| StorageError::InvalidPath(root.to_path_buf()))?;

        let builder = services::Fs::default().root(root_str);
        let op = Operator::new(builder)
            .map_err(|e| StorageError::OpenDal(e.to_string()))?
            .finish();

        Ok(Self {
            op,
            root: root.to_path_buf(),
        })
    }

    /// Retourne le chemin absolu de la racine configurée.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl Storage for FileStorage {
    /// Lit le contenu d'un fichier au chemin relatif `path`.
    ///
    /// `path` est relatif à la racine du storage.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::InvalidPath` — chemin absolu ou contenant `..` (E-29).
    #[instrument(skip(self), fields(path))]
    async fn read(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        validate_relative_path(path)?;
        self.op
            .read(path)
            .await
            .map(|buf| buf.to_vec())
            .map_err(|e| {
                if e.kind() == opendal::ErrorKind::NotFound {
                    StorageError::NotFound(path.to_owned())
                } else {
                    StorageError::OpenDal(e.to_string())
                }
            })
    }

    /// Écrit `content` au chemin relatif `path`.
    ///
    /// Les répertoires intermédiaires sont créés automatiquement par OpenDAL/Fs.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::InvalidPath` — chemin absolu ou contenant `..` (E-29).
    #[instrument(skip(self, content), fields(path, bytes = content.len()))]
    async fn write(&self, path: &str, content: &[u8]) -> Result<(), StorageError> {
        validate_relative_path(path)?;
        self.op
            .write(path, content.to_vec())
            .await
            .map_err(|e| StorageError::OpenDal(e.to_string()))
    }

    /// Supprime le fichier au chemin relatif `path`.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::InvalidPath` — chemin absolu ou contenant `..` (E-29).
    #[instrument(skip(self), fields(path))]
    async fn delete(&self, path: &str) -> Result<(), StorageError> {
        validate_relative_path(path)?;
        self.op.delete(path).await.map_err(|e| {
            if e.kind() == opendal::ErrorKind::NotFound {
                StorageError::NotFound(path.to_owned())
            } else {
                StorageError::OpenDal(e.to_string())
            }
        })
    }

    /// Liste les entrées dont le chemin commence par `prefix`.
    ///
    /// Retourne une liste plate (non-récursive par entrée mais scan récursif du prefix).
    /// Les répertoires sont inclus si retournés par le backend.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::InvalidPath` — chemin absolu ou contenant `..` (E-29).
    #[instrument(skip(self), fields(prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, StorageError> {
        validate_relative_path(prefix)?;
        // `list_with(prefix).recursive(true)` pour scan récursif (équivalent find).
        // Sans `.recursive(true)`, seul le niveau immédiat est retourné.
        let entries = self
            .op
            .list_with(prefix)
            .recursive(true)
            .await
            .map_err(|e| StorageError::OpenDal(e.to_string()))?;

        let result = entries
            .into_iter()
            .map(|e| {
                let meta = e.metadata();
                let is_dir = matches!(meta.mode(), EntryMode::DIR);
                let size = if is_dir { 0 } else { meta.content_length() };
                let last_modified = meta.last_modified().map(|dt| dt.timestamp_millis());
                StorageEntry {
                    path: e.path().to_owned(),
                    size,
                    last_modified,
                    is_dir,
                }
            })
            .collect();

        Ok(result)
    }

    /// Retourne les métadonnées de l'objet à `path`.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::InvalidPath` — chemin absolu ou contenant `..` (E-29).
    #[instrument(skip(self), fields(path))]
    async fn stat(&self, path: &str) -> Result<StorageEntry, StorageError> {
        validate_relative_path(path)?;
        let meta = self.op.stat(path).await.map_err(|e| {
            if e.kind() == opendal::ErrorKind::NotFound {
                StorageError::NotFound(path.to_owned())
            } else {
                StorageError::OpenDal(e.to_string())
            }
        })?;

        let is_dir = matches!(meta.mode(), EntryMode::DIR);
        let size = if is_dir { 0 } else { meta.content_length() };
        let last_modified = meta.last_modified().map(|dt| dt.timestamp_millis());

        Ok(StorageEntry {
            path: path.to_owned(),
            size,
            last_modified,
            is_dir,
        })
    }

    /// Retourne `true` si un objet existe à `path`, `false` sinon.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::InvalidPath` — chemin absolu ou contenant `..` (E-29).
    #[instrument(skip(self), fields(path))]
    async fn exists(&self, path: &str) -> Result<bool, StorageError> {
        validate_relative_path(path)?;
        self.op
            .exists(path)
            .await
            .map_err(|e| StorageError::OpenDal(e.to_string()))
    }

    /// Crée le répertoire à `path` (idempotent — convergence v81 §6).
    ///
    /// Délègue à `Operator::create_dir` — opération native OpenDAL.
    /// Le chemin doit se terminer par `/` (exigence OpenDAL).
    ///
    /// # Erreurs
    ///
    /// - `StorageError::InvalidPath` — chemin absolu ou contenant `..` (E-29).
    #[instrument(skip(self), fields(path))]
    async fn create_dir(&self, path: &str) -> Result<(), StorageError> {
        validate_relative_path(path)?;
        self.op
            .create_dir(path)
            .await
            .map_err(|e| StorageError::OpenDal(e.to_string()))
    }
}
