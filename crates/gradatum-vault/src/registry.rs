//! Vault registry — creation, opening, and access to the vault root.
//!
//! ## On-disk layout
//!
//! ```text
//! <root>/
//!   <vault_id>/           ← Markdown notes (OpenDAL-friendly tree)
//!   .gradatum/
//!     config.toml         ← VaultConfig (sections [vault], [embed], etc.)
//!     index.db            ← SQLite FTS5 + note_overrides + file_checksums
//!     audit.log           ← JSONL audit trail (dual-storage SIEM)
//!     overrides/
//!       <vault_id>/       ← TOML overrides per note (mirrors note tree)
//! ```
//!
//! ## Invariants
//!
//! - `vault_id`: the physical namespace of the vault — NOT the authenticated principal,
//!   which is `TenantId`. Non-empty, derived from `VaultConfig.vault.default_tenant_id`
//!   or defaulting to `"main"`.
//! - NFS reject: `FileStorage::new()` checks `statfs(2)`.
//! - Idempotent: `create` is safe if directories already exist (`create_dir_all`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gradatum_cache::{EffectiveNoteCache, EffectiveNoteCacheConfig};
use gradatum_core::config::VaultConfig;
use gradatum_core::error::GradatumError;
use gradatum_core::paths::vault_dir_index_path;
use gradatum_core::scope::VaultId;
use gradatum_index::SqliteIndex;
use gradatum_storage::{FileStorage, Storage as _};

use crate::error::VaultError;

/// Entry point for a Gradatum vault.
///
/// Holds handles to storage, the SQLite index, and the moka cache.
/// Thread-safe — all internal fields are `Arc` or `Clone + Send + Sync`.
///
/// ## Typical usage
///
/// ```rust,no_run
/// use gradatum_vault::Vault;
/// use gradatum_core::scope::VaultId;
/// use std::path::Path;
///
/// // Create a new vault
/// # async fn example() {
/// let vault = Vault::create(Path::new("/my/vault"), VaultId::new("main")).await.unwrap();
///
/// // Or open an existing vault
/// let vault = Vault::open(Path::new("/my/vault")).await.unwrap();
/// # }
/// ```
pub struct Vault {
    pub(crate) root: PathBuf,
    /// Physical namespace of the vault served by this instance: the `<vault_id>/`
    /// directory on disk. This is the **namespace** dimension, NOT the authenticated
    /// **principal**, which is carried by `TenantId` in the `TrustContext`.
    pub(crate) vault_id: VaultId,
    pub(crate) config: VaultConfig,
    pub(crate) storage: FileStorage,
    pub(crate) index: Arc<SqliteIndex>,
    pub(crate) cache: Arc<EffectiveNoteCache>,
    /// Cache hit counter — incremented on each return from cache.
    ///
    /// Atomic to allow incrementing from `&self` references in async handlers.
    /// Used in tests (`cache_hits()`) to verify cache behaviour.
    pub(crate) cache_hits: Arc<AtomicU64>,
}

