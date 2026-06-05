//! Vault registry — création, ouverture et accès au vault root.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §5.1.
//!
//! ## Layout on-disk (spec §5.1)
//!
//! ```text
//! <root>/
//!   <tenant_id>/          ← notes Markdown (arborescence OpenDAL-friendly)
//!   .gradatum/
//!     config.toml         ← VaultConfig (section [vault], [embed], etc.)
//!     index.db            ← SQLite FTS5 + note_overrides + file_checksums
//!     audit.log           ← JSONL audit trail (double-stockage SIEM B12)
//!     overrides/
//!       <tenant_id>/      ← TOML overrides par note (arborescence ISO notes)
//! ```
//!
//! ## Invariants
//!
//! - `tenant_id` : non-vide, déduit de `VaultConfig.vault.default_tenant_id` ou `"main"`.
//! - NFS reject : `FileStorage::new()` vérifie `statfs(2)` (caveat C11).
//! - Idempotent : `create` est safe si les répertoires existent déjà (`create_dir_all`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gradatum_cache::{EffectiveNoteCache, EffectiveNoteCacheConfig};
use gradatum_core::config::VaultConfig;
use gradatum_core::error::GradatumError;
use gradatum_core::scope::VaultId;
use gradatum_index::SqliteIndex;
use gradatum_storage::{FileStorage, Storage as _};

use crate::error::VaultError;

/// Point d'entrée du vault Gradatum.
///
/// Contient les handles vers le storage, l'index SQLite et le cache moka.
/// Thread-safe — tous les champs internes sont `Arc` ou `Clone + Send + Sync`.
///
/// ## Usage typique
///
/// ```rust,no_run
/// use gradatum_vault::Vault;
/// use gradatum_core::scope::VaultId;
/// use std::path::Path;
///
/// // Créer un nouveau vault
/// # async fn example() {
/// let vault = Vault::create(Path::new("/my/vault"), VaultId::new("main")).await.unwrap();
///
/// // Ou ouvrir un vault existant
/// let vault = Vault::open(Path::new("/my/vault")).await.unwrap();
/// # }
/// ```
pub struct Vault {
    pub(crate) root: PathBuf,
    pub(crate) tenant_id: VaultId,
    pub(crate) config: VaultConfig,
    pub(crate) storage: FileStorage,
    pub(crate) index: Arc<SqliteIndex>,
    pub(crate) cache: Arc<EffectiveNoteCache>,
    /// Compteur de cache hits — incrémenté à chaque retour depuis le cache.
    ///
    /// Atomique pour permettre l'incrémentation depuis des refs `&self` (handlers async).
    /// Utilisé par les tests (`cache_hits()`) pour vérifier le comportement du cache.
    pub(crate) cache_hits: Arc<AtomicU64>,
}

