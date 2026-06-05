//! API key store — génération argon2id + vérification + SQLite persistence.
//!
//! # Sécurité argon2id
//!
//! Cost fixé alpha.5 : m=19456 KiB / t=2 / p=1 (défaut crate `argon2`).
//! Configurabilité reportée Phase 2.1.
//! La vérification est constant-time (argon2 crate interne).
//!
//! # Naming
//!
//! Clé format `ak_<32 chars hex>` (256 bits de secret, encodage hexadécimal).
//! Préfixe display : `"ak_" + secret[..8]` (11 chars total, unique par construction via ULID).
//!
//! # Atomicité rotate
//!
//! `rotate()` exécute `BEGIN; INSERT new; UPDATE old SET revoked_at=NOW; COMMIT` en une
//! transaction SQLite — aucun état partiel visible.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::rand_core::OsRng as ArgonOsRng;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use rand::rngs::OsRng;
use rand::RngCore;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use tracing::{debug, warn};
use ulid::Ulid;

/// Préfixe de toutes les API keys (Stripe-like, D2).
pub const KEY_PREFIX: &str = "ak_";

/// Longueur du secret en caractères hexadécimaux (32 hex chars = 128 bits).
///
/// Note : 32 hex chars = 16 octets = 128 bits d'entropie effective.
/// Le préfixe display utilise les 8 premiers chars (32 bits), ce qui laisse
/// 96 bits de secret non exposé.
pub const SECRET_LEN: usize = 32;

// ── Erreurs ────────────────────────────────────────────────────────────────────

/// Erreurs possibles lors des opérations sur le store d'API keys.
#[derive(Debug, thiserror::Error)]
pub enum ApiKeyError {
    /// Aucune clé correspondant au préfixe ou au secret n'a été trouvée.
    #[error("API key non trouvée")]
    NotFound,

    /// La clé a déjà été révoquée (P1-1 spec V2 : erreur explicite).
    #[error("API key déjà révoquée")]
    AlreadyRevoked,

    /// Erreur lors du hachage argon2id (ne devrait pas arriver en pratique).
    #[error("erreur de hachage argon2id : {0}")]
    ArgonHash(String),

    /// Erreur SQLite sous-jacente.
    #[error("erreur SQLite : {0}")]
    Sql(#[from] sqlx::Error),

    /// Erreur de génération de secret cryptographique.
    #[error("erreur cryptographique : {0}")]
    Crypto(String),
}

// ── Types ──────────────────────────────────────────────────────────────────────

/// Représentation persistée d'une API key (sans le secret en clair).
///
/// Le champ `hash` contient le hash argon2id encodé — jamais le secret original.
/// Le champ `prefix` est un identifiant display non-secret (11 chars `ak_` + 8 hex).
#[derive(Debug, Clone)]
pub struct ApiKey {
    /// Identifiant unique de la clé (ULID).
    pub id: Ulid,
    /// Préfixe display non-secret (`ak_` + 8 premiers chars du secret).
    pub prefix: String,
    /// Hash argon2id encodé (PHC string format).
    pub hash: String,
    /// Propriétaire de la clé (owner CLI `--owner`).
    pub owner: String,
    /// Scopes autorisés (`["admin"]` alpha.5, granulaire Phase 2.1).
    pub scopes: Vec<String>,
    /// Tenant cible (D3-complet, D10 multi-tenancy).
    pub tenant_id: String,
    /// Timestamp de création (epoch secondes).
    pub created_at: i64,
    /// Timestamp du dernier usage (epoch secondes, nullable).
    pub last_used_at: Option<i64>,
    /// Timestamp de révocation (epoch secondes, nullable). `None` = clé active.
    pub revoked_at: Option<i64>,
    /// Description optionnelle (CLI `--description`).
    pub description: Option<String>,
}

impl ApiKey {
    /// Retourne `true` si la clé a été révoquée.
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

/// Matériel retourné lors de la création ou rotation d'une API key.
///
/// Le champ `secret` contient la clé complète `ak_<32 hex>` — **affiché UNE SEULE FOIS**
/// sur stdout (D8 spec V2). Non stocké en clair en DB.
#[derive(Debug, Clone)]
pub struct ApiKeyMaterial {
    /// Clé complète en clair : `ak_<32 hex chars>` (à afficher UNE SEULE FOIS).
    pub secret: String,
    /// Préfixe display non-secret (`ak_` + 8 premiers chars).
    pub prefix: String,
}

// ── Trait ──────────────────────────────────────────────────────────────────────

/// Store d'API keys — gestion du cycle de vie (create, verify, list, revoke, rotate).
///
/// ## Stability
///
/// `0.x` — aucune garantie de stabilité API (Phase 2.0, tagged `unstable`).
/// Les implémentations concrètes doivent implémenter toutes les méthodes.
///
/// ## argon2id cost (P1-2)
///
/// `m=19456 KiB / t=2 / p=1` (défaut crate `argon2` alpha.5).
/// Configurabilité reportée Phase 2.1.
#[async_trait::async_trait]
pub trait ApiKeyStore: Send + Sync {
    /// Crée une nouvelle API key pour `owner` avec les `scopes` et le `tenant_id` donnés.
    ///
    /// Génère un secret cryptographique (256 bits), hache via argon2id, persiste en DB.
    /// Retourne [`ApiKeyMaterial`] contenant le secret en clair (à afficher UNE SEULE FOIS).
    ///
    /// # Erreurs
    /// - `ApiKeyError::ArgonHash` si le hachage argon2id échoue (ne devrait pas arriver)
    /// - `ApiKeyError::Sql` si l'insert SQLite échoue
    async fn create(
        &self,
        owner: &str,
        scopes: Vec<String>,
        tenant_id: String,
        description: Option<String>,
    ) -> Result<ApiKeyMaterial, ApiKeyError>;

