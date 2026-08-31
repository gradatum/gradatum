//! Shared application state — Arc-able and cloneable across handlers.
//!
//! `AppState::search` is an `Arc<dyn Index>` — wired via `with_search_path` in production
//! and backed by an in-memory placeholder in sync constructors.
//! `AppState::vault` is `Arc<dyn gradatum_vault::Registry>` — wired via
//! `with_vault_path` in production and `PlaceholderRegistry` in sync constructors.
//!
//! `AppState::jwt` (`JwtService`) and `AppState::acl` (`AclEngine`) are real
//! implementations — initialised with safe defaults for tests.
//!
//! `AppState::audit` defaults to `NoopAuditSink` — wired via `with_audit_dir`
//! in production to produce rotating JSONL files.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Instant, SystemTime};

use gradatum_embed::{Embedder, Noop as NoopEmbedder};

use async_trait::async_trait;
use gradatum_acl_auth::{ApiKeyStore, SqliteApiKeyStore, has_write_scope};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_auth::revocation::{InMemoryRevocationStore, RevocationStore};
use gradatum_core::QueueStore;
use gradatum_core::audit::http::AuditSink;
use gradatum_core::error::GradatumError;
use gradatum_vault::Registry;

use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;

use crate::event_log_store::EventLogStore;
use crate::mcp_usage::McpToolCounters;
use crate::metrics::AppMetrics;
use crate::note_usage_store::{NoteUsageStore, UsageKey, UsageValue};
use crate::read_usage_store::ReadUsageCounterStore;

// ── NoopAuditSink (state.rs) ──────────────────────────────────────────────────

/// No-op audit sink for tests and sync constructors.
///
/// Replaced by `JsonlFileSink` in production via `AppState::with_audit_dir`.
/// Distinct from the `NoopAuditSink` in `gradatum-worker` — each crate keeps its own
/// to avoid cross-crate coupling.
struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    /// Does nothing — events are silently discarded.
    async fn record(
        &self,
        _event: gradatum_core::audit::http::HttpAuditEvent,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}

// ── Placeholders internes ──────────────────────────────────────────────────────

/// Initialises an in-memory `SqliteIndex` for sync constructors.
///
/// Called by `with_jwt` and `with_jwt_and_acl` to populate the `search` field
/// before injection via `with_search_path`.
///
/// Uses a dedicated thread with its own Tokio runtime to work around the
/// `block_on` restriction inside a `current_thread` runtime (used by `#[tokio::test]`).
/// The thread is joined immediately — no leak.
///
/// Returns `Arc<dyn Index>` — the concrete `SqliteIndex` type is erased immediately
/// to match the `AppState.search: Arc<dyn Index>` field.
///
/// # Panics
///
/// Panics only if the in-memory SQLite initialisation fails — impossible in practice.
fn placeholder_search() -> Arc<dyn Index> {
    let idx = std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("mini-runtime placeholder_search — invariant build")
            .block_on(SqliteIndex::open_in_memory())
            .expect("SqliteIndex::open_in_memory() placeholder — invariant init search")
    })
    .join()
    .expect("thread join placeholder_search — invariant thread");
    Arc::new(idx) as Arc<dyn Index>
}

// ── NoopQueueStore — placeholder QueueStore (F-16) ───────────────────────────

/// Placeholder [`QueueStore`] for sync constructors.
///
/// Returns `NotFound` or empty on all operations.
/// Replaced in production by `GradatumQueue` via `with_job_store`.
///
/// Private to `state.rs` — not exposed in the crate's public API.
struct NoopQueueStore;

#[async_trait]
impl QueueStore for NoopQueueStore {
    async fn enqueue(
        &self,
        _job: gradatum_core::JobRecord,
    ) -> Result<ulid::Ulid, gradatum_core::QueueError> {
        Err(gradatum_core::QueueError::Storage(
            "NoopQueueStore: no job_store wired — use with_job_store".into(),
        ))
    }

    async fn dequeue(
        &self,
        _tenant_filter: Option<&str>,
    ) -> Result<Option<gradatum_core::JobRecord>, gradatum_core::QueueError> {
        Ok(None)
    }

    async fn get(
        &self,
        _id: ulid::Ulid,
        _tenant_filter: Option<&str>,
    ) -> Result<Option<gradatum_core::JobRecord>, gradatum_core::QueueError> {
        Ok(None)
    }

    async fn complete(
        &self,
        _id: ulid::Ulid,
        _result: gradatum_core::JobResult,
    ) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn fail(
        &self,
        _id: ulid::Ulid,
        _err: &str,
        _attempt: u32,
    ) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn cancel(
        &self,
        _id: ulid::Ulid,
        _tenant_filter: Option<&str>,
    ) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn fail_dlq(&self, _id: ulid::Ulid, _err: &str) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn find_awaiting(
        &self,
        _job_id: ulid::Ulid,
    ) -> Result<Vec<gradatum_core::JobRecord>, gradatum_core::QueueError> {
        Ok(vec![])
    }

    async fn set_pending(&self, _id: ulid::Ulid) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn recover_stale_leases(
        &self,
        _ttl: std::time::Duration,
    ) -> Result<Vec<ulid::Ulid>, gradatum_core::QueueError> {
        Ok(vec![])
    }

    async fn cancel_expired_deadlines(
        &self,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<ulid::Ulid>, gradatum_core::QueueError> {
        Ok(vec![])
    }

    async fn promote_retries(
        &self,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<ulid::Ulid>, gradatum_core::QueueError> {
        Ok(vec![])
    }

    async fn schedule_retry(
        &self,
        _id: ulid::Ulid,
        _at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn list(
        &self,
        _filter: gradatum_core::JobFilter,
    ) -> Result<Vec<gradatum_core::JobRecord>, gradatum_core::QueueError> {
        Ok(vec![])
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<gradatum_core::QueueEvent> {
        // Canal vide — aucun event émis par le noop store.
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(tx);
        rx
    }
}

// ── NoopApiKeyStore ───────────────────────────────────────────────────────────

/// No-op API key store for sync constructors and tests that do not exercise
/// the `/auth/exchange` flow.
///
/// Every verification attempt returns `NotFound`. Replaced in production
/// by `SqliteApiKeyStore` via `AppState::with_api_keys_path`.
///
/// Private to `state.rs` — not exposed in the crate's public API.
struct NoopApiKeyStore;

#[async_trait]
impl ApiKeyStore for NoopApiKeyStore {
    async fn create(
        &self,
        _owner: &gradatum_core::scope::AgentId,
        _scopes: Vec<String>,
        _tenant_id: String,
        _description: Option<String>,
    ) -> Result<gradatum_acl_auth::ApiKeyMaterial, gradatum_acl_auth::ApiKeyError> {
        Err(gradatum_acl_auth::ApiKeyError::Crypto(
            "NoopApiKeyStore: create() not supported — wire SqliteApiKeyStore via with_api_keys_path".into(),
        ))
    }

    async fn verify(
        &self,
        _secret: &str,
    ) -> Result<gradatum_acl_auth::ApiKey, gradatum_acl_auth::ApiKeyError> {
        Err(gradatum_acl_auth::ApiKeyError::NotFound)
    }

    async fn list(
        &self,
        _include_revoked: bool,
        _tenant_filter: Option<&str>,
    ) -> Result<Vec<gradatum_acl_auth::ApiKey>, gradatum_acl_auth::ApiKeyError> {
        Ok(vec![])
    }

    async fn revoke(&self, _prefix: &str) -> Result<(), gradatum_acl_auth::ApiKeyError> {
        Err(gradatum_acl_auth::ApiKeyError::NotFound)
    }

    async fn rotate(
        &self,
        _prefix: &str,
    ) -> Result<gradatum_acl_auth::ApiKeyMaterial, gradatum_acl_auth::ApiKeyError> {
        Err(gradatum_acl_auth::ApiKeyError::NotFound)
    }

    /// Un magasin noop signifie « la gestion des clés n'est pas le sujet ici »,
    /// pas « le registre est vierge ». Le corps par défaut du trait dérive la
    /// réponse de `list` (qui rend `vec![]`) et conclurait donc à `Ok(false)` —
    /// une installation non provisionnée. Or `reject_unauthenticated`
    /// (middleware) est le seul consommateur : `Ok(false)` y déclenche le 503
    /// d'amorçage (R5) pour toute requête non authentifiée, même dans un test
    /// qui n'a rien à voir avec le provisioning. On renvoie donc `Ok(true)` —
    /// « comporte-toi comme une installation provisionnée » — ce qui restaure le
    /// refus d'auth ordinaire (401) attendu du chemin legacy.
    ///
    /// La divergence `has_any_active() == Ok(true)` vs `list() == Ok(vec![])` est
    /// assumée et sans effet : le noop n'est jamais câblé en production
    /// (`AppState::with_api_keys_path` le remplace par `SqliteApiKeyStore`), et
    /// la discrimination réelle 503/401/fail-closed du contrat R5 est prouvée par
    /// les tests `SpyApiKeyStore` (`middleware.rs`), qui pilotent l'état du
    /// registre explicitement plutôt que via ce stub.
    async fn has_any_active(&self) -> Result<bool, gradatum_acl_auth::ApiKeyError> {
        Ok(true)
    }
}

// ── PlaceholderRegistry ───────────────────────────────────────────────────────

/// Registry placeholder for sync constructors (`with_jwt`, `with_jwt_and_acl`).
///
/// Returns 0/0 for `tenant_count`/`locus_count` and no-ops `ensure_tenant`.
/// Replaced immediately by `Vault` via `with_vault_path` in production.
///
/// Private to `state.rs` — not exposed in the crate's public API.
/// Allows sync constructors to initialise `AppState` before async injection.
#[derive(Debug, Clone)]
struct PlaceholderRegistry;

#[async_trait]
impl Registry for PlaceholderRegistry {
    async fn tenant_count(&self) -> Result<u32, GradatumError> {
        // Placeholder : retourne 0 — signale l'absence de vault réel.
        // En prod, ce chemin est impossible car `with_vault_path` est toujours
        // appelé avant tout accès aux counts.
        Ok(0)
    }

    async fn locus_count(&self) -> Result<u32, GradatumError> {
        Ok(0)
    }

    async fn ensure_tenant(&self, _tenant_id: &str) -> Result<(), GradatumError> {
        Ok(())
    }

    async fn read_note_by_id(
        &self,
        note_id: &str,
    ) -> Result<gradatum_core::note::Note, GradatumError> {
        // Placeholder : retourne NoteNotFound — en prod `with_vault_path` injecte Vault réel.
        use gradatum_core::identity::NoteId;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("placeholder: invalid ULID: {e}")))?;
        Err(GradatumError::NoteNotFound(NoteId(ulid)))
    }

    async fn history_versions(&self, _note_id: &str) -> Result<Vec<i64>, GradatumError> {
        // Placeholder — pas de vault réel.
        Ok(Vec::new())
    }

    async fn history_get(
        &self,
        note_id: &str,
        _ts_ms: i64,
    ) -> Result<gradatum_core::note::Note, GradatumError> {
        use gradatum_core::identity::NoteId;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("placeholder: invalid ULID: {e}")))?;
        Err(GradatumError::NoteNotFound(NoteId(ulid)))
    }

    async fn history_restore(
        &self,
        _checked: &gradatum_core::scope::AclCheckedVaultId,
        _note_id: &str,
        _ts_ms: i64,
    ) -> Result<String, GradatumError> {
        Err(GradatumError::Storage(
            "history_restore: placeholder vault — injection required".to_string(),
        ))
    }