impl Vault {
    /// Creates a new vault at `root` with the supplied `vault_id` (namespace).
    ///
    /// ## Layout initialisation
    ///
    /// Creates the following directories (idempotent — `create_dir_all` is safe if already present):
    /// - `<root>/<vault_id>/`                        ← Markdown notes
    /// - `<root>/.gradatum/`                          ← vault metadata
    /// - `<root>/.gradatum/overrides/<vault_id>/`     ← TOML overrides
    ///
    /// The SQLite index is created (and migrated) via `SqliteIndex::open`.
    ///
    /// ## Errors
    ///
    /// - `VaultError::Core(GradatumError::VaultOnNfs)` if `root` is on NFS.
    /// - `VaultError::Core(GradatumError::Storage(...))` on filesystem error.
    pub async fn create(root: &Path, vault_id: VaultId) -> Result<Self, VaultError> {
        // Construit le FileStorage — vérifie NFS (caveat C11) en premier.
        // FileStorage::new() appelle ensure_local_filesystem() avant tout I/O.
        let storage = FileStorage::new(root)
            .map_err(|e| GradatumError::Storage(format!("FileStorage init: {e}")))?;

        // Initialise l'arborescence layout spec §5.1 via OpenDAL (convergence v81 §6).
        // Les chemins OpenDAL sont relatifs à root et se terminent par `/` pour create_dir.
        storage
            .create_dir(&format!("{}/", vault_id.as_str()))
            .await
            .map_err(|e| GradatumError::Storage(format!("create vault dir: {e}")))?;

        storage
            .create_dir(".gradatum/")
            .await
            .map_err(|e| GradatumError::Storage(format!("create .gradatum dir: {e}")))?;

        storage
            .create_dir(&format!(".gradatum/overrides/{}/", vault_id.as_str()))
            .await
            .map_err(|e| GradatumError::Storage(format!("create overrides dir: {e}")))?;

        // SSOT : index_path via helper canonique — jamais root.join(".gradatum").join("index.db").
        // `root` ici est le répertoire vault/ (vault_dir), donc vault_dir_index_path.
        let index_path = vault_dir_index_path(root);
        let index = SqliteIndex::open(&index_path).await?;

        let config = VaultConfig::load_from_root(root).map_err(GradatumError::from)?;
        let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());