    /// Vérifie un secret API key et retourne les métadonnées si valide.
    ///
    /// Retourne `ApiKeyError::NotFound` si la clé n'existe pas OU si le secret
    /// ne correspond pas (pas de distinction énumération — sécurité uniforme).
    /// Retourne `ApiKeyError::AlreadyRevoked` si la clé existe mais est révoquée.
    ///
    /// Met à jour `last_used_at` si la vérification réussit.
    ///
    /// # Sécurité
    /// La vérification argon2id est constant-time.
    ///
    /// # Erreurs
    /// - `ApiKeyError::NotFound` si pas de clé ou secret incorrect
    /// - `ApiKeyError::AlreadyRevoked` si révoquée
    /// - `ApiKeyError::Sql` si erreur DB
    async fn verify(&self, secret: &str) -> Result<ApiKey, ApiKeyError>;

    /// Liste les API keys (sans les secrets).
    ///
    /// Si `include_revoked = false`, retourne uniquement les clés actives.
    async fn list(&self, include_revoked: bool) -> Result<Vec<ApiKey>, ApiKeyError>;

    /// Révoque une API key par son préfixe.
    ///
    /// Retourne `ApiKeyError::NotFound` si le préfixe est inconnu (P1-1 spec V2).
    /// Retourne `ApiKeyError::AlreadyRevoked` si déjà révoquée.
    ///
    /// # Erreurs
    /// - `ApiKeyError::NotFound` si le préfixe n'existe pas
    /// - `ApiKeyError::AlreadyRevoked` si déjà révoquée
    /// - `ApiKeyError::Sql` si erreur DB
    async fn revoke(&self, prefix: &str) -> Result<(), ApiKeyError>;