    async fn history_diff(
        &self,
        _note_id: &str,
        _a: &str,
        _b: &str,
    ) -> Result<Vec<String>, GradatumError> {
        Err(GradatumError::Storage(
            "history_diff: placeholder vault — injection required".to_string(),
        ))
    }

    async fn update_note_status(
        &self,
        _checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        _target: gradatum_core::status::NoteStatus,
        _reason: Option<String>,
    ) -> Result<(), GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        use gradatum_core::identity::NoteId;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("placeholder: invalid ULID: {e}")))?;
        Err(GradatumError::NoteNotFound(NoteId(ulid)))
    }

    async fn add_tags(
        &self,
        _checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        _tags: &[String],
    ) -> Result<(), GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        use gradatum_core::identity::NoteId;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("placeholder: invalid ULID: {e}")))?;
        Err(GradatumError::NoteNotFound(NoteId(ulid)))
    }

    async fn move_locus(
        &self,
        _checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        _new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        use gradatum_core::identity::NoteId;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("placeholder: invalid ULID: {e}")))?;
        Err(GradatumError::NoteNotFound(NoteId(ulid)))
    }

    async fn write_note_with_id_internal(
        &self,
        _checked: &gradatum_core::scope::AclCheckedVaultId,
        _frontmatter: gradatum_core::frontmatter::Frontmatter,
        _body: String,
        _id: gradatum_core::identity::NoteId,
    ) -> Result<gradatum_core::note::Note, GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        Err(GradatumError::Storage(
            "write_note_with_id_internal: placeholder vault — injection required".to_string(),
        ))
    }

    async fn delete_note_by_id(
        &self,
        id: gradatum_core::identity::NoteId,
    ) -> Result<(), GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        Err(GradatumError::NoteNotFound(id))
    }

    async fn archive_note_by_id(
        &self,
        id: gradatum_core::identity::NoteId,
        _archived_by: Option<String>,
        _gc_due_ms: i64,
    ) -> Result<gradatum_vault::ArchiveOutcome, GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        Err(GradatumError::NoteNotFound(id))
    }

    async fn run_archive_gc(&self, _now_ms: i64, _limit: usize) -> Result<u64, GradatumError> {
        // Placeholder — aucun vault réel, rien à collecter.
        Ok(0)
    }

    async fn list_archives(
        &self,
        _filter: &gradatum_index::ArchiveListFilter,
    ) -> Result<Vec<gradatum_index::ArchiveEntry>, GradatumError> {
        // Placeholder — aucun vault réel, registre vide.
        Ok(Vec::new())
    }

    async fn get_active_archive(
        &self,
        _note_id: &str,
    ) -> Result<Option<gradatum_index::ArchiveEntry>, GradatumError> {
        // Placeholder — aucun vault réel, aucune archive.
        Ok(None)
    }

    async fn purge_archive_by_id(&self, _note_id: &str) -> Result<bool, GradatumError> {
        // Placeholder — aucun vault réel, rien à purger.
        Ok(false)
    }

    async fn restore_archive_by_id(
        &self,
        _note_id: &str,
    ) -> Result<gradatum_vault::RestoreOutcome, GradatumError> {
        // Placeholder — aucun vault réel, aucune archive à restaurer.
        Err(GradatumError::Storage(
            "restore_archive_by_id: placeholder vault — injection required".to_string(),
        ))
    }
}

// ── VaultRegistry — registre de handles multi-vault (GAP-1, W3) ───────────────

/// Registre de handles [`gradatum_vault::Vault`] indexés par
/// [`gradatum_core::scope::VaultId`] — design cible du routage multi-vault.
///
/// À flag `multi_tenant` OFF, le registre LIVE contient EXACTEMENT `{main}` (singleton,
/// byte-identical). Tous les handles partagent le MÊME `Arc<SqliteIndex>` (un seul pool sur
/// `index.db`) — la partition est assurée par la colonne `vault_id` (PK composite
/// `(vault_id, id)`). Le 2e vault n'est instancié qu'en test ; il n'existe LIVE que si le
/// flag `multi_tenant` est activé, ce qui est INTERDIT dans le Groupe B.
///
/// `BTreeMap` (pas `HashMap`) : ordre d'itération déterministe (ADN 2 — un
/// `list_active_vaults` futur doit être stable).
#[derive(Default)]
pub struct VaultRegistry {
    /// Mutabilité **intérieure** (`RwLock`) — la registration runtime d'un 2e vault
    /// (`handle_admin_vault_create`, flag ON) mute le registre PARTAGÉ derrière
    /// `Arc<VaultRegistry>` sans changer le type du champ `AppState.vaults` ni la signature
    /// de [`resolve`](Self::resolve) (consommée en prod). `std::sync::RwLock` (pas
    /// `tokio`) : les sections critiques sont **synchrones et sans `.await`** (resolve/get
    /// clonent l'`Arc` puis relâchent le guard ; insert/add_vault sont non-async) —
    /// `anti-lock-across-await` respecté (ADN 2). `BTreeMap` : ordre d'itération
    /// déterministe (un `list_active_vaults` futur doit être stable).
    handles: std::sync::RwLock<
        std::collections::BTreeMap<gradatum_core::scope::VaultId, Arc<gradatum_vault::Vault>>,
    >,
}

/// Erreur d'insertion dans le [`VaultRegistry`] — fail-closed provisioning.
// Consommé par la crate de test externe + le provisioning multi-vault (Task 14/18) ;
// invisible au dead_code lint du binaire tant que ces consommateurs ne sont pas mergés.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum VaultRegistryError {
    /// Le `vault_id` réel du handle (`vault.vault_id()`, dérivé du `config.toml` on-disk)
    /// diverge de la clé de routage attendue → insertion REFUSÉE. Sans ce refus, un
    /// `config.toml` silencieusement incohérent ré-ouvrirait la classe cross-vault (un handle
    /// servirait un namespace différent de celui sous lequel il est routé).
    #[error("vault_id mismatch: expected key {expected}, actual handle identity {actual}")]
    VaultIdMismatch {
        /// Clé de routage sous laquelle l'insertion a été tentée.
        expected: gradatum_core::scope::VaultId,
        /// Identité réelle du handle (`vault.vault_id()`).
        actual: gradatum_core::scope::VaultId,
    },
}

// Les accesseurs `insert`/`get`/`len`/`is_empty` ne sont consommés (à ce stade du train)
// que par la crate de test externe + le routage multi-vault à venir (Task 14/18) : allow
// dead_code au niveau impl, aligné sur la convention du fichier (cf. `with_jwt_and_acl`).
#[allow(dead_code)]
impl VaultRegistry {
    /// Registre vide.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registre singleton depuis un unique handle (câblage LIVE `{main}`).
    ///
    /// La clé est l'identité réelle du handle (`vault.vault_id()`) — aucune divergence
    /// possible par construction.
    #[must_use]
    pub fn singleton(vault: Arc<gradatum_vault::Vault>) -> Self {
        let mut handles = std::collections::BTreeMap::new();
        handles.insert(vault.vault_id().clone(), vault);
        Self {
            handles: std::sync::RwLock::new(handles),
        }
    }

    /// Verrou lecture, **résilient au poison** : un lock empoisonné (panic d'un writer)
    /// ne doit pas bloquer le routage — on récupère la map via `into_inner`. Aucun panic
    /// n'est atteignable sous le guard (aucun `.await`, aucun point de panic), donc le
    /// poison est en pratique inaccessible ; la récupération est une défense en profondeur.
    fn read_handles(
        &self,
    ) -> std::sync::RwLockReadGuard<
        '_,
        std::collections::BTreeMap<gradatum_core::scope::VaultId, Arc<gradatum_vault::Vault>>,
    > {
        self.handles
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Verrou écriture, résilient au poison (cf. [`read_handles`](Self::read_handles)).
    fn write_handles(
        &self,
    ) -> std::sync::RwLockWriteGuard<
        '_,
        std::collections::BTreeMap<gradatum_core::scope::VaultId, Arc<gradatum_vault::Vault>>,
    > {
        self.handles
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Vérifie que l'identité réelle du handle (`vault.vault_id()`) coïncide avec la clé de
    /// routage `expected` — fail-closed partagé par [`insert`](Self::insert) et
    /// [`add_vault`](Self::add_vault).
    fn ensure_identity(
        expected: &gradatum_core::scope::VaultId,
        vault: &Arc<gradatum_vault::Vault>,
    ) -> Result<(), VaultRegistryError> {
        if vault.vault_id() != expected {
            return Err(VaultRegistryError::VaultIdMismatch {
                actual: vault.vault_id().clone(),
                expected: expected.clone(),
            });
        }
        Ok(())
    }

    /// Insère un handle sous la clé `expected`, **fail-closed** sur divergence d'identité.
    ///
    /// Prend `&self` (mutabilité intérieure) : insertion runtime dans le registre partagé.
    ///
    /// # Errors
    ///
    /// [`VaultRegistryError::VaultIdMismatch`] si `vault.vault_id() != expected` — le handle
    /// n'est alors PAS enregistré (aucun effet de bord).
    pub fn insert(
        &self,
        expected: gradatum_core::scope::VaultId,
        vault: Arc<gradatum_vault::Vault>,
    ) -> Result<(), VaultRegistryError> {
        Self::ensure_identity(&expected, &vault)?;
        self.write_handles().insert(expected, vault);
        Ok(())
    }

    /// Ajoute un handle au registre — **idempotent** (ADN 2) et **fail-closed** (ADN 5).
    ///
    /// Constructeur multi-handle additif, consommé par le provisioning d'un 2e
    /// vault (`handle_admin_vault_create`) et le bootstrap N vaults. Le chemin
    /// singleton prod ([`singleton`](Self::singleton)) reste inchangé.
    ///
    /// - **Idempotent** : si `expected` est déjà enregistré, le 2e appel est un no-op (un
    ///   provisioning rejoué au boot ou un retry admin ne remplace pas un handle vivant —
    ///   ADN 2 : « 2e appel sans force = no-op »).
    /// - **Fail-closed** : sinon, refuse ([`VaultRegistryError::VaultIdMismatch`]) tout
    ///   handle dont l'identité réelle diverge de la clé attendue (aucun effet de bord).
    ///
    /// Prend `&self` (mutabilité intérieure). Check + insertion sous **un seul** verrou
    /// écriture → atomique (pas de fenêtre TOCTOU entre le test d'idempotence et l'insert).
    ///
    /// # Errors
    ///
    /// [`VaultRegistryError::VaultIdMismatch`] si le handle a un `vault_id` divergent de
    /// `expected`.
    pub fn add_vault(
        &self,
        expected: gradatum_core::scope::VaultId,
        vault: Arc<gradatum_vault::Vault>,
    ) -> Result<(), VaultRegistryError> {
        Self::ensure_identity(&expected, &vault)?;
        let mut handles = self.write_handles();
        // Idempotence : clé déjà enregistrée (identité déjà validée == expected) → no-op.
        if handles.contains_key(&expected) {
            return Ok(());
        }
        handles.insert(expected, vault);
        Ok(())
    }

    /// Retourne un **clone** du handle du vault `vault_id`, ou `None` s'il n'est pas
    /// enregistré.
    ///
    /// Clone l'`Arc` (bon marché) : le guard de lecture est relâché à la sortie, aucun
    /// borrow ne franchit un `.await` chez l'appelant (`anti-lock-across-await` / ADN 2).
    /// Point d'accès consommé par le routage des reads et le re-check TOCTOU purge :
    /// ils résolvent le handle du vault EFFECTIF au lieu du singleton `main`.
    #[must_use]
    pub fn get(
        &self,
        vault_id: &gradatum_core::scope::VaultId,
    ) -> Option<Arc<gradatum_vault::Vault>> {
        self.read_handles().get(vault_id).cloned()
    }

    /// Résout le handle du vault EFFECTIF sous forme de façade [`gradatum_vault::Registry`],
    /// **fail-closed** : un vault absent du registre → [`GradatumError::VaultNotFound`],
    /// JAMAIS un repli silencieux sur le singleton `main` (évite la classe split-brain
    /// read-back : le mark est scopé mais le read/write-back passait par le singleton).
    ///
    /// L'`Arc` est **cloné** (coercition `Arc<Vault>` → `Arc<dyn Registry>`) : le read-back
    /// est asynchrone (`.await`), le handle résolu doit survivre à travers le point de
    /// suspension sans retenir de borrow sur le registre (`anti-lock-across-await` / ADN 2).
    ///
    /// # Errors
    ///
    /// [`GradatumError::VaultNotFound`] si `vault_id` n'est pas enregistré — fail-closed,
    /// mappé en 500 côté handler (`err_to_status`), aucun oracle sur l'état interne.
    #[must_use = "the resolved handle must be used for the scoped read-back"]
    pub fn resolve(
        &self,
        vault_id: &gradatum_core::scope::VaultId,
    ) -> Result<Arc<dyn gradatum_vault::Registry>, gradatum_core::error::GradatumError> {
        self.get(vault_id)
            .map(|v| v as Arc<dyn gradatum_vault::Registry>)
            .ok_or_else(|| gradatum_core::error::GradatumError::VaultNotFound(vault_id.clone()))
    }

    /// Nombre de handles enregistrés.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read_handles().len()
    }

    /// `true` si le registre ne contient aucun handle.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read_handles().is_empty()
    }
}

