//! Cache `EffectiveNote` avec validation de checksum sur hit (D-perf-2 / B22).
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §6.1 risque #5.
//!
//! ## Design
//!
//! - Clé composite `(NoteId, u64)` : `NoteId` identifie la note, `u64` est le hash
//!   du `OverrideScope` calculé par le caller (évite d'importer le type complet en clé).
//! - Valeur stockée : `Entry { value: Arc<EffectiveNote>, content_hash: ContentHash }`.
//! - Sur **cache hit** : le caller fournit un closure async `validator` retournant le hash
//!   courant depuis SQLite. Match → retour cache. Mismatch → invalidation + `None`.
//! - Sur **cache miss** : `validator` n'est pas appelé (zero overhead sur miss).
//!
//! ## Defaults
//!
//! | Paramètre      | Valeur |
//! |---|---|
//! | `max_capacity` | 10 000 |
//! | `time_to_live` | 5 min  |
//! | `time_to_idle` | 60 s   |
//!
//! ## Coût
//!
//! +200µs p99 par read (appel SQLite validator sur hit) — acceptable vs risque de stale
//! concurrent (worker write + server read). Référence spec §6.1 risque #5.

use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;

use gradatum_core::identity::{ContentHash, NoteId};
use gradatum_core::note::EffectiveNote;

/// Clé composite du cache : `(note_id, scope_hash)`.
///
/// `scope_hash` est un `u64` calculé par le caller depuis un `OverrideScope`
/// (ex. via `std::hash::Hasher`). Ce design évite d'importer `OverrideScope`
/// dans la clé de cache et maintient la taille du cache petite.
pub type CacheKey = (NoteId, u64);

/// Configuration du `EffectiveNoteCache`.
#[derive(Debug, Clone)]
pub struct EffectiveNoteCacheConfig {
    /// Nombre maximum d'entrées dans le cache (éviction LRU approximative).
    pub max_capacity: u64,
    /// Durée de vie maximale d'une entrée depuis son insertion.
    pub time_to_live: Duration,
    /// Durée d'inactivité maximale avant expiration.
    pub time_to_idle: Duration,
}

impl Default for EffectiveNoteCacheConfig {
    fn default() -> Self {
        Self {
            max_capacity: 10_000,
            time_to_live: Duration::from_secs(300),
            time_to_idle: Duration::from_secs(60),
        }
    }
}

/// Entrée interne du cache : valeur + hash de checksum associé.
#[derive(Clone)]
struct Entry {
    value: Arc<EffectiveNote>,
    content_hash: ContentHash,
}

/// Cache `EffectiveNote` moka LRU avec validation de checksum sur hit.
///
/// Thread-safe et `Clone` (moka `future::Cache` est un wrapper `Arc` interne).
/// Peut être partagé librement entre handlers Axum via `Arc<EffectiveNoteCache>`
/// ou clone direct (les deux pointent sur le même state interne).
pub struct EffectiveNoteCache {
    inner: Cache<CacheKey, Entry>,
}

impl EffectiveNoteCache {
    /// Construit un nouveau cache avec la configuration fournie.
    pub fn new(cfg: EffectiveNoteCacheConfig) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(cfg.max_capacity)
                .time_to_live(cfg.time_to_live)
                .time_to_idle(cfg.time_to_idle)
                .build(),
        }
    }

    /// Insère ou remplace une entrée dans le cache.
    ///
    /// L'appel est async car moka notifie les eviction listeners de façon async.
    pub async fn insert(
        &self,
        key: CacheKey,
        value: Arc<EffectiveNote>,
        content_hash: ContentHash,
    ) {
        self.inner
            .insert(
                key,
                Entry {
                    value,
                    content_hash,
                },
            )
            .await;
    }

    /// Retourne la valeur cachée si le hash est toujours valide, sinon `None`.
    ///
    /// ## Comportement
    ///
    /// 1. **Cache miss** : retourne `Ok(None)` sans appeler `validator`.
    /// 2. **Cache hit** : appelle `validator(note_id)` pour obtenir le hash courant depuis SQLite.
    ///    - Hash identique -> retourne `Ok(Some(arc_value))`.
    ///    - Hash différent -> invalide l'entrée + retourne `Ok(None)`.
    ///    - `validator` retourne une erreur -> propage `Err(e)` sans invalider le cache
    ///      (la donnée n'est pas confirmée stale, c'est une erreur DB transitoire).
    ///
    /// ## Paramètres génériques
    ///
    /// - `F` : closure qui prend un `NoteId` et retourne un `Future<Output = Result<ContentHash, E>>`.
    /// - `E` : type d'erreur propagé tel quel (ex. `sqlx::Error`, `rusqlite::Error`).
    ///
    /// ## Exemple
    ///
    /// ```ignore
    /// let result = cache.get(key, |note_id| async move {
    ///     db_store.fetch_content_hash(note_id).await
    /// }).await?;
    /// ```
    pub async fn get<F, Fut, E>(
        &self,
        key: CacheKey,
        validator: F,
    ) -> Result<Option<Arc<EffectiveNote>>, E>
    where
        F: FnOnce(NoteId) -> Fut,
        Fut: std::future::Future<Output = Result<ContentHash, E>>,
    {
        let Some(entry) = self.inner.get(&key).await else {
            // Cache miss - validator non appelé (zero overhead).
            return Ok(None);
        };

        let live_hash = validator(key.0).await?;

        if live_hash == entry.content_hash {
            return Ok(Some(entry.value));
        }

        // Hash différent : entrée stale - invalider immédiatement.
        self.inner.invalidate(&key).await;
        Ok(None)
    }

    /// Invalide explicitement une entrée du cache.
    ///
    /// À appeler après une écriture sur la note pour prévenir les lectures stale.
    pub async fn invalidate(&self, key: &CacheKey) {
        self.inner.invalidate(key).await;
    }

    /// Nombre d'entrées actuellement dans le cache (approximatif, best-effort moka).
    ///
    /// Utile pour les métriques et les tests. Ne pas utiliser pour la logique métier.
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }

    /// Force l'exécution des tâches d'éviction en attente (TTL/TTI/LRU).
    ///
    /// Utile dans les tests pour observer l'éviction après `tokio::time::sleep`.
    /// En production, moka exécute ces tâches automatiquement en background.
    pub async fn run_pending_tasks(&self) {
        self.inner.run_pending_tasks().await;
    }
}