    /// Révoque l'ancienne clé et en crée une nouvelle atomiquement (P1-5 spec V2).
    ///
    /// Exécuté dans une transaction `BEGIN/COMMIT SQLite` :
    /// - INSERT nouveau secret haché
    /// - UPDATE ancien: `SET revoked_at = now()`
    ///
    /// En cas d'échec du COMMIT, ni la nouvelle clé ni la révocation ne sont persistées.
    ///
    /// Retourne [`ApiKeyMaterial`] de la nouvelle clé (à afficher UNE SEULE FOIS).
    ///
    /// # Erreurs
    /// - `ApiKeyError::NotFound` si le préfixe source est inconnu
    /// - `ApiKeyError::AlreadyRevoked` si la clé source est déjà révoquée
    /// - `ApiKeyError::Sql` si la transaction échoue
    async fn rotate(&self, prefix: &str) -> Result<ApiKeyMaterial, ApiKeyError>;
}

// ── Implémentation SQLite ──────────────────────────────────────────────────────

/// Implémentation [`ApiKeyStore`] via SQLite (sqlx pool).
///
/// La base de données doit être initialisée avec la migration
/// `migrations/V0001__create_api_keys.sql` avant utilisation
/// (via [`SqliteApiKeyStore::init`]).
#[derive(Clone)]
pub struct SqliteApiKeyStore {
    pool: SqlitePool,
}

impl SqliteApiKeyStore {
    /// Ouvre (ou crée) un `SqliteApiKeyStore` sur `db_path`.
    ///
    /// Exécute les migrations sqlx embarquées au démarrage (idempotent A3).
    /// Si la table `api_keys` préexiste avec des rows, log un WARN (non-destructif).
    ///
    /// # Erreurs
    /// - `ApiKeyError::Sql` si la connexion ou la migration échoue
    pub async fn init(db_path: &std::path::Path) -> Result<Self, ApiKeyError> {
        // Configurer WAL AVANT la migration : sqlx::migrate! exécute chaque migration dans
        // une transaction implicite. SQLite refuse PRAGMA journal_mode=WAL en transaction
        // ("cannot change into wal mode from within a transaction"). On applique donc WAL
        // via SqliteConnectOptions, ce qui le configure au niveau de la connexion avant
        // toute migration.
        let connect_options =
            SqliteConnectOptions::from_str(&format!("sqlite://{}?mode=rwc", db_path.display()))
                .map_err(|e| ApiKeyError::Sql(sqlx::Error::Configuration(Box::new(e))))?
                .journal_mode(SqliteJournalMode::Wal)
                .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .connect_with(connect_options)
            .await?;

        // Migrations embarquées dans le répertoire `migrations/` de ce crate.
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| {
                ApiKeyError::Sql(sqlx::Error::Protocol(format!(
                    "migration api_keys échouée : {e}"
                )))
            })?;

        // Warn log si rows préexistent (A3 — re-init non-destructive).
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);

        if count > 0 {
            warn!(
                rows = count,
                "api_keys table existe avec {} rows — re-init non-destructive", count
            );
        }

        Ok(Self { pool })
    }