// ── ReadUsageAccumulators ─────────────────────────────────────────────────────

/// Compteurs AtomicU64 en mémoire pour les 5 read-paths instrumentés (télémétrie v0.5.3).
///
/// ## Design
///
/// Chaque handler read-path incrémente son compteur via `fetch_add(1, Ordering::Relaxed)`.
/// Coût hot-path : ~0 (instruction atomique, aucun I/O).
///
/// La tâche flush (interval 60s dans `main.rs`) swaps les compteurs à 0 (`swap(0, Relaxed)`)
/// et flushes les valeurs accumulées dans `read_usage_counters` (UPSERT par window_h).
///
/// ## Thread-safety
///
/// `Arc<ReadUsageAccumulators>` — partagé entre tous les handlers et la tâche flush.
/// `AtomicU64` est `Send + Sync` — safe pour un partage multi-thread.
///
/// ## Ordering::Relaxed
///
/// Suffisant ici : seule la valeur du compteur compte.
/// Aucun ordering cross-thread requis (pas d'effets mémoire à synchroniser avec le
/// flush — le swap-et-reset est l'unique point de synchronisation, géré par atomicité).
#[derive(Debug, Default)]
pub struct ReadUsageAccumulators {
    /// Compteur de hits pour `POST /api/v1/vault_search`.
    pub vault_search: AtomicU64,
    /// Compteur de hits pour `POST /api/v1/vault_read`.
    pub vault_read: AtomicU64,
    /// Compteur de hits pour `POST /api/v1/code_scope`.
    pub code_scope: AtomicU64,
    /// Compteur de hits pour `POST /api/v1/vault_timeline`.
    pub vault_timeline: AtomicU64,
    /// Compteur de hits pour `GET /api/v1/lessons/recall`.
    pub lessons_recall: AtomicU64,
}

// ── NoteUsageAccumulators (F-110 Phase 1) ─────────────────────────────────────

/// Accumulateur d'usage PAR NOTE en mémoire (salience per note).
///
/// ## Design
///
/// Jumeau per-note de [`ReadUsageAccumulators`] : là où celui-ci a 5 `AtomicU64` fixes
/// (granularité endpoint), la clé d'usage par note `(tenant_id, note_id, kind)` est
/// dynamique — un `Mutex<HashMap>` std remplace donc les atomiques. Les handlers
/// enregistrent via [`NoteUsageAccumulators::record`] (verrou court, O(1)), la tâche
/// flush 60 s vide la map via [`NoteUsageAccumulators::swap`] puis UPSERT dans
/// `note_usage` (migration 0029).
///
/// ## Thread-safety & best-effort
///
/// `Arc<NoteUsageAccumulators>` partagé entre handlers et tâche flush. Le verrou n'est
/// JAMAIS tenu à travers un `.await` (opérations map synchrones uniquement). Sur verrou
/// empoisonné (panic d'un autre thread), on récupère la donnée (`into_inner`) plutôt que
/// de propager : c'est de la télémétrie best-effort, un incrément perdu n'est jamais fatal.
#[derive(Debug, Default)]
pub struct NoteUsageAccumulators {
    /// Map `(vault_id, note_id, kind) → (count, last_used_ms)`.
    ///
    /// Dimension (`note_usage`) : la première composante de la clé
    /// est le **vault namespace** (colonne SQL legacy `tenant_id`), PAS le principal JWT.
    /// Les call-sites (`vault_search_impl` → `read_vault`, `vault_read_impl` →
    /// `note.frontmatter.vault_id`) fournissent le vault effectif ; `flush_batch` persiste
    /// cette chaîne telle quelle. À flag OFF `vault == principal == "main"` (byte-identical).
    ///
    /// `std::sync::Mutex` (pas Tokio) : les sections critiques sont purement synchrones
    /// et brèves (insert/increment) — aucun `.await` sous verrou.
    inner: std::sync::Mutex<std::collections::HashMap<UsageKey, UsageValue>>,
}

impl NoteUsageAccumulators {
    /// Enregistre un usage `+1` pour `(tenant_id, note_id, kind)`, `last_used_ms = max`.
    ///
    /// O(1) amorti, verrou court, best-effort : ne panique jamais (verrou empoisonné →
    /// récupéré). Appelé sur le hot-path des read-paths APRÈS construction de la réponse.
    pub fn record(&self, tenant_id: &str, note_id: &str, kind: &'static str, now_ms: i64) {
        // Verrou empoisonné : récupérer la map malgré tout (télémétrie best-effort).
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let entry = guard
            .entry((tenant_id.to_owned(), note_id.to_owned(), kind.to_owned()))
            .or_insert((0, now_ms));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = entry.1.max(now_ms);
    }

    /// Vide l'accumulateur atomiquement et retourne la map accumulée (batch de flush).
    ///
    /// Best-effort : verrou empoisonné → récupéré, jamais de panic.
    pub fn swap(&self) -> std::collections::HashMap<UsageKey, UsageValue> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *guard)
    }
}

// ── AppState ──────────────────────────────────────────────────────────────────

/// Shared application state, cloneable via `Arc<AppState>`.
///
/// Fields:
/// - `started_at`, `version`, `build_sha`: used by `/health`.
/// - `metrics`: Prometheus metrics on the sidecar port :19091.
/// - `jwt`: Ed25519 JWT service — verifies incoming bearer tokens.
/// - `acl`: compiled ACL engine. Initialised with an empty preset (default deny).
/// - `revocation`: JWT revocation store. In-memory by default (development).
/// - `vault`: vault registry — `PlaceholderRegistry` before injection;
///   `Vault` in production (via `with_vault_path`).
/// - `search`: search index — replaced by a real `SqliteIndex` in production.
#[allow(dead_code)]
#[derive(Clone)]
pub struct AppState {
    /// Server start instant (used to compute uptime in `/health`).
    pub started_at: Instant,
    /// System time at startup (used for the RFC 3339 `started_at` field in `/health`).
    pub started_at_systime: SystemTime,
    /// Binary version (from `CARGO_PKG_VERSION` at compile time).
    pub version: &'static str,
    /// Build commit SHA (from the `BUILD_SHA` env var at compile time, or `"unknown"`).
    pub build_sha: &'static str,
    /// Prometheus metrics — sidecar loopback port :19091.
    pub metrics: AppMetrics,
    /// Ed25519 JWT service for verifying incoming bearer tokens.
    ///
    /// Shared via `Arc` — immutable after boot initialisation. `verify` takes `&self`.
    pub jwt: Arc<JwtService>,
    /// Compiled ACL engine (deny-wins, TOML preset). Initialised with an empty preset = default deny.
    pub acl: Arc<AclEngine>,
    /// JWT revocation store. In-memory by default; `SqliteRevocationStore` in production.
    pub revocation: Arc<dyn RevocationStore>,
    /// Vault registry — `PlaceholderRegistry` before `with_vault_path`;
    /// `Vault` in production.
    pub vault: Arc<dyn Registry>,
    /// Registre de handles multi-vault `Map<VaultId, Vault>` (design cible du routage —
    /// GAP-1, W3). Vide avant `with_vault_path` ; singleton `{main}` en production
    /// (byte-identical LIVE tant que `multi_tenant` est OFF). Consommé par le routage des
    /// reads et le re-check purge — pas encore lu à ce stade.
    pub vaults: Arc<VaultRegistry>,
    /// Handle `Arc<SqliteIndex>` **concret** partagé du vault racine.
    ///
    /// `None` avant `with_vault_path` ; `Some(vault.index())` après (le pool `index.db` du
    /// `Vault` racine). Sert la registration runtime d'un 2e vault via
    /// [`gradatum_vault::Vault::with_shared_index`] (`handle_admin_vault_create`, flag ON) —
    /// `state.search`/`state.vault` étant type-effacés (`dyn Index`/`dyn Registry`), ils
    /// n'exposent pas l'`Arc<SqliteIndex>` requis. Additif, byte-identical OFF (jamais lu à
    /// flag OFF : la registration runtime est gatée `multi_tenant.enabled`).
    pub shared_index: Option<Arc<SqliteIndex>>,
    /// Search index — `dyn Index` facade (`DocumentStore` + `IndexStore` + `VectorStore`).
    ///
    /// Concrete type erased; `Arc<dyn Index>` enables dynamic dispatch.
    /// In production, injected via `with_search_path` (backed by `SqliteIndex`).
    /// In dev/test, an in-memory placeholder.
    pub search: Arc<dyn Index>,
    /// JSONL audit sink. `NoopAuditSink` by default (tests + dev);
    /// `JsonlFileSink` in production via `with_audit_dir`.
    pub audit: Arc<dyn AuditSink>,
    /// API key store. `NoopApiKeyStore` by default;
    /// `SqliteApiKeyStore` in production via `with_api_keys_path`.
    ///
    /// Used by the `/auth/exchange` handler to verify incoming keys.
    pub api_keys: Arc<dyn ApiKeyStore>,
    /// Embedder for async embedding generation.
    ///
    /// `Noop(384)` by default — wired to `HttpEmbedder` in production when `cfg.embed.enabled`.
    /// Used by the worker (via shared `AppState`) for `embed_note`.
    pub embedder: Arc<dyn Embedder>,
    /// Cross-encoder reranker for post-ranking in vault search.
    ///
    /// `NoopReranker` by default (preserves composite order); `JinaOnnxReranker`
    /// enabled in production via the `onnx-reranker` feature + `with_reranker`.
    pub reranker: Arc<dyn gradatum_search::Reranker>,
    /// [`gradatum_core::QueueStore`] for job endpoints.
    ///
    /// `NoopQueueStore` by default — wired via `with_job_store` in production.
    /// In production, injected from `GradatumQueue` (via `gradatum-queue`).
    /// The legacy `queue` field (`Queue` trait, `jobs_v2`) was removed in 2.1.0 (F-177).
    pub job_store: Arc<dyn QueueStore>,
    /// Shared SQLite database handle for the idempotency table (migration 008).
    ///
    /// `None` by default — wired via `with_job_store_pool` in production.
    /// When `None`, job endpoints return 501 Not Implemented for operations
    /// that require the database (Idempotency-Key).
    pub jobs_pool: Option<gradatum_db_sqlite::QueueDb>,
    /// Append-only store for the `event_log` table.
    ///
    /// `None` by default — wired via `with_event_log_path` in production (same `index.db`).
    /// When `None`, `POST /api/v1/event-log` returns 503 Service Unavailable.
    ///
    /// Uses a dedicated connection on `index.db` (WAL, multi-connection safe).
    /// Retention bounded via a Tokio interval task (6 h by default, `[event_log]` config).
    pub event_log: Option<EventLogStore>,