impl Vault {
    /// Crée un nouveau vault à `root` avec le `tenant_id` fourni.
    ///
    /// ## Initialisation du layout
    ///
    /// Crée les répertoires suivants (idempotent — `create_dir_all` est safe si déjà présents) :
    /// - `<root>/<tenant_id>/`                       ← notes Markdown
    /// - `<root>/.gradatum/`                          ← metadata vault
    /// - `<root>/.gradatum/overrides/<tenant_id>/`    ← TOML overrides
    ///
    /// L'index SQLite est créé (et migré) via `SqliteIndex::open`.
    ///
    /// ## Erreurs
    ///
    /// - `VaultError::Core(GradatumError::VaultOnNfs)` si `root` est sur NFS (caveat C11).
    /// - `VaultError::Core(GradatumError::Storage(...))` sur erreur filesystem.
    pub async fn create(root: &Path, tenant_id: VaultId) -> Result<Self, VaultError> {
        // Construit le FileStorage — vérifie NFS (caveat C11) en premier.
        // FileStorage::new() appelle ensure_local_filesystem() avant tout I/O.
        let storage = FileStorage::new(root)
            .map_err(|e| GradatumError::Storage(format!("FileStorage init: {e}")))?;

        // Initialise l'arborescence layout spec §5.1 via OpenDAL (convergence v81 §6).
        // Les chemins OpenDAL sont relatifs à root et se terminent par `/` pour create_dir.
        storage
            .create_dir(&format!("{}/", tenant_id.as_str()))
            .await
            .map_err(|e| GradatumError::Storage(format!("create vault dir: {e}")))?;

        storage
            .create_dir(".gradatum/")
            .await
            .map_err(|e| GradatumError::Storage(format!("create .gradatum dir: {e}")))?;

        storage
            .create_dir(&format!(".gradatum/overrides/{}/", tenant_id.as_str()))
            .await
            .map_err(|e| GradatumError::Storage(format!("create overrides dir: {e}")))?;

        // Ouvre (ou crée) l'index SQLite avec migrations
        let index_path = root.join(".gradatum").join("index.db");
        let index = SqliteIndex::open(&index_path).await?;

        let config = VaultConfig::load_from_root(root).map_err(GradatumError::from)?;
        let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());

        Ok(Self {
            root: root.to_path_buf(),
            tenant_id,
            config,
            storage,
            index: Arc::new(index),
            cache: Arc::new(cache),
            cache_hits: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Ouvre un vault existant à `root`.
    ///
    /// ## Comportement
    ///
    /// - Charge la `VaultConfig` depuis `<root>/.gradatum/config.toml`.
    /// - Déduit le `tenant_id` depuis `config.vault.default_tenant_id` (défaut : `"main"`).
    /// - Ouvre l'index SQLite existant via `SqliteIndex::open` (applique les migrations manquantes).
    /// - Refuse le montage NFS via `FileStorage::new` (caveat C11).
    ///
    /// ## Erreurs
    ///
    /// - `VaultError::Core(GradatumError::VaultOnNfs)` si `root` est sur NFS.
    /// - `VaultError::Core(GradatumError::Config(...))` si le TOML est malformé.
    pub async fn open(root: &Path) -> Result<Self, VaultError> {
        let config = VaultConfig::load_from_root(root).map_err(GradatumError::from)?;
        let tenant_id = VaultId::new(
            config
                .vault
                .default_tenant_id
                .clone()
                .unwrap_or_else(|| "main".into()),
        );

        let storage = FileStorage::new(root)
            .map_err(|e| GradatumError::Storage(format!("FileStorage init: {e}")))?;

        let index_path = root.join(".gradatum").join("index.db");
        let index = SqliteIndex::open(&index_path).await?;

        let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());

        Ok(Self {
            root: root.to_path_buf(),
            tenant_id,
            config,
            storage,
            index: Arc::new(index),
            cache: Arc::new(cache),
            cache_hits: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Retourne le chemin racine du vault.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Retourne le `VaultId` (tenant_id) courant.
    pub fn tenant_id(&self) -> &VaultId {
        &self.tenant_id
    }

    /// Retourne la configuration chargée depuis `.gradatum/config.toml`.
    pub fn config(&self) -> &VaultConfig {
        &self.config
    }

    /// Retourne un handle partagé vers l'index SQLite.
    ///
    /// Exposé pour permettre aux tests d'intégration de vérifier l'état de l'index
    /// après une opération (ex. `write_note` → `get_content_hash`).
    pub fn index(&self) -> &std::sync::Arc<gradatum_index::SqliteIndex> {
        &self.index
    }

    /// Retourne le nombre de cache hits depuis la création du vault.
    ///
    /// Incrémenté par `read_note` à chaque retour depuis le cache (checksum valide).
    /// Utilisé dans les tests de performance et de comportement cache (T4 P2.0c).
    pub fn cache_hits(&self) -> u64 {
        self.cache_hits.load(Ordering::Relaxed)
    }
}