    /// Crée un `SqliteApiKeyStore` sur une base SQLite en mémoire (tests uniquement).
    ///
    /// La base est réinitialisée à chaque appel — pas de persistance.
    #[cfg(test)]
    pub async fn in_memory() -> Result<Self, ApiKeyError> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| {
                ApiKeyError::Sql(sqlx::Error::Protocol(format!(
                    "migration api_keys (in-memory) échouée : {e}"
                )))
            })?;
        Ok(Self { pool })
    }

    /// Génère un secret API key valide (256 bits = 32 octets = 64 hex chars ?).
    ///
    /// Note : `SECRET_LEN = 32` chars hex = 16 octets = 128 bits d'entropie.
    /// Le préfixe `ak_` + 32 hex = 35 chars total.
    fn generate_secret() -> String {
        let mut bytes = [0u8; SECRET_LEN / 2]; // 16 octets → 32 chars hex
        OsRng.fill_bytes(&mut bytes);
        format!("{}{}", KEY_PREFIX, hex::encode(&bytes))
    }

    /// Dérive le préfixe display depuis le secret complet.
    ///
    /// `prefix = "ak_" + secret[3..11]` (8 chars hex après le préfixe `ak_`).
    /// Unique par construction : le secret est généré via CSPRNG (128 bits).
    fn derive_prefix(secret: &str) -> &str {
        // secret = "ak_" (3) + 32 hex → prefix = "ak_" + 8 chars = 11 chars total
        &secret[..11.min(secret.len())]
    }

    /// Hache un secret via argon2id (PHC string format).
    ///
    /// Cost : m=19456 KiB / t=2 / p=1 (défaut `argon2` crate alpha.5).
    /// Configurabilité reportée Phase 2.1.
    fn hash_secret(secret: &str) -> Result<String, ApiKeyError> {
        let salt = SaltString::generate(&mut ArgonOsRng);
        let argon2 = Argon2::default();
        argon2
            .hash_password(secret.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| ApiKeyError::ArgonHash(e.to_string()))
    }

    /// Vérifie un secret contre un hash argon2id stocké (constant-time).
    fn verify_secret(secret: &str, hash: &str) -> Result<bool, ApiKeyError> {
        let parsed_hash =
            PasswordHash::new(hash).map_err(|e| ApiKeyError::ArgonHash(e.to_string()))?;
        Ok(Argon2::default()
            .verify_password(secret.as_bytes(), &parsed_hash)
            .is_ok())
    }

    /// Epoch secondes courant.
    fn now_epoch() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Mappe une row SQLite en [`ApiKey`].
    ///
    /// 10 arguments justifiés : mapping 1:1 des 10 colonnes SQL de la table `api_keys`.
    /// Eviter l'allocation d'une struct intermédiaire supplémentaire.
    #[allow(clippy::too_many_arguments)]
    fn row_to_api_key(
        id: String,
        prefix: String,
        hash: String,
        owner: String,
        scopes_json: String,
        tenant_id: String,
        created_at: i64,
        last_used_at: Option<i64>,
        revoked_at: Option<i64>,
        description: Option<String>,
    ) -> ApiKey {
        let scopes: Vec<String> = serde_json::from_str(&scopes_json).unwrap_or_default();
        let id_ulid = Ulid::from_string(&id).unwrap_or_else(|_| Ulid::new());
        ApiKey {
            id: id_ulid,
            prefix,
            hash,
            owner,
            scopes,
            tenant_id,
            created_at,
            last_used_at,
            revoked_at,
            description,
        }
    }
}

// ── hex encode helper ─────────────────────────────────────────────────────────

// Micro-helper pour encoder en hex sans dépendance additionnelle.
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