    /// Append-only store for the `session_trace` table (session-log Tier 1).
    ///
    /// `None` by default — wired via `with_session_trace_path` in production (same
    /// `index.db`). When `None`, `POST /api/v1/session-log/trace` returns 503 Service Unavailable.
    ///
    /// Uses a dedicated connection on `index.db` (WAL, multi-connection safe).
    /// Retention bounded to 90 days via a Tokio interval task (`[session_trace]` config).
    pub session_trace: Option<crate::session_trace_store::SessionTraceStore>,

    /// UPSERT latest-per-tenant store for the `proactive_surface` table (F-46, Active Recall).
    ///
    /// `None` by default — wired via `with_proactive_surface_path` in production (same
    /// `index.db`). When `None`, the proactive refresh task is skipped (noop).
    ///
    /// Uses a dedicated connection on `index.db` (WAL, multi-connection safe).
    /// Written by the `proactive_refresh_once` interval task.
    pub proactive_surface: Option<crate::proactive_surface_store::ProactiveSurfaceStore>,

    /// Sessions + feedback store pour le rappel proactif (F-46, Active Recall v0.7.1).
    ///
    /// `None` par défaut — câblé via `with_proactive_recall_path` en production (même
    /// `index.db`). Quand `None`, les handlers de feedback proactif retournent 503.
    ///
    /// Connexion WAL dédiée (safe multi-connexion SQLite).
    /// Written by `insert_session` and `record_feedback`.
    pub proactive_recall: Option<crate::proactive_recall_store::ProactiveRecallStore>,

    /// Trust-decay configuration for search scoring.
    ///
    /// Default: decay enabled (`distilled = 90 days`). Disable via `[scoring]` config
    /// (`trust_decay_enabled = false`). Wired via `with_scoring`.
    pub scoring: Arc<gradatum_search::TrustDecayConfig>,

    /// Usage-salience scoring params.
    ///
    /// `None` = disabled (default, `[salience] enabled = false`) ⇒ scores stay
    /// byte-identical to the salience-free baseline. `Some` = enabled, resolved
    /// from `[salience]` config. Wired via `with_salience`.
    pub salience: Option<Arc<gradatum_search::SalienceParams>>,

    /// Params salience EFFECTIFS pré-résolus PAR vault (L6, overrides A6 `[per_vault.*.salience]`).
    ///
    /// Pré-calculé au boot via [`crate::config::ServerConfig::resolve_salience_per_vault`]
    /// (aucune allocation au read-time). **Défaut : map vide** ⇒ tout vault retombe sur
    /// `salience` (global) ⇒ scores byte-identical. Consulté UNIQUEMENT à l'intérieur du bras
    /// `salience.is_some()` du hot-path (`api_v1::logic`) : à OFF (`salience == None`) la map
    /// n'est jamais lue. `Arc` : clone `AppState` par requête sans copie de la map. Wired via
    /// `with_salience_per_vault`.
    pub salience_per_vault:
        Arc<std::collections::HashMap<String, Option<Arc<gradatum_search::SalienceParams>>>>,

    /// Path to the SQLite WAL file (`<index.db>-wal`).
    ///
    /// `None` before `with_search_path` (in-memory placeholder: no on-disk WAL).
    /// Wired in production to `<index.db>-wal` to expose the real `sqlite_wal_size_bytes`
    /// in `/health` and the dashboard. Unmeasurable WAL → `None` ⇒ honest "n/a"
    /// (never 0, which would falsely claim "healthy").
    pub wal_path: Option<std::path::PathBuf>,

    /// Store de compteurs d'usage des read-paths (télémétrie v0.5.3 #4).
    ///
    /// `None` par défaut — câblé via `with_read_usage_path` en production (même `index.db`).
    /// Flushed toutes les 60s par la tâche flush dans `main.rs`.
    pub read_usage: Option<ReadUsageCounterStore>,

    /// Accumulateurs AtomicU64 pour les 5 read-paths instrumentés.
    ///
    /// `Arc` partagé entre tous les handlers et la tâche flush.
    /// Les handlers incrémentent via `fetch_add(1, Ordering::Relaxed)` — coût ~0, aucun I/O.
    /// wired: AppState::with_jwt() → initialisé à 0 dès le boot.
    pub read_usage_accumulators: Arc<ReadUsageAccumulators>,

    /// Store de compteurs d'usage PAR NOTE (salience per note).
    ///
    /// `None` par défaut — câblé via `with_note_usage_path` en production (même `index.db`,
    /// table `note_usage` migration 0029). Flushed toutes les 60 s par la tâche flush.
    pub note_usage: Option<NoteUsageStore>,

    /// Accumulateur d'usage par note en mémoire (`Mutex<HashMap>`).
    ///
    /// `Arc` partagé entre les handlers read-path et la tâche flush. Les handlers
    /// enregistrent via `record(...)` APRÈS construction de la réponse (best-effort, O(1)).
    /// wired: initialisé vide dès le boot (constructeurs), toujours présent.
    pub note_usage_accumulators: Arc<NoteUsageAccumulators>,

    /// Token de l'API interne server-to-worker (Wave 2, v0.5.3).
    ///
    /// `None` = listener interne désactivé (opt-in via `[internal_api]` config).
    /// wired: `gradatum-server/src/main.rs` + `internal/auth.rs`
    pub internal_api_token: Option<Arc<secrecy::SecretString>>,

    /// Token de l'API admin (F-100 incrément 1.6 — delete/restore/purge opérateur).
    ///
    /// **Distinct** du `internal_api_token` worker : les endpoints admin
    /// (`/internal/v1/admin/*`) exigent CE token via `X-Gradatum-Admin: Bearer <token>`,
    /// jamais le token worker. La séparation empêche le worker (qui ne détient que le
    /// token worker) d'atteindre la surface de mutation admin (invariant fondateur F-100 :
    /// jamais par la main des agents/services, uniquement l'opérateur ou le GC).
    ///
    /// `None` = endpoints admin désactivés (fail-closed). Provisionné hors argv
    /// (fichier 0600 côté CLI, valeur config côté serveur).
    /// wired: `gradatum-server/src/main.rs` + `internal/admin_auth.rs`
    pub admin_api_token: Option<Arc<secrecy::SecretString>>,

    /// Compteurs atomiques par outil MCP — télémétrie feat/usage-telemetry-19091.
    ///
    /// Map fermée pré-peuplée (21 outils). `record(name)` est un no-op sur nom inconnu.
    /// Swappé toutes les 60s par la tâche flush pour UPSERT dans `read_usage_counters`
    /// (clés `mcp:<tool>`) et fan-out dans la famille Prometheus `mcp_tool_calls`.
    pub mcp_tool_counters: Arc<McpToolCounters>,

    /// In-memory cache of the skills index (section `"skills"`).
    ///
    /// `None` avant le premier appel `vault_context` avec `inject_skills=true`.
    /// Construit paresseusement (lazy) par `context::skills::build_skill_index` :
    /// scan SQL section `"skills"` + `embed_batch`. Stocké comme `Arc<SkillIndex>`
    /// pour cloner la référence hors du `RwLock` sans copier les vecteurs d'embedding.
    ///
    /// # Invalidation
    ///
    /// Aucun hook `vault_write` pour l'instant (ECON: rebuild lazy à chaque cache miss).
    /// Le cache persiste pour la durée de vie du serveur tant que le build réussit.
    pub skills_index: Arc<tokio::sync::RwLock<Option<Arc<crate::context::skills::SkillIndex>>>>,

    /// Configuration du serveur (intervalles des tâches, options avancées).
    ///
    /// Utilisé par `GET /api/v1/system/scheduled` pour exposer `interval_secs` via
    /// `task_interval_secs(name, &server_config)` — SSOT T4 (zéro divergence entre
    /// intervalles réels et intervalles rapportés).
    /// Défaut : `ServerConfig::default()` (valeurs de production par défaut).
    /// Wired via `AppState::with_server_config` dans `main.rs`.
    pub server_config: Arc<crate::config::ServerConfig>,

    /// Configuration for the context assembly pipeline.
    ///
    /// Governs `assemble_assembled`: default budget, top_n candidates, max skills,
    /// skills budget fraction, embed timeout.
    /// Defaults: budget=2000, top_n=50, max_skills=3, frac=0.15, embed=800ms.
    /// Wired via `AppState::with_context` in `main.rs`.
    pub context: Arc<crate::config::ContextConfig>,
}

impl AppState {
    /// Creates a new state with the current start instant.
    ///
    /// The ACL is initialised with an empty preset (default deny for everything).
    /// The revocation store is in-memory (development only).
    /// `JwtService` is initialised with an ephemeral key (dev/test — WARN logged at boot).
    pub fn new() -> Self {
        // Clé éphémère : acceptable en dev/test uniquement.
        // En production, utiliser `with_jwt(jwt_service)` après chargement PEM.
        let jwt = JwtService::new_ephemeral();
        Self::with_jwt(jwt)
    }

    /// Creates a state with an explicit `JwtService` (for production).
    ///
    /// Used in `main.rs` after loading the Ed25519 key from config.
    pub fn with_jwt(jwt: JwtService) -> Self {
        // Preset vide = aucun consumer → AclEngine retourne DenyImplicit pour tout.
        // Les handlers exigent Allow — tout token sans consumer configuré sera FORBIDDEN.
        let acl = AclEngine::from_preset_str("")
            .expect("the empty ACL preset is always valid — static invariant");
        Self {
            started_at: Instant::now(),
            started_at_systime: SystemTime::now(),
            version: env!("CARGO_PKG_VERSION"),
            build_sha: option_env!("BUILD_SHA").unwrap_or("unknown"),
            metrics: AppMetrics::new(),
            jwt: Arc::new(jwt),
            acl: Arc::new(acl),
            revocation: Arc::new(InMemoryRevocationStore::new()),
            vault: Arc::new(PlaceholderRegistry),
            vaults: Arc::new(VaultRegistry::new()),
            shared_index: None,
            search: placeholder_search(),
            audit: Arc::new(NoopAuditSink),
            api_keys: Arc::new(NoopApiKeyStore),
            embedder: Arc::new(NoopEmbedder::new(384)),
            reranker: Arc::new(gradatum_search::NoopReranker),
            job_store: Arc::new(NoopQueueStore),
            jobs_pool: None,
            event_log: None,
            session_trace: None,
            proactive_surface: None,
            proactive_recall: None,
            scoring: Arc::new(gradatum_search::TrustDecayConfig::default()),
            salience: None,
            salience_per_vault: Arc::new(std::collections::HashMap::new()),
            wal_path: None,
            read_usage: None,
            read_usage_accumulators: Arc::new(ReadUsageAccumulators::default()),
            note_usage: None,
            note_usage_accumulators: Arc::new(NoteUsageAccumulators::default()),
            internal_api_token: None,
            admin_api_token: None,
            mcp_tool_counters: Arc::new(McpToolCounters::new()),
            skills_index: Arc::new(tokio::sync::RwLock::new(None)),
            server_config: Arc::new(crate::config::ServerConfig::default()),
            context: Arc::new(crate::config::ContextConfig::default()),
        }
    }

