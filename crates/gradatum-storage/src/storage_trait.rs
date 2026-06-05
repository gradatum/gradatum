//! Trait `Storage` — abstraction des opérations de stockage primitives.
//!
//! Toutes les opérations sont asynchrones et orientées chemin (path-based).
//! Les chemins sont relatifs à la racine configurée dans chaque implémentation.
//!
//! ## Implémentations fournies
//!
//! - [`crate::FileStorage`] — backend OpenDAL filesystem (feature `fs`, activée par défaut).
//! - Backend S3 (feature `s3`) — Phase 2+.
//! - Backend Azure Blob (feature `azblob`) — Phase 2+.
//!
//! ## Contrat de chemin
//!
//! Les chemins passés aux méthodes du trait sont toujours relatifs à la racine du storage.
//! Le séparateur est `/` (Unix). Les chemins absolus sont refusés.

use async_trait::async_trait;

use crate::error::StorageError;

/// Entrée retournée par `list` et `stat` — métadonnées d'un objet stocké.
#[derive(Debug, Clone, PartialEq)]
pub struct StorageEntry {
    /// Chemin relatif depuis la racine du storage.
    pub path: String,
    /// Taille en octets. `0` pour les répertoires.
    pub size: u64,
    /// Date de dernière modification en millisecondes epoch Unix.
    /// `None` si le backend ne fournit pas cette information.
    pub last_modified: Option<i64>,
    /// `true` si l'entrée est un répertoire (préfixe/dossier).
    pub is_dir: bool,
}

/// Trait d'abstraction storage — primitives async Read/Write/List/Delete/Stat.
///
/// Toutes les implémentations doivent être `Send + Sync` pour être utilisables
/// dans des contextes async multi-thread (Tokio).
///
/// ## Erreurs
///
/// - Entrée absente → `StorageError::NotFound`
/// - Chemin invalide → `StorageError::InvalidPath`
/// - Erreur backend → `StorageError::Io` ou `StorageError::OpenDal`
#[async_trait]
pub trait Storage: Send + Sync {
    /// Lit le contenu brut d'un objet au chemin relatif `path`.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::NotFound` si `path` n'existe pas.
    /// - `StorageError::Io` sur erreur lecture.
    async fn read(&self, path: &str) -> Result<Vec<u8>, StorageError>;

    /// Écrit `content` à `path`, créant les répertoires intermédiaires si nécessaire.
    ///
    /// # Effets de bord
    ///
    /// Écrase silencieusement le contenu existant si `path` existe déjà.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::Io` sur erreur écriture ou permission insuffisante.
    async fn write(&self, path: &str, content: &[u8]) -> Result<(), StorageError>;

    /// Supprime l'objet à `path`.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::NotFound` si `path` n'existe pas.
    /// - `StorageError::Io` sur erreur de suppression.
    async fn delete(&self, path: &str) -> Result<(), StorageError>;

    /// Liste les entrées dont le chemin commence par `prefix`.
    ///
    /// Le `prefix` peut être un répertoire (ex. `"notes/"`) ou une chaîne arbitraire.
    /// Les répertoires eux-mêmes peuvent apparaître dans le résultat (selon le backend).
    ///
    /// # Erreurs
    ///
    /// - `StorageError::Io` sur erreur de lecture du répertoire.
    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, StorageError>;

    /// Retourne les métadonnées de l'objet à `path`.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::NotFound` si `path` n'existe pas.
    /// - `StorageError::Io` sur erreur stat.
    async fn stat(&self, path: &str) -> Result<StorageEntry, StorageError>;

    /// Retourne `true` si un objet existe à `path`, `false` sinon.
    ///
    /// Équivalent optimisé de `stat(path).is_ok()` — peut éviter une allocation selon le backend.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::Io` sur erreur backend inattendue (pas NotFound).
    async fn exists(&self, path: &str) -> Result<bool, StorageError>;

    /// Crée le répertoire au chemin relatif `path` (idempotent).
    ///
    /// Le chemin doit se terminer par `/` (convention OpenDAL `create_dir`).
    /// L'opération est idempotente — aucune erreur si le répertoire existe déjà.
    ///
    /// # Erreurs
    ///
    /// - `StorageError::OpenDal` si le backend ne supporte pas `create_dir`.
    /// - `StorageError::Io` sur erreur filesystem (permissions, etc.).
    async fn create_dir(&self, path: &str) -> Result<(), StorageError>;
}