// ── Trait impl ────────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl ApiKeyStore for SqliteApiKeyStore {
    async fn create(
        &self,
        owner: &str,
        scopes: Vec<String>,
        tenant_id: String,
        description: Option<String>,
    ) -> Result<ApiKeyMaterial, ApiKeyError> {
        let secret = Self::generate_secret();
        let prefix = Self::derive_prefix(&secret).to_string();
        let hash = Self::hash_secret(&secret)?;
        let id = Ulid::new().to_string();
        let scopes_json = serde_json::to_string(&scopes)
            .map_err(|e| ApiKeyError::Crypto(format!("sérialisation scopes JSON échouée : {e}")))?;
        let now = Self::now_epoch();

        // INSERT avec gestion de collision de préfixe (retry si UNIQUE constraint fail).
        // En pratique, la probabilité de collision sur 32 bits est ~1/4 milliard — log seul.
        let result = sqlx::query(
            "INSERT INTO api_keys (id, prefix, hash, owner, scopes_json, tenant_id, created_at, description) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&prefix)
        .bind(&hash)
        .bind(owner)
        .bind(&scopes_json)
        .bind(&tenant_id)
        .bind(now)
        .bind(&description)
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => {
                debug!(
                    owner = owner,
                    prefix = %prefix,
                    tenant = %tenant_id,
                    "API key créée"
                );
                Ok(ApiKeyMaterial { secret, prefix })
            }
            Err(sqlx::Error::Database(db_err))
                if db_err
                    .message()
                    .contains("UNIQUE constraint failed: api_keys.prefix") =>
            {
                // Collision de préfixe (P1-1 spec V2) — quasi-impossible en pratique.
                // Retry avec un nouveau secret.
                warn!("collision préfixe API key détectée — retry génération");
                let secret2 = Self::generate_secret();
                let prefix2 = Self::derive_prefix(&secret2).to_string();
                let hash2 = Self::hash_secret(&secret2)?;
                let id2 = Ulid::new().to_string();
                sqlx::query(
                    "INSERT INTO api_keys (id, prefix, hash, owner, scopes_json, tenant_id, created_at, description) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id2)
                .bind(&prefix2)
                .bind(&hash2)
                .bind(owner)
                .bind(&scopes_json)
                .bind(&tenant_id)
                .bind(now)
                .bind(&description)
                .execute(&self.pool)
                .await?;
                Ok(ApiKeyMaterial {
                    secret: secret2,
                    prefix: prefix2,
                })
            }
            Err(e) => Err(ApiKeyError::Sql(e)),
        }
    }

    async fn verify(&self, secret: &str) -> Result<ApiKey, ApiKeyError> {
        // Fast-fail si le secret ne commence pas par le bon préfixe.
        if !secret.starts_with(KEY_PREFIX) || secret.len() < KEY_PREFIX.len() + 1 {
            return Err(ApiKeyError::NotFound);
        }

        // Chercher par préfixe display pour limiter la portée de la vérification CT.
        let prefix = Self::derive_prefix(secret);

        let row = sqlx::query_as::<_, (String, String, String, String, String, String, i64, Option<i64>, Option<i64>, Option<String>)>(
            "SELECT id, prefix, hash, owner, scopes_json, tenant_id, created_at, last_used_at, revoked_at, description \
             FROM api_keys WHERE prefix = ?",
        )
        .bind(prefix)
        .fetch_optional(&self.pool)
        .await?;

        let row = match row {
            Some(r) => r,
            None => return Err(ApiKeyError::NotFound),
        };

        let (
            id,
            pfx,
            hash,
            owner,
            scopes_json,
            tenant_id,
            created_at,
            _last_used_at,
            revoked_at,
            description,
        ) = row;

        // Vérification argon2id avant le check révocation — constant-time pour éviter
        // l'énumération (un attaquant ne saurait pas si la clé existe ou est révoquée).
        let valid = Self::verify_secret(secret, &hash)?;
        if !valid {
            return Err(ApiKeyError::NotFound);
        }

        // Après vérification CT : retourner AlreadyRevoked si révoquée.
        if revoked_at.is_some() {
            return Err(ApiKeyError::AlreadyRevoked);
        }

        // Mise à jour last_used_at (best-effort — pas bloquant si update échoue).
        let now = Self::now_epoch();
        let _ = sqlx::query("UPDATE api_keys SET last_used_at = ? WHERE prefix = ?")
            .bind(now)
            .bind(prefix)
            .execute(&self.pool)
            .await;

        Ok(Self::row_to_api_key(
            id,
            pfx,
            hash,
            owner,
            scopes_json,
            tenant_id,
            created_at,
            Some(now),
            revoked_at,
            description,
        ))
    }

    async fn list(&self, include_revoked: bool) -> Result<Vec<ApiKey>, ApiKeyError> {
        let rows = if include_revoked {
            sqlx::query_as::<_, (String, String, String, String, String, String, i64, Option<i64>, Option<i64>, Option<String>)>(
                "SELECT id, prefix, hash, owner, scopes_json, tenant_id, created_at, last_used_at, revoked_at, description \
                 FROM api_keys ORDER BY created_at DESC",
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, (String, String, String, String, String, String, i64, Option<i64>, Option<i64>, Option<String>)>(
                "SELECT id, prefix, hash, owner, scopes_json, tenant_id, created_at, last_used_at, revoked_at, description \
                 FROM api_keys WHERE revoked_at IS NULL ORDER BY created_at DESC",
            )
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    prefix,
                    hash,
                    owner,
                    scopes_json,
                    tenant_id,
                    created_at,
                    last_used_at,
                    revoked_at,
                    description,
                )| {
                    Self::row_to_api_key(
                        id,
                        prefix,
                        hash,
                        owner,
                        scopes_json,
                        tenant_id,
                        created_at,
                        last_used_at,
                        revoked_at,
                        description,
                    )
                },
            )
            .collect())
    }

    async fn revoke(&self, prefix: &str) -> Result<(), ApiKeyError> {
        // Chercher la clé pour vérifier son existence et son état actuel.
        let row =
            sqlx::query_as::<_, (Option<i64>,)>("SELECT revoked_at FROM api_keys WHERE prefix = ?")
                .bind(prefix)
                .fetch_optional(&self.pool)
                .await?;

        match row {
            None => return Err(ApiKeyError::NotFound),
            Some((Some(_),)) => return Err(ApiKeyError::AlreadyRevoked),
            Some((None,)) => {}
        }

        let now = Self::now_epoch();
        let affected = sqlx::query(
            "UPDATE api_keys SET revoked_at = ? WHERE prefix = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(prefix)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if affected == 0 {
            // Race condition : révoquée entre le SELECT et l'UPDATE.
            return Err(ApiKeyError::AlreadyRevoked);
        }

        debug!(prefix = prefix, "API key révoquée");
        Ok(())
    }

    async fn rotate(&self, prefix: &str) -> Result<ApiKeyMaterial, ApiKeyError> {
        // Vérifier source : doit exister + non révoquée.
        let row = sqlx::query_as::<_, (String, String, Option<i64>)>(
            "SELECT owner, tenant_id, revoked_at FROM api_keys WHERE prefix = ?",
        )
        .bind(prefix)
        .fetch_optional(&self.pool)
        .await?;

        let (owner, tenant_id, revoked_at) = match row {
            None => return Err(ApiKeyError::NotFound),
            Some(r) => r,
        };

        if revoked_at.is_some() {
            return Err(ApiKeyError::AlreadyRevoked);
        }

        // Copier les scopes de l'ancienne clé.
        let scopes_json: String =
            sqlx::query_scalar("SELECT scopes_json FROM api_keys WHERE prefix = ?")
                .bind(prefix)
                .fetch_one(&self.pool)
                .await?;

        // Générer le nouveau secret.
        let new_secret = Self::generate_secret();
        let new_prefix = Self::derive_prefix(&new_secret).to_string();
        let new_hash = Self::hash_secret(&new_secret)?;
        let new_id = Ulid::new().to_string();
        let now = Self::now_epoch();

        // Transaction atomique : INSERT new + UPDATE old (P1-5 spec V2).
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO api_keys (id, prefix, hash, owner, scopes_json, tenant_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&new_id)
        .bind(&new_prefix)
        .bind(&new_hash)
        .bind(&owner)
        .bind(&scopes_json)
        .bind(&tenant_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query("UPDATE api_keys SET revoked_at = ? WHERE prefix = ? AND revoked_at IS NULL")
            .bind(now)
            .bind(prefix)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        debug!(
            old_prefix = prefix,
            new_prefix = %new_prefix,
            "API key rotée"
        );
        Ok(ApiKeyMaterial {
            secret: new_secret,
            prefix: new_prefix,
        })
    }
}