    /// Creates a state with explicit `JwtService` and `AclEngine` (for tests).
    ///
    /// Used in integration tests to inject an ACL preset that allows the test consumer.
    ///
    /// In production the ACL is loaded from config — use [`AppState::with_jwt`].
    #[allow(dead_code)] // Utilisé dans v1-parity-tests (crate externe), invisible au dead_code lint binaire.
    pub fn with_jwt_and_acl(jwt: JwtService, acl: AclEngine) -> Self {
        Self {
            started_at: Instant::now(),
            started_at_systime: SystemTime::now(),
            version: env!("CARGO_PKG_VERSION"),
            build_sha: option_env!("BUILD_SHA").unwrap_or("unknown"),
            metrics: AppMetrics::new(),
            jwt: Arc::new(jwt),
            acl: Arc::new(acl),
            revocation: Arc::new(InMemoryRevocationStore::new()),
            vault: Arc::new(PlaceholderRegistry),
            vaults: Arc::new(VaultRegistry::new()),
            shared_index: None,
            search: placeholder_search(),
            audit: Arc::new(NoopAuditSink),
            api_keys: Arc::new(NoopApiKeyStore),
            embedder: Arc::new(NoopEmbedder::new(384)),
            reranker: Arc::new(gradatum_search::NoopReranker),
            job_store: Arc::new(NoopQueueStore),
            jobs_pool: None,
            event_log: None,
            session_trace: None,
            proactive_surface: None,
            proactive_recall: None,
            scoring: Arc::new(gradatum_search::TrustDecayConfig::default()),
            salience: None,
            salience_per_vault: Arc::new(std::collections::HashMap::new()),
            wal_path: None,
            read_usage: None,
            read_usage_accumulators: Arc::new(ReadUsageAccumulators::default()),
            note_usage: None,
            note_usage_accumulators: Arc::new(NoteUsageAccumulators::default()),
            internal_api_token: None,
            admin_api_token: None,
            mcp_tool_counters: Arc::new(McpToolCounters::new()),
            skills_index: Arc::new(tokio::sync::RwLock::new(None)),
            server_config: Arc::new(crate::config::ServerConfig::default()),
            context: Arc::new(crate::config::ContextConfig::default()),
        }
    }