        Ok(Self {
            root: root.to_path_buf(),
            vault_id,
            config,
            storage,
            index: Arc::new(index),
            cache: Arc::new(cache),
            cache_hits: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Opens an existing vault at `root`.
    ///
    /// ## Behaviour
    ///
    /// - Loads `VaultConfig` from `<root>/.gradatum/config.toml`.
    /// - Derives `vault_id` (namespace) from `config.vault.default_tenant_id` (default: `"main"`).
    /// - Opens the existing SQLite index via `SqliteIndex::open` (applies any pending migrations).
    /// - Rejects NFS mounts via `FileStorage::new`.
    ///
    /// ## Errors
    ///
    /// - `VaultError::Core(GradatumError::VaultOnNfs)` if `root` is on NFS.
    /// - `VaultError::Core(GradatumError::Config(...))` if the TOML is malformed.
    pub async fn open(root: &Path) -> Result<Self, VaultError> {
        let config = VaultConfig::load_from_root(root).map_err(GradatumError::from)?;
        let vault_id = VaultId::new(
            config
                .vault
                .default_tenant_id
                .clone()
                .unwrap_or_else(|| "main".into()),
        );

        let storage = FileStorage::new(root)
            .map_err(|e| GradatumError::Storage(format!("FileStorage init: {e}")))?;

        // SSOT : index_path via helper canonique — jamais root.join(".gradatum").join("index.db").
        // `root` ici est le répertoire vault/ (vault_dir), donc vault_dir_index_path.
        let index_path = vault_dir_index_path(root);
        let index = SqliteIndex::open(&index_path).await?;

        let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());

        Ok(Self {
            root: root.to_path_buf(),
            vault_id,
            config,
            storage,
            index: Arc::new(index),
            cache: Arc::new(cache),
            cache_hits: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Opens a `Vault` instance reusing a **shared** SQLite index handle.
    ///
    /// Unlike [`Vault::create`] and [`Vault::open`], which each open a SEPARATE
    /// `SqliteIndex` pool over `index.db`, this constructor INJECTS an already-open
    /// `Arc<SqliteIndex>`. It is the building block of the multi-vault handle registry:
    /// every handle of a given application state shares THE SAME pool over the single
    /// `index.db`, partitioning being provided by the `vault_id` column and the composite
    /// primary key `(vault_id, id)`. Without that sharing, N handles would open N pools
    /// over the same database.
    ///
    /// The physical namespace (`<root>/<vault_id>/` and `overrides/<vault_id>/`) is
    /// materialised idempotently. The `.gradatum/` directory (configuration and index) is
    /// already present, since the index is shared, and is NOT recreated.
    ///
    /// ## Isolation gate preserved
    ///
    /// The supplied `vault_id` IS the namespace served by the instance (`self.vault_id`,
    /// fixed at startup). The isolation gate `Vault::ensure_witness_owns_vault` compares
    /// the TARGET witness derived from the JWT against that `self.vault_id`, and sharing
    /// the pool does not weaken it: each handle stays bound to exactly one vault.
    /// Provisioning is fail-closed — the caller MUST derive `vault_id` from a trusted
    /// source ([`VaultId::parse`] at an untrusted boundary), and inserting into the
    /// registry re-checks that the key matches `vault.vault_id()`.
    ///
    /// ## Errors
    ///
    /// - `VaultError::Core(GradatumError::VaultOnNfs)` if `root` sits on NFS.
    /// - `VaultError::Core(GradatumError::Storage(...))` on a filesystem error.
    /// - `VaultError::Core(GradatumError::Config(...))` if `config.toml` is malformed.
    #[must_use = "a constructed Vault must be used or registered, otherwise the namespace is provisioned for nothing"]
    pub async fn with_shared_index(
        root: &Path,
        vault_id: VaultId,
        index: Arc<SqliteIndex>,
    ) -> Result<Self, VaultError> {
        // FileStorage::new() vérifie NFS (caveat C11) avant tout I/O.
        let storage = FileStorage::new(root)
            .map_err(|e| GradatumError::Storage(format!("FileStorage init: {e}")))?;

        // Matérialise le namespace md du vault (idempotent) — le `.gradatum/`/index.db est
        // déjà ouvert (partagé), on ne le recrée pas.
        storage
            .create_dir(&format!("{}/", vault_id.as_str()))
            .await
            .map_err(|e| GradatumError::Storage(format!("create vault dir: {e}")))?;
        storage
            .create_dir(&format!(".gradatum/overrides/{}/", vault_id.as_str()))
            .await
            .map_err(|e| GradatumError::Storage(format!("create overrides dir: {e}")))?;

        let config = VaultConfig::load_from_root(root).map_err(GradatumError::from)?;
        let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig::default());

        Ok(Self {
            root: root.to_path_buf(),
            vault_id,
            config,
            storage,
            // Pool PARTAGÉ — PAS de `SqliteIndex::open` : un seul pool sur `index.db`.
            index,
            cache: Arc::new(cache),
            cache_hits: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Returns the vault root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the `VaultId` **namespace** served by this instance.
    ///
    /// This is the **namespace** dimension — the `<vault_id>/` directory on disk — NOT the
    /// authenticated **principal**, which is modelled by `TenantId` and lives in the
    /// `TrustContext` derived from the JWT, never here.
    pub fn vault_id(&self) -> &VaultId {
        &self.vault_id
    }

    /// Returns the configuration loaded from `.gradatum/config.toml`.
    pub fn config(&self) -> &VaultConfig {
        &self.config
    }

    /// Returns a shared handle to the SQLite index.
    ///
    /// Exposed to allow integration tests to verify index state after an operation
    /// (e.g. `write_note` → `get_content_hash`).
    pub fn index(&self) -> &std::sync::Arc<gradatum_index::SqliteIndex> {
        &self.index
    }

    /// Returns the number of cache hits since the vault was created.
    ///
    /// Incremented by `read_note` on each return from cache (valid checksum).
    /// Used in performance and cache-behaviour tests.
    pub fn cache_hits(&self) -> u64 {
        self.cache_hits.load(Ordering::Relaxed)
    }

    /// Returns a handle to the underlying storage.
    ///
    /// Exposed to allow integration tests to verify the existence and physical
    /// location of a `.md` after a relocation operation (`move_locus`).
    /// Read-only — tests do not mutate storage directly.
    pub fn storage(&self) -> &FileStorage {
        &self.storage
    }
}