// ── Tests unitaires ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Crée un store in-memory pour les tests.
    async fn make_store() -> SqliteApiKeyStore {
        SqliteApiKeyStore::in_memory()
            .await
            .expect("init store in-memory")
    }

    #[tokio::test]
    async fn create_and_verify_roundtrip_ok() {
        let store = make_store().await;
        let material = store
            .create(
                "test-owner",
                vec!["admin".to_string()],
                "main".to_string(),
                Some("test key".to_string()),
            )
            .await
            .expect("create OK");

        assert!(
            material.secret.starts_with(KEY_PREFIX),
            "secret doit commencer par ak_"
        );
        assert_eq!(material.prefix.len(), 11, "préfixe = ak_ + 8 chars");

        let key = store.verify(&material.secret).await.expect("verify OK");
        assert_eq!(key.owner, "test-owner");
        assert_eq!(key.scopes, vec!["admin".to_string()]);
        assert_eq!(key.tenant_id, "main");
        assert!(!key.is_revoked());
    }

    #[tokio::test]
    async fn verify_wrong_secret_fails() {
        let store = make_store().await;
        let material = store
            .create("owner", vec![], "main".to_string(), None)
            .await
            .expect("create");

        // Modifier un char dans le secret → verify doit retourner NotFound.
        let wrong = {
            let mut s = material.secret.clone();
            // Remplacer le dernier char par un char différent.
            let last = s.pop().unwrap_or('a');
            let replacement = if last == 'a' { 'b' } else { 'a' };
            s.push(replacement);
            s
        };

        assert!(
            matches!(store.verify(&wrong).await, Err(ApiKeyError::NotFound)),
            "secret incorrect → NotFound"
        );
    }

    #[tokio::test]
    async fn create_then_revoke_then_verify_fails() {
        let store = make_store().await;
        let material = store
            .create("owner", vec!["admin".into()], "main".to_string(), None)
            .await
            .expect("create");

        store.revoke(&material.prefix).await.expect("revoke OK");

        // Après révocation, verify retourne AlreadyRevoked.
        let result = store.verify(&material.secret).await;
        assert!(
            matches!(result, Err(ApiKeyError::AlreadyRevoked)),
            "clé révoquée → AlreadyRevoked, obtenu : {result:?}"
        );
    }

    #[tokio::test]
    async fn revoke_not_found_prefix_returns_not_found() {
        let store = make_store().await;
        let result = store.revoke("ak_deadbeef").await;
        assert!(
            matches!(result, Err(ApiKeyError::NotFound)),
            "préfixe inexistant → NotFound"
        );
    }

    #[tokio::test]
    async fn rotate_produces_new_key_and_revokes_old() {
        let store = make_store().await;
        let original = store
            .create("owner", vec!["admin".into()], "main".to_string(), None)
            .await
            .expect("create original");

        let rotated = store.rotate(&original.prefix).await.expect("rotate OK");

        // La nouvelle clé est différente de l'ancienne.
        assert_ne!(
            original.secret, rotated.secret,
            "rotate doit générer un nouveau secret"
        );
        assert_ne!(original.prefix, rotated.prefix, "nouveaux prefix");

        // L'ancienne clé est révoquée.
        let result = store.verify(&original.secret).await;
        assert!(
            matches!(result, Err(ApiKeyError::AlreadyRevoked)),
            "ancienne clé révoquée après rotate"
        );

        // La nouvelle clé fonctionne.
        let new_key = store
            .verify(&rotated.secret)
            .await
            .expect("nouvelle clé valide");
        assert_eq!(new_key.owner, "owner");
        assert!(!new_key.is_revoked());
    }

    #[tokio::test]
    async fn rotate_already_revoked_returns_error() {
        let store = make_store().await;
        let material = store
            .create("owner", vec![], "main".to_string(), None)
            .await
            .expect("create");

        store.revoke(&material.prefix).await.expect("revoke");

        let result = store.rotate(&material.prefix).await;
        assert!(
            matches!(result, Err(ApiKeyError::AlreadyRevoked)),
            "rotate clé révoquée → AlreadyRevoked"
        );
    }

    #[tokio::test]
    async fn rotate_not_found_returns_error() {
        let store = make_store().await;
        let result = store.rotate("ak_00000000").await;
        assert!(
            matches!(result, Err(ApiKeyError::NotFound)),
            "rotate préfixe inexistant → NotFound"
        );
    }

    #[tokio::test]
    async fn list_excludes_revoked_by_default() {
        let store = make_store().await;
        let k1 = store
            .create("owner1", vec![], "main".to_string(), None)
            .await
            .expect("k1");
        let _k2 = store
            .create("owner2", vec![], "main".to_string(), None)
            .await
            .expect("k2");

        store.revoke(&k1.prefix).await.expect("revoke k1");

        let active = store.list(false).await.expect("list active");
        assert_eq!(active.len(), 1, "1 clé active attendue");
        assert_eq!(active[0].owner, "owner2");

        let all = store.list(true).await.expect("list all");
        assert_eq!(all.len(), 2, "2 clés total attendues");
    }

    #[tokio::test]
    async fn verify_empty_secret_returns_not_found() {
        let store = make_store().await;
        let result = store.verify("").await;
        assert!(
            matches!(result, Err(ApiKeyError::NotFound)),
            "secret vide → NotFound"
        );
    }

    #[tokio::test]
    async fn verify_no_prefix_returns_not_found() {
        let store = make_store().await;
        // Secret valide en longueur mais sans le bon préfixe.
        let result = store.verify("xx_a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6").await;
        assert!(
            matches!(result, Err(ApiKeyError::NotFound)),
            "préfixe invalide → NotFound"
        );
    }
}