    /// Opens (or creates) a `SqliteIndex` at `path` and injects it into the state (production wiring).
    ///
    /// Returns an error if the SQLite file is inaccessible or migrations fail.
    /// Used in `main.rs` for production wiring — pointed at `vault/.gradatum/index.db`
    /// (the shared index used by the worker).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // In an async Tokio context:
    /// // let state = AppState::new()
    /// //     .with_search_path(std::path::Path::new("/var/lib/gradatum/index.db"))
    /// //     .await
    /// //     .expect("search init failed");
    /// ```
    #[allow(dead_code)]
    pub async fn with_search_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        let index = SqliteIndex::open(path).await?;
        self.search = Arc::new(index) as Arc<dyn Index>;
        // F-37 S1.3 / T12 — chemin WAL conventionnel SQLite : `<index.db>-wal`.
        // Exposé read-only pour `sqlite_wal_size_bytes` (/health + dashboard).
        let mut wal = path.as_os_str().to_os_string();
        wal.push("-wal");
        self.wal_path = Some(std::path::PathBuf::from(wal));
        Ok(self)
    }

    /// Ouvre un `SqliteIndex`, configure le chemin ANN (v0.5.3 ANN-5), puis injecte.
    ///
    /// Variante de `with_search_path` qui configure le moteur ANN AVANT de caster
    /// le `SqliteIndex` en `Arc<dyn Index>` (le downcast devient impossible après).
    ///
    /// Si `ann_backend = SqliteVec`, active le chemin ANN sur l'index.
    /// Si `ann_backend = BruteForce` (défaut), comportement identique à `with_search_path`.
    ///
    /// # Errors
    ///
    /// Retourne `Err` si l'ouverture SQLite ou les migrations échouent.
    pub async fn with_search_path_ann(
        mut self,
        path: &std::path::Path,
        ann_backend: crate::config::AnnBackend,
        ann_ef_search: u32,
    ) -> anyhow::Result<Self> {
        let index = SqliteIndex::open(path).await?;

        // Configure ANN AVANT le cast en dyn — seule fenêtre où le type concret
        // est encore accessible sans downcast.
        if ann_backend == crate::config::AnnBackend::SqliteVec {
            index.set_ann_enabled(true);
            index.set_ann_ef_search(ann_ef_search);
            tracing::info!(
                ef_search = ann_ef_search,
                "ANN sqlite-vec enabled (ann_backend=sqlite_vec)"
            );
        }

        self.search = Arc::new(index) as Arc<dyn Index>;
        // F-37 S1.3 / T12 — chemin WAL.
        let mut wal = path.as_os_str().to_os_string();
        wal.push("-wal");
        self.wal_path = Some(std::path::PathBuf::from(wal));
        Ok(self)
    }

    /// Injects a pre-built `Arc<dyn Registry>` as the vault registry.
    ///
    /// Used in integration tests to inject a `Vault` already created
    /// (typically via `Vault::create` or `Vault::open` inside a `TempDir`).
    /// In production, use `with_vault_path`.
    #[allow(dead_code)]
    pub fn with_vault_arc(mut self, vault: Arc<dyn Registry>) -> Self {
        self.vault = vault;
        self
    }

    /// Injects a pre-built `Arc<dyn AuditSink>` as the audit sink.
    ///
    /// Used in integration tests to inject an audit sink directly.
    /// In production, use `with_audit_dir`.
    #[allow(dead_code)]
    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = audit;
        self
    }

    /// Opens (or creates) a `Vault` at `path` and injects it as the registry.
    ///
    /// If the vault layout does not yet exist (`path/.gradatum/` absent), `Vault::create`
    /// initialises the full layout before opening the SQLite index.
    ///
    /// Returns an error if the index is inaccessible.
    /// Used in `main.rs` for production wiring.
    ///
    /// Le `vault_id` (namespace physique) est fourni explicitement — préparation du
    /// registre de handles `Map<VaultId, Vault>` (GAP-1, W3). Le call-site LIVE reste
    /// mono-vault `"main"` (byte-identical). Il n'est consommé que par `Vault::create`
    /// (layout initial) ; à l'ouverture d'un vault existant, le `vault_id` est dérivé du
    /// `config.toml` sur disque (source de vérité), le paramètre est alors ignoré.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// use gradatum_core::scope::VaultId;
    /// // In an async Tokio context:
    /// // let state = AppState::new()
    /// //     .with_vault_path(std::path::Path::new("/var/lib/gradatum/vault"), VaultId::new("main"))
    /// //     .await
    /// //     .expect("vault init failed");
    /// ```
    pub async fn with_vault_path(
        mut self,
        path: &std::path::Path,
        vault_id: gradatum_core::scope::VaultId,
    ) -> anyhow::Result<Self> {
        // Si le layout vault n'existe pas, on le crée (idempotent) avec le vault_id fourni.
        // `Vault::create` est safe si les répertoires existent déjà.
        // À l'ouverture, le vault_id vient du config.toml on-disk (SSOT) — cf. doc ci-dessus.
        let vault = if path.join(".gradatum").exists() {
            gradatum_vault::Vault::open(path).await?
        } else {
            gradatum_vault::Vault::create(path, vault_id).await?
        };
        // Un seul `Arc<Vault>` sert à la fois la façade `Registry` (chemin actuel) et le
        // registre de handles `{main}` (design cible, W3). LIVE reste mono-vault singleton :
        // le registre porte exactement le vault ouvert (byte-identical, aucun consommateur
        // du registre à ce stade). Clé = identité réelle du handle (aucune divergence).
        let vault = Arc::new(vault);
        self.vaults = Arc::new(VaultRegistry::singleton(Arc::clone(&vault)));
        // Exposer l'`Arc<SqliteIndex>` concret du vault racine — support de la registration
        // runtime d'un 2e vault (Task 3, flag ON). Le pool `index.db` est partagé : le 2e
        // vault sera adossé à CE pool (partition par colonne `vault_id`).
        self.shared_index = Some(Arc::clone(vault.index()));
        self.vault = vault;
        Ok(self)
    }

    /// Instancie un handle [`gradatum_vault::Vault`] **réel** pour `vault_id`, adossé au pool
    /// `index.db` PARTAGÉ du vault racine ([`shared_index`](Self::shared_index)), sous le même
    /// root md (sibling `<root>/<vault_id>/`). **N'enregistre pas** le handle — l'appelant
    /// décide (registration runtime admin `handle_admin_vault_create`, ou bootstrap boot
    /// [`bootstrap_active_vaults`](Self::bootstrap_active_vaults)).
    ///
    /// # Errors
    ///
    /// - `shared_index` absent (couche vault non initialisée — invariant boot rompu) ;
    /// - vault racine `main` absent du registre ;
    /// - échec I/O d'instanciation ([`gradatum_vault::Vault::with_shared_index`]).
    pub(crate) async fn instantiate_vault_handle(
        &self,
        vault_id: gradatum_core::scope::VaultId,
    ) -> anyhow::Result<Arc<gradatum_vault::Vault>> {
        let shared_index = self.shared_index.clone().ok_or_else(|| {
            anyhow::anyhow!("shared_index unavailable (vault layer not initialised)")
        })?;
        // Root SSOT : celui du vault racine vivant — le sibling est créé sous le MÊME root.
        let root = self
            .vaults
            .get(&gradatum_core::scope::VaultId::new("main"))
            .ok_or_else(|| anyhow::anyhow!("root vault `main` not registered"))?
            .root()
            .to_path_buf();
        let vault = gradatum_vault::Vault::with_shared_index(&root, vault_id, shared_index).await?;
        Ok(Arc::new(vault))
    }

    /// Enregistre au boot les handles de tous les vaults ACTIFS — **fail-closed** :
    /// un vault actif dont le handle ne peut être ouvert fait échouer le boot.
    ///
    /// - **Flag OFF** (défaut LIVE) : **no-op** — le registre reste le singleton `{main}`
    ///   câblé par [`with_vault_path`](Self::with_vault_path) (byte-identical). Aucune I/O,
    ///   aucun chemin fail-closed atteignable : la sortie anticipée est l'unique instruction
    ///   exécutée.
    /// - **Flag ON** : itère [`gradatum_core::index::IndexStore::list_active_vaults`] et
    ///   enregistre un handle réel pour chaque vault pas encore présent (`main` déjà singleton
    ///   → sauté), puis lance la réconciliation registre↔disque
    ///   ([`reconcile_vault_dirs`](Self::reconcile_vault_dirs), volet 2, non bloquante).
    ///
    /// ## Fail-closed (volet 1) — et sa conséquence assumée
    ///
    /// Un vault marqué `active` en base mais **non instanciable** (I/O, `config.toml`
    /// incohérent, invariant boot rompu) **abort le boot** : l'erreur est propagée jusqu'à
    /// `main`, qui n'écoute jamais. Le raisonnement précédent (« warn + continue, cohérent
    /// avec le GC ANN ») est **explicitement inversé** : un vault actif silencieusement absent
    /// du registre ne dégrade pas le service, il le rend **faussement sain** — les écritures
    /// de ce tenant partent en `VaultNotFound` (500) alors que le health check est vert, et
    /// aucun opérateur ne regarde les warns de boot d'un service qui démarre.
    ///
    /// **Conséquence, à énoncer clairement : un seul vault défaillant fait tomber le
    /// démarrage entier, y compris pour les tenants sains.** Le rayon de panne passe d'« un
    /// tenant dégradé en silence » à « service indisponible, bruyamment ». C'est le choix
    /// voulu : à flag ON, la panne visible prime sur la
    /// disponibilité partielle non signalée. À flag OFF ce chemin est inatteignable.
    ///
    /// L'asymétrie avec le volet 2 est **voulue** : volet 1 (registre → disque) est dur,
    /// volet 2 (disque → registre) est souple.
    ///
    /// # Errors
    ///
    /// - échec de `list_active_vaults` (lecture du registre au boot) ;
    /// - **fail-closed** : échec d'instanciation du handle d'un vault actif ;
    /// - **fail-closed** : refus d'insertion registre ([`VaultRegistryError`], divergence
    ///   d'identité `vault_id`).
    pub async fn bootstrap_active_vaults(&self) -> anyhow::Result<()> {
        if !self.server_config.multi_tenant.enabled {
            return Ok(());
        }
        let active = self.search.list_active_vaults().await?;
        for vault_id in active {
            // Déjà enregistré (`main` singleton, ou re-boot idempotent) → sauté.
            // C'est le point d'idempotence multi-boot : un 2e démarrage sur le même état
            // ne ré-instancie ni ne duplique rien (`add_vault` est lui-même idempotent).
            if self.vaults.get(&vault_id).is_some() {
                continue;
            }
            let handle = self
                .instantiate_vault_handle(vault_id.clone())
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "boot fail-closed: active vault `{vault_id}` not instantiable — \
                         startup aborted ({e})"
                    )
                })?;
            self.vaults
                .add_vault(vault_id.clone(), handle)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "boot fail-closed: active vault `{vault_id}` refused by the registry — \
                         startup aborted ({e})"
                    )
                })?;
        }
        self.reconcile_vault_dirs().await;
        Ok(())
    }

    /// Réconciliation **disque → registre** au boot (L7 volet 2) — **non bloquante**.
    ///
    /// Recense les répertoires de namespace `<root>/<vault_id>/` présents sur disque qui n'ont
    /// **aucune ligne `tenants`** (quel que soit le statut), et les signale : un `warn` par
    /// orphelin + la jauge [`AppMetrics::vault_orphan_dirs`]
    /// (`gradatum_vault_orphan_dirs` sur :19091). Aucun effet correctif : rien n'est créé,
    /// supprimé ni enregistré — un orphelin est un fait d'exploitation, son traitement est une
    /// décision humaine (données potentiellement précieuses).
    ///
    /// Asymétrie assumée avec le volet 1 : un vault absent du disque alors qu'il est `active`
    /// est une **incohérence servie** (fail-closed) ; un répertoire sans tenant n'est servi par
    /// personne — il ne justifie pas de refuser le démarrage.
    ///
    /// ## Ce qui n'est PAS un orphelin
    ///
    /// - un vault `suspended` / `deleted` : il a une ligne `tenants`, son répertoire est
    ///   légitimement sur disque (sinon chaque suspension produirait un faux positif
    ///   permanent) ;
    /// - les répertoires cachés (`.gradatum/`, `.archive/`) ;
    /// - les noms qui ne peuvent pas être un `vault_id` ([`gradatum_core::scope::VaultId::parse`]).
    ///
    /// ## Erreurs (toutes avalées — volet non bloquant)
    ///
    /// Root introuvable, `read_dir` en échec ou lecture de statut en échec → `warn`, et la
    /// jauge est **laissée inchangée** plutôt que remise à `0` : un scan qui n'a pas pu
    /// conclure ne doit pas se présenter comme un scan à zéro orphelin.
    async fn reconcile_vault_dirs(&self) {
        let root = match self.vaults.get(&gradatum_core::scope::VaultId::new("main")) {
            Some(v) => v.root().to_path_buf(),
            None => {
                tracing::warn!(
                    "boot: disk reconciliation skipped — root vault `main` absent from registry"
                );
                return;
            }
        };

        let mut entries = match tokio::fs::read_dir(&root).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(root = %root.display(), error = %e, "boot: disk reconciliation skipped — read_dir failed");
                return;
            }
        };

        let mut orphans: u64 = 0;
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(root = %root.display(), error = %e, "boot: disk reconciliation interrupted — entry read failed");
                    return;
                }
            };

            match entry.file_type().await {
                Ok(ft) if ft.is_dir() => {}
                // Fichier, lien, ou type illisible → hors périmètre namespace.
                _ => continue,
            }

            let name = entry.file_name();
            // Noms non-UTF8 : impossibles en `vault_id` (charset `[a-z0-9-]`) → ignorés.
            let Some(name) = name.to_str() else { continue };
            // `.gradatum/` (métadonnées + index partagé) et `.archive/` ne sont pas des namespaces.
            if name.starts_with('.') {
                continue;
            }
            // Un répertoire dont le nom ne peut pas être un `vault_id` n'a jamais pu être
            // produit par le provisioning — ce n'est pas un vault orphelin.
            if gradatum_core::scope::VaultId::parse(name).is_err() {
                continue;
            }
            let vault_id = gradatum_core::scope::VaultId::new(name);
            // Déjà résoluble (racine `main` ou vault actif enregistré juste avant) → connu.
            if self.vaults.get(&vault_id).is_some() {
                continue;
            }

            match self.search.get_tenant_status(name).await {
                // Ligne `tenants` présente (`suspended` / `deleted`) → connu, pas orphelin.
                Ok(Some(status)) => {
                    tracing::debug!(vault_id = %name, status = %status.as_db_str(), "boot: inactive vault present on disk — known to the registry");
                }
                // Aucune ligne `tenants` → orphelin disque.
                Ok(None) => {
                    orphans += 1;
                    tracing::warn!(
                        vault_id = %name,
                        path = %entry.path().display(),
                        "boot: orphan vault directory (no `tenants` row) — not served, no automatic action"
                    );
                }
                Err(e) => {
                    tracing::warn!(vault_id = %name, error = %e, "boot: unreadable tenant status — unclassified directory");
                }
            }
        }

        // `i64` : la jauge prometheus_client est signée ; `orphans` est borné par le nombre
        // d'entrées d'un répertoire, la conversion ne peut pas déborder en pratique — on la
        // sature par prudence plutôt que de caster à l'aveugle (`err-no-as-overflow`).
        self.metrics
            .vault_orphan_dirs
            .set(i64::try_from(orphans).unwrap_or(i64::MAX));
        if orphans > 0 {
            tracing::warn!(
                orphans,
                root = %root.display(),
                "boot: disk reconciliation complete — vault directories without tenant"
            );
        }
    }

    /// Réconciliation **clés → preset ACL** au boot (B6′b) — **non bloquante**.
    ///
    /// Recense les clés API **actives** dont l'`owner` n'est déclaré par aucun `[[consumer]]`
    /// du preset chargé, et les signale : un `error!` par clé orpheline + la jauge
    /// [`AppMetrics::api_key_orphan_owners`] (`gradatum_api_key_orphan_owners` sur :19091).
    /// Aucun effet correctif : rien n'est révoqué ni créé — une clé orpheline peut être un
    /// credential provisionné en avance de son entrée ACL, son traitement est une décision
    /// humaine.
    ///
    /// ## Pourquoi ça n'empêche JAMAIS le démarrage
    ///
    /// Une clé orpheline s'authentifie puis se fait refuser partout : elle est déjà inerte.
    /// Refuser de démarrer pour la signaler échangerait un agent muet contre un service
    /// indisponible pour tous — un incident strictement pire que celui qu'on rapporte, et
    /// déclenché par une donnée héritée qu'aucun redéploiement ne corrige. L'asymétrie est
    /// la même que celle du volet 2 de la réconciliation disque
    /// ([`AppState::reconcile_vault_dirs`]) : ce qui n'est servi à personne ne justifie pas
    /// de fermer le service.
    ///
    /// Le niveau `error!` (et non `warn!`) est délibéré : c'est la ligne unique qui aurait
    /// rendu l'incident `engine` visible immédiatement, et elle doit franchir un filtre de
    /// log réglé sur `error`.
    ///
    /// ## Ce qui n'est PAS un orphelin
    ///
    /// - une clé **révoquée** dont l'owner a disparu du preset : elle n'authentifie plus,
    ///   la signaler ferait du bruit permanent sur un passé assumé ;
    /// - une identité déclarée **sans aucune clé** : c'est l'état nominal d'un consumer dont
    ///   le credential n'a pas encore été émis (4 cas sur le parc au moment de l'écriture).
    ///   La relation n'est vérifiée que dans le sens clé → identité.
    ///
    /// ## Erreurs (toutes avalées — volet non bloquant)
    ///
    /// Listing en échec → `warn`, et la jauge est **laissée inchangée** plutôt que remise à
    /// `0` : un scan qui n'a pas pu conclure ne doit pas se présenter comme un scan à zéro
    /// orphelin.
    pub async fn reconcile_key_owners(&self) {
        let keys = match self.api_keys.list(false, None).await {
            Ok(keys) => keys,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "boot: api-key/ACL reconciliation skipped — active key listing failed"
                );
                return;
            }
        };

        let mut orphans: u64 = 0;
        let mut agent_grants_created: u64 = 0;
        for key in &keys {
            if !self.acl.has_identity(&key.owner) {
                orphans += 1;
                tracing::error!(
                    owner = %key.owner,
                    prefix = %key.prefix,
                    tenant = %key.tenant_id,
                    "boot: active API key whose owner is declared by no consumer of the ACL \
                     preset — it will authenticate and then be denied on every locus (no \
                     automatic action)"
                );
                continue;
            }
            // B7 : provisionner le grant agent↔vault pour que le contrôle
            // `agent_grants_authorize` (middleware) trouve au moins une ligne.
            // Accès dérivé des scopes de la clé : write → Write, sinon Read.
            let access = if has_write_scope(&key.scopes) {
                gradatum_core::scope::GrantAccess::Write
            } else {
                gradatum_core::scope::GrantAccess::Read
            };
            let vault_id = gradatum_core::scope::VaultId::new(key.tenant_id.as_str());
            match self
                .search
                .upsert_agent_grant(&key.owner, &vault_id, access, None)
                .await
            {
                Ok(()) => {
                    agent_grants_created += 1;
                }
                Err(e) => {
                    tracing::error!(
                        owner = %key.owner,
                        vault = %vault_id,
                        err = %e,
                        "boot: upsert_agent_grant failed — key may be denied at agent-grants \
                         check (non-fatal)"
                    );
                }
            }
        }

        // `i64` : la jauge prometheus_client est signée ; `orphans` est borné par le nombre
        // de clés actives, la conversion ne peut pas déborder en pratique — on la sature par
        // prudence plutôt que de caster à l'aveugle (`err-no-as-overflow`).
        self.metrics
            .api_key_orphan_owners
            .set(i64::try_from(orphans).unwrap_or(i64::MAX));
        if orphans > 0 {
            tracing::error!(
                orphans,
                active_keys = keys.len(),
                "boot: api-key/ACL reconciliation complete — active keys without declared identity"
            );
        }
        if agent_grants_created > 0 {
            tracing::info!(
                agent_grants_created,
                active_keys = keys.len(),
                "boot: api-key/agent-grants sync — agent_vault_grants rows upserted"
            );
        }
    }

    /// Opens (or creates) a `SqliteApiKeyStore` at `path` and injects it into the state.
    ///
    /// Replaces the `NoopApiKeyStore` with a persistent SQLite store.
    /// Required for the `/auth/exchange` handler to function in production.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // In an async Tokio context:
    /// // let state = AppState::new()
    /// //     .with_api_keys_path(std::path::Path::new("/var/lib/gradatum/db/api_keys.sqlite"))
    /// //     .await
    /// //     .expect("api_keys store init failed");
    /// ```
    pub async fn with_api_keys_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        let store = SqliteApiKeyStore::init(path)
            .await
            .map_err(|e| anyhow::anyhow!("SqliteApiKeyStore init failed: {e}"))?;
        self.api_keys = Arc::new(store);
        Ok(self)
    }

    /// Opens (or creates) a `SqliteRevocationStore` at `path` and injects it into the state.
    ///
    /// Replaces the `InMemoryRevocationStore` (development only) with a persistent implementation.
    /// The `InMemoryRevocationStore activated — DEV ONLY` warning disappears after this call.
    ///
    /// Used in `main.rs` when `config.auth.revocation_store == "sqlite"`.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // In an async Tokio context:
    /// // let state = AppState::new()
    /// //     .with_revocation_path(std::path::Path::new("/var/lib/gradatum/db/revocation.sqlite"))
    /// //     .await
    /// //     .expect("revocation store init failed");
    /// ```
    pub async fn with_revocation_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        use gradatum_auth::revocation::SqliteRevocationStore;
        let store = SqliteRevocationStore::new(path)
            .await
            .map_err(|e| anyhow::anyhow!("SqliteRevocationStore init failed: {e}"))?;
        self.revocation = Arc::new(store);
        Ok(self)
    }

    /// Loads an ACL preset from `path` and injects it into the state (production wiring).
    ///
    /// The file at `path` must be an ACL preset TOML (`bearer.toml`).
    /// If the file is absent, unreadable, or the path is empty → fallback DENY-ALL
    /// (`AclEngine::from_preset_str("")`).
    ///
    /// Used in `main.rs` after reading `cfg.acl.preset_path`.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // let state = AppState::new()
    /// //     .with_acl_preset_path(std::path::Path::new("/var/lib/gradatum/config/bearer.toml"));
    /// ```
    pub fn with_acl_preset_path(mut self, path: &std::path::Path) -> Self {
        if path.as_os_str().is_empty() {
            tracing::warn!("ACL preset absent or unreadable — DENY-ALL fallback (empty path)");
            return self;
        }
        match std::fs::read_to_string(path) {
            Ok(content) => match AclEngine::from_preset_str(&content) {
                Ok(engine) => {
                    tracing::info!(path = %path.display(), "AclEngine loaded from preset");
                    self.acl = Arc::new(engine);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "ACL preset unreadable (parse error) — DENY-ALL fallback"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "ACL preset absent or unreadable — DENY-ALL fallback"
                );
            }
        }
        self
    }

    /// Creates a `JsonlFileSink` on `dir` and injects it as the audit sink (production wiring).
    ///
    /// The directory is created automatically on the first call to `record`.
    /// Replaces the default `NoopAuditSink`.
    ///
    /// Used in `main.rs` for optional production wiring.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // In an async Tokio context:
    /// // let state = AppState::new()
    /// //     .with_audit_dir(std::path::Path::new("/var/lib/gradatum/audit"))
    /// //     .await
    /// //     .expect("audit sink init failed");
    /// ```
    pub async fn with_audit_dir(mut self, dir: &std::path::Path) -> anyhow::Result<Self> {
        use crate::audit_jsonl::JsonlFileSink;
        let sink = JsonlFileSink::new(dir.to_path_buf());
        // Créer le répertoire dès le wiring pour détecter les erreurs de permissions tôt.
        tokio::fs::create_dir_all(dir).await?;
        self.audit = Arc::new(sink);
        Ok(self)
    }

    /// Injects a pre-built embedder (production wiring).
    ///
    /// Replaces the default `Noop(384)` with an `HttpEmbedder` configured via `cfg.embed`.
    /// Used in `main.rs` when `cfg.embed.enabled` is true.
    ///
    /// Builder pattern: `state.with_embedder(Arc::new(HttpEmbedder::new(...)))`
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = embedder;
        self
    }

    /// Replaces the default reranker (`NoopReranker`) with an alternative implementation.
    ///
    /// Used to wire `JinaOnnxReranker` when the `onnx-reranker` feature
    /// is enabled and an ONNX model file is available.
    ///
    /// Builder pattern: `state.with_reranker(Arc::new(JinaOnnxReranker::from_file(p)?))`
    #[allow(dead_code)]
    pub fn with_reranker(mut self, reranker: Arc<dyn gradatum_search::Reranker>) -> Self {
        self.reranker = reranker;
        self
    }

    /// Injects a [`QueueStore`] and its SQLite pool for job endpoints.
    ///
    /// Builder pattern: `state.with_job_store(Arc::new(SqliteQueueStore::new(db.clone())), db)`
    ///
    /// In production, called from `main.rs` after initialising the WAL pool
    /// on `cfg.storage.root/db/queue.sqlite` (same file as the worker).
    /// The pool is required for idempotency operations (table `gradatum_idempotency`).
    pub fn with_job_store(
        mut self,
        store: Arc<dyn QueueStore>,
        db: gradatum_db_sqlite::QueueDb,
    ) -> Self {
        self.job_store = store;
        self.jobs_pool = Some(db);
        self
    }

    /// Opens an `EventLogStore` at `path` and injects it into the state (production wiring).
    ///
    /// `path` must point to the same `index.db` file as `with_search_path`
    /// (migration 0006 adds the `event_log` table there).
    ///
    /// Opens a dedicated connection in WAL mode (safe for SQLite multi-connection).
    /// The store is used by `POST /api/v1/event-log` and the retention task.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite file is inaccessible.
    pub async fn with_event_log_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        use crate::event_log_store::EventLogStore;
        let store = EventLogStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("EventLogStore init failed: {e}"))?;
        self.event_log = Some(store);
        Ok(self)
    }

    /// Injects a pre-built `EventLogStore` (for integration tests).
    ///
    /// Pattern: `state.with_event_log(EventLogStore::open_in_memory().await?)`
    #[cfg(test)]
    pub fn with_event_log(mut self, store: EventLogStore) -> Self {
        self.event_log = Some(store);
        self
    }

    /// Opens a `SessionTraceStore` at `path` and injects it into the state (production wiring).
    ///
    /// `path` must point to the same `index.db` file as `with_search_path`
    /// (migration 0015 adds the `session_trace` table there).
    ///
    /// Opens a dedicated connection in WAL mode (safe for SQLite multi-connection).
    /// The store is used by `POST /api/v1/session-log/trace` and the 90-day retention task.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite file is inaccessible.
    pub async fn with_session_trace_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        use crate::session_trace_store::SessionTraceStore;
        let store = SessionTraceStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("SessionTraceStore init failed: {e}"))?;
        self.session_trace = Some(store);
        Ok(self)
    }

    /// Injecte un `SessionTraceStore` pré-construit (dev/test uniquement).
    ///
    /// Pattern: `state.with_session_trace(SessionTraceStore::open_in_memory().await?)`
    ///
    /// **Ne pas utiliser en production** — utiliser [`AppState::with_session_trace_path`].
    /// Méthode disponible sans gate `#[cfg(test)]` pour permettre son usage depuis
    /// les tests d'intégration externes (`tests/`).
    #[allow(dead_code)]
    pub fn with_session_trace(
        mut self,
        store: crate::session_trace_store::SessionTraceStore,
    ) -> Self {
        self.session_trace = Some(store);
        self
    }

    /// Ouvre un `ProactiveSurfaceStore` à `path` et l'injecte dans l'état (câblage production).
    ///
    /// `path` doit pointer vers le même fichier `index.db` que `with_search_path`
    /// (la migration 0022 ajoute la table `proactive_surface`).
    ///
    /// Ouvre une connexion dédiée en mode WAL (safe multi-connexion SQLite).
    /// Used by the proactive refresh task and the recall handlers.
    ///
    /// # Errors
    ///
    /// Retourne une erreur si le fichier SQLite est inaccessible.
    pub async fn with_proactive_surface_path(
        mut self,
        path: &std::path::Path,
    ) -> anyhow::Result<Self> {
        use crate::proactive_surface_store::ProactiveSurfaceStore;
        let store = ProactiveSurfaceStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("ProactiveSurfaceStore init failed: {e}"))?;
        self.proactive_surface = Some(store);
        Ok(self)
    }

    /// Injecte un `ProactiveSurfaceStore` pré-construit (pour les tests).
    ///
    /// Pattern : `state.with_proactive_surface(ProactiveSurfaceStore::open_in_memory().await?)`
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn with_proactive_surface(
        mut self,
        store: crate::proactive_surface_store::ProactiveSurfaceStore,
    ) -> Self {
        self.proactive_surface = Some(store);
        self
    }

    /// Ouvre un `ProactiveRecallStore` à `path` et l'injecte dans l'état (câblage production).
    ///
    /// `path` doit pointer vers le même fichier `index.db` que `with_search_path`
    /// (la migration 0023 ajoute les tables `proactive_recall_sessions` et
    /// `proactive_recall_feedback`).
    ///
    /// Ouvre une connexion dédiée en mode WAL (safe multi-connexion SQLite).
    /// Used by the proactive recall feedback handlers.
    ///
    /// # Errors
    ///
    /// Retourne une erreur si le fichier SQLite est inaccessible.
    pub async fn with_proactive_recall_path(
        mut self,
        path: &std::path::Path,
    ) -> anyhow::Result<Self> {
        use crate::proactive_recall_store::ProactiveRecallStore;
        let store = ProactiveRecallStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("ProactiveRecallStore init failed: {e}"))?;
        self.proactive_recall = Some(store);
        Ok(self)
    }

    /// Injecte un `ProactiveRecallStore` pré-construit (dev/test uniquement).
    ///
    /// Pattern : `state.with_proactive_recall(ProactiveRecallStore::open_in_memory().await?)`
    ///
    /// **Ne pas utiliser en production** — utiliser [`AppState::with_proactive_recall_path`]
    /// qui ouvre le fichier depuis la config. Méthode disponible sans gate `cfg(test)` pour
    /// permettre son usage depuis les tests d'intégration externes (`tests/`).
    #[allow(dead_code)] // Utilisé dans tests/mcp_native.rs (test d'intégration externe).
    pub fn with_proactive_recall(
        mut self,
        store: crate::proactive_recall_store::ProactiveRecallStore,
    ) -> Self {
        self.proactive_recall = Some(store);
        self
    }

    /// Injects the trust-decay scoring configuration.
    ///
    /// Wired from `[scoring]` in the server config in production.
    #[must_use]
    pub fn with_scoring(mut self, scoring: gradatum_search::TrustDecayConfig) -> Self {
        self.scoring = Arc::new(scoring);
        self
    }

    /// Injects the usage-salience scoring params.
    ///
    /// `None` = disabled (default) ⇒ byte-identical scores. Wired from `[salience]`
    /// (`SalienceConfig::resolve`) in the server config in production.
    #[must_use]
    pub fn with_salience(mut self, salience: Option<gradatum_search::SalienceParams>) -> Self {
        self.salience = salience.map(Arc::new);
        self
    }

    /// Injecte les params salience EFFECTIFS pré-résolus par vault (L6, overrides A6).
    ///
    /// Map vide (défaut) ⇒ tout vault utilise la salience globale (`with_salience`) ⇒ scores
    /// byte-identical. Wired depuis
    /// [`crate::config::ServerConfig::resolve_salience_per_vault`] dans `main.rs`.
    #[must_use]
    pub fn with_salience_per_vault(
        mut self,
        per_vault: std::collections::HashMap<String, Option<Arc<gradatum_search::SalienceParams>>>,
    ) -> Self {
        self.salience_per_vault = Arc::new(per_vault);
        self
    }

    /// Injecte la `ServerConfig` de production dans l'état.
    ///
    /// Permet à l'endpoint `GET /api/v1/system/scheduled` d'accéder aux intervalles
    /// réels des tâches via `task_interval_secs(name, &state.server_config)` — SSOT T4.
    /// Sans appel explicite, le défaut est `ServerConfig::default()` (valeurs de production
    /// par défaut). Wired via `main.rs` après le parsing du config.toml de production.
    #[must_use]
    pub fn with_server_config(mut self, cfg: crate::config::ServerConfig) -> Self {
        self.server_config = Arc::new(cfg);
        self
    }

    /// Injects the context assembly pipeline configuration.
    ///
    /// Wired depuis `[context]` dans la config serveur en production.
    /// En l'absence d'appel, les valeurs par défaut (`ContextConfig::default()`) s'appliquent.
    #[must_use]
    pub fn with_context(mut self, context: crate::config::ContextConfig) -> Self {
        self.context = Arc::new(context);
        self
    }

    /// Opens a `ReadUsageCounterStore` at `path` and injects it into the state (production wiring).
    ///
    /// `path` must point to the same `index.db` file as `with_search_path`
    /// (migration 0019 adds the `read_usage_counters` table there).
    ///
    /// Opens a dedicated connection in WAL mode (safe for SQLite multi-connection).
    /// The store is flushed every 60s by the flush task in `main.rs`.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite file is inaccessible.
    pub async fn with_read_usage_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        let store = ReadUsageCounterStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("ReadUsageCounterStore init failed: {e}"))?;
        self.read_usage = Some(store);
        Ok(self)
    }

    /// Ouvre un `NoteUsageStore` à `path` et l'injecte dans le state.
    ///
    /// `path` doit pointer sur le même `index.db` que `with_search_path`
    /// (migration 0029 y ajoute la table `note_usage`).
    ///
    /// Ouvre une connexion dédiée en mode WAL (sûr pour SQLite multi-connexion).
    /// Le store est flushé toutes les 60 s par la tâche flush dans `main.rs`.
    ///
    /// # Errors
    ///
    /// Retourne une erreur si le fichier SQLite est inaccessible.
    pub async fn with_note_usage_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        let store = NoteUsageStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("NoteUsageStore init failed: {e}"))?;
        self.note_usage = Some(store);
        Ok(self)
    }

    /// Injecte le token pour l'API interne (production wiring).
    ///
    /// Appelé dans `main.rs` après lecture de `cfg.internal_api.token`.
    /// Si `token` est `None` (config par défaut) → le listener interne est désactivé.
    ///
    /// Builder pattern: `state.with_internal_api_token(token)`
    pub fn with_internal_api_token(mut self, token: secrecy::SecretString) -> Self {
        self.internal_api_token = Some(Arc::new(token));
        self
    }

    /// Injecte le token de l'API admin (F-100 incrément 1.6 — delete/restore/purge).
    ///
    /// Appelé dans `main.rs` après lecture de `cfg.internal_api.admin_token`.
    /// Si `token` est `None` → les endpoints admin `/internal/v1/admin/*` restent
    /// désactivés (fail-closed). **Distinct** du token worker.
    ///
    /// Builder pattern: `state.with_admin_api_token(token)`
    pub fn with_admin_api_token(mut self, token: secrecy::SecretString) -> Self {
        self.admin_api_token = Some(Arc::new(token));
        self
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod note_usage_accumulator_tests {
    use super::NoteUsageAccumulators;
    use crate::note_usage_store::{KIND_READ, KIND_SEARCH_HIT, NoteUsageStore};
    use std::sync::Arc;

    /// `record` agrège count `+1` et `last_used_ms = max` ; `swap` retourne puis vide.
    #[test]
    fn record_then_swap_returns_and_resets() {
        let acc = NoteUsageAccumulators::default();
        acc.record("main", "01AAA", KIND_READ, 100);
        acc.record("main", "01AAA", KIND_READ, 200);
        acc.record("main", "01BBB", KIND_SEARCH_HIT, 150);

        let batch = acc.swap();
        assert_eq!(
            batch.get(&("main".into(), "01AAA".into(), KIND_READ.into())),
            Some(&(2, 200)),
            "count cumulé, last_used_ms = max(100, 200)"
        );
        assert_eq!(
            batch.get(&("main".into(), "01BBB".into(), KIND_SEARCH_HIT.into())),
            Some(&(1, 150))
        );

        assert!(
            acc.swap().is_empty(),
            "après un swap, l'accumulateur doit être vide"
        );
    }

    /// `swap` sur un accumulateur vierge retourne une map vide (aucune panique).
    #[test]
    fn swap_empty_returns_empty() {
        let acc = NoteUsageAccumulators::default();
        assert!(acc.swap().is_empty());
    }

    /// 8 threads × 1000 records sur la même clé → count exact = 8000 (aucun incrément perdu).
    #[test]
    fn record_concurrent_8_threads_sums_exact() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 1000;

        let acc = Arc::new(NoteUsageAccumulators::default());
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let acc = Arc::clone(&acc);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        acc.record("main", "01AAA", KIND_READ, i as i64);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread join");
        }

        let batch = acc.swap();
        let (count, last) = batch
            .get(&("main".into(), "01AAA".into(), KIND_READ.into()))
            .copied()
            .expect("clé présente");
        assert_eq!(
            count,
            (THREADS * PER_THREAD) as u64,
            "aucun incrément perdu sous concurrence"
        );
        assert_eq!(
            last,
            (PER_THREAD - 1) as i64,
            "last_used_ms = max des now_ms observés"
        );
    }

    /// Chaîne complète T2 : record → swap → flush_batch → get (intégration boot simulée).
    #[tokio::test]
    async fn record_swap_flush_persists_to_db() {
        let acc = NoteUsageAccumulators::default();
        acc.record("main", "01AAA", KIND_READ, 100);
        acc.record("main", "01AAA", KIND_READ, 100);

        let store = NoteUsageStore::open_in_memory().await.expect("open");
        let n = store.flush_batch(acc.swap()).await.expect("flush");
        assert_eq!(n, 1);
        assert_eq!(
            store
                .get(
                    &gradatum_core::scope::VaultId::new("main"),
                    "01AAA",
                    KIND_READ
                )
                .await
                .expect("get"),
            Some((2, 100))
        );
    }
}

#[cfg(test)]
mod vault_registry_tests {
    use super::{AppState, VaultRegistry};
    use gradatum_core::scope::VaultId;
    use std::sync::Arc;

    /// Ouvre `main` + un 2e vault `vault-b` adossé au MÊME pool index (handle partagé).
    async fn two_real_vaults(
        root: &std::path::Path,
    ) -> (Arc<gradatum_vault::Vault>, Arc<gradatum_vault::Vault>) {
        let vault_main = Arc::new(
            gradatum_vault::Vault::create(root, VaultId::new("main"))
                .await
                .expect("Vault::create main"),
        );
        let vault_b = Arc::new(
            gradatum_vault::Vault::with_shared_index(
                root,
                VaultId::new("vault-b"),
                Arc::clone(vault_main.index()),
            )
            .await
            .expect("Vault::with_shared_index vault-b"),
        );
        (vault_main, vault_b)
    }

    /// `singleton(main)` + `add_vault(vault-b)` → LES DEUX résolubles ; un inconnu reste
    /// fail-closed (`VaultNotFound`). Prouve le constructeur multi-handle (additif — le
    /// chemin `singleton` prod n'est pas modifié).
    #[tokio::test]
    async fn registry_add_second_vault_then_resolve() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (vault_main, vault_b) = two_real_vaults(tmp.path()).await;

        let reg = VaultRegistry::singleton(vault_main);
        reg.add_vault(VaultId::new("vault-b"), vault_b)
            .expect("add_vault vault-b");

        assert!(reg.resolve(&VaultId::new("main")).is_ok());
        assert!(reg.resolve(&VaultId::new("vault-b")).is_ok());
        assert!(
            reg.resolve(&VaultId::new("inconnu")).is_err(),
            "un vault non enregistré doit rester fail-closed (VaultNotFound)"
        );
    }

    /// `add_vault` est **idempotent** (ADN 2) : un 2e appel de MÊME identité est un no-op
    /// (le registre ne grossit pas, le handle vivant n'est pas remplacé).
    #[tokio::test]
    async fn add_vault_is_idempotent_on_same_identity() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (vault_main, _vault_b) = two_real_vaults(tmp.path()).await;

        let reg = VaultRegistry::new();
        reg.add_vault(VaultId::new("main"), Arc::clone(&vault_main))
            .expect("add main #1");
        reg.add_vault(VaultId::new("main"), vault_main)
            .expect("add main #2 (idempotent no-op)");
        assert_eq!(reg.len(), 1, "2e add de même identité = no-op");
    }

    /// `add_vault` reste **fail-closed** sur divergence d'identité (un handle `vault-z`
    /// inséré sous la clé `vault-b` est refusé — pas d'insertion).
    #[tokio::test]
    async fn add_vault_fail_closed_on_mismatch() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let vault_z = Arc::new(
            gradatum_vault::Vault::create(tmp.path(), VaultId::new("vault-z"))
                .await
                .expect("create vault-z"),
        );
        let reg = VaultRegistry::new();
        let err = reg
            .add_vault(VaultId::new("vault-b"), vault_z)
            .expect_err("clé divergente doit être refusée");
        assert!(matches!(
            err,
            super::VaultRegistryError::VaultIdMismatch { .. }
        ));
        assert_eq!(reg.len(), 0, "aucun handle enregistré après refus");
    }

    /// Byte-identical OFF : `with_vault_path` produit EXACTEMENT le registre singleton
    /// `{main}`. Le multi-handle est purement additif — le chemin prod par défaut au boot
    /// reste 1 seul vault `main`.
    #[tokio::test]
    async fn boot_flag_off_single_main_vault() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let state = AppState::new()
            .with_vault_path(tmp.path(), VaultId::new("main"))
            .await
            .expect("with_vault_path");
        assert_eq!(state.vaults.len(), 1, "boot OFF = 1 seul vault");
        assert!(state.vaults.resolve(&VaultId::new("main")).is_ok());
        assert!(
            state.vaults.resolve(&VaultId::new("vault-b")).is_err(),
            "aucun 2e vault à flag OFF"
        );
    }
}
