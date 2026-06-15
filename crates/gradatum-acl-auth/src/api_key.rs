//! API key store — argon2id generation, verification, and SQLite persistence.
//!
//! # argon2id security
//!
//! argon2id cost parameters: m=19456 KiB / t=2 / p=1 (defaults of the `argon2` crate).
//! Verification is constant-time (enforced by the `argon2` crate internally).
//!
//! # Naming
//!
//! Key format: `ak_<64 hex chars>` (256-bit secret, hexadecimal encoding).
//! Display prefix: `"ak_" + secret[..8]` (11 chars total, unique by construction via ULID).
//!
//! # rotate atomicity
//!
//! `rotate()` executes `BEGIN; INSERT new; UPDATE old SET revoked_at=NOW; COMMIT` in a single
//! SQLite transaction — no partial state is ever visible.

use std::str::FromStr;
use std::sync::OnceLock;
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

/// Prefix shared by all API keys (Stripe-style).
pub const KEY_PREFIX: &str = "ak_";

/// Secret length in hexadecimal characters (64 hex chars = 256 bits).
///
/// 64 hex chars = 32 bytes = 256 bits of effective entropy.
/// The display prefix uses the first 8 chars (32 bits), leaving
/// 224 bits of secret unexposed.
pub const SECRET_LEN: usize = 64;

// ── Erreurs ────────────────────────────────────────────────────────────────────

/// Errors returned by API key store operations.
#[derive(Debug, thiserror::Error)]
pub enum ApiKeyError {
    /// No key matching the given prefix or secret was found.
    #[error("API key non trouvée")]
    NotFound,

    /// The key has already been revoked.
    #[error("API key déjà révoquée")]
    AlreadyRevoked,

    /// argon2id hashing failed (should not occur under normal conditions).
    #[error("erreur de hachage argon2id : {0}")]
    ArgonHash(String),

    /// Underlying SQLite error.
    #[error("erreur SQLite : {0}")]
    Sql(#[from] sqlx::Error),

    /// Cryptographic secret generation failed.
    #[error("erreur cryptographique : {0}")]
    Crypto(String),

    /// Unsupported tenant at creation time.
    ///
    /// While the vault is single-physical (`"main"`), creating a key for a
    /// tenant other than `"main"` is rejected: such a key would be refused at
    /// the `/auth/exchange` endpoint anyway. This is a code-level guard
    /// (no SQL constraint) and is reversible once true multi-tenant support is
    /// implemented.
    #[error("tenant non supporté (mono-vault) : '{0}' ≠ 'main'")]
    InvalidTenant(String),
}

// ── Types ──────────────────────────────────────────────────────────────────────

/// Persisted representation of an API key (plaintext secret excluded).
///
/// The `hash` field holds the encoded argon2id hash — never the original secret.
/// The `prefix` field is a non-secret display identifier (11 chars: `ak_` + 8 hex).
#[derive(Debug, Clone)]
pub struct ApiKey {
    /// Unique key identifier (ULID).
    pub id: Ulid,
    /// Non-secret display prefix (`ak_` + first 8 chars of the secret).
    pub prefix: String,
    /// Encoded argon2id hash (PHC string format).
    pub hash: String,
    /// Key owner (CLI `--owner`).
    pub owner: String,
    /// Authorized scopes (e.g. `["admin"]`, extensible).
    pub scopes: Vec<String>,
    /// Target tenant identifier.
    pub tenant_id: String,
    /// Creation timestamp (epoch seconds).
    pub created_at: i64,
    /// Last-used timestamp (epoch seconds, nullable).
    pub last_used_at: Option<i64>,
    /// Revocation timestamp (epoch seconds, nullable). `None` = key is active.
    pub revoked_at: Option<i64>,
    /// Optional description (CLI `--description`).
    pub description: Option<String>,
}

impl ApiKey {
    /// Returns `true` if the key has been revoked.
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

/// Material returned on API key creation or rotation.
///
/// The `secret` field holds the full `ak_<64 hex>` key — **displayed ONCE ONLY**
/// on stdout. Never stored in plaintext in the DB.
#[derive(Debug, Clone)]
pub struct ApiKeyMaterial {
    /// Full plaintext key: `ak_<64 hex chars>` (display ONCE ONLY).
    pub secret: String,
    /// Non-secret display prefix (`ak_` + first 8 chars).
    pub prefix: String,
}

// ── Trait ──────────────────────────────────────────────────────────────────────

/// API key store — lifecycle management (create, verify, list, revoke, rotate).
///
/// ## Stability
///
/// `0.x` — no API stability guarantees.
/// Concrete implementations must implement all methods.
///
/// ## argon2id cost
///
/// `m=19456 KiB / t=2 / p=1` (defaults of the `argon2` crate).
#[async_trait::async_trait]
pub trait ApiKeyStore: Send + Sync {
    /// Creates a new API key for `owner` with the given `scopes` and `tenant_id`.
    ///
    /// Generates a cryptographic secret (256 bits), hashes it via argon2id, and persists
    /// the result to the DB. Returns [`ApiKeyMaterial`] containing the plaintext secret
    /// (to be displayed ONCE ONLY).
    ///
    /// # Errors
    /// - `ApiKeyError::ArgonHash` if argon2id hashing fails (should not occur under normal conditions)
    /// - `ApiKeyError::Sql` if the SQLite insert fails
    async fn create(
        &self,
        owner: &str,
        scopes: Vec<String>,
        tenant_id: String,
        description: Option<String>,
    ) -> Result<ApiKeyMaterial, ApiKeyError>;

    /// Verifies an API key secret and returns the key metadata if valid.
    ///
    /// Returns `ApiKeyError::NotFound` if the key does not exist OR if the secret
    /// does not match (no distinction between the two cases — uniform security).
    /// Returns `ApiKeyError::AlreadyRevoked` if the key exists but has been revoked.
    ///
    /// Updates `last_used_at` on successful verification.
    ///
    /// # Security
    /// argon2id verification is constant-time.
    ///
    /// # Errors
    /// - `ApiKeyError::NotFound` if no key matches or the secret is incorrect
    /// - `ApiKeyError::AlreadyRevoked` if the key is revoked
    /// - `ApiKeyError::Sql` on DB error
    async fn verify(&self, secret: &str) -> Result<ApiKey, ApiKeyError>;

    /// Lists API keys (secrets excluded).
    ///
    /// If `include_revoked` is `false`, returns only active keys.
    async fn list(&self, include_revoked: bool) -> Result<Vec<ApiKey>, ApiKeyError>;

    /// Revokes an API key by its prefix.
    ///
    /// Returns `ApiKeyError::NotFound` if the prefix is unknown.
    /// Returns `ApiKeyError::AlreadyRevoked` if the key is already revoked.
    ///
    /// # Errors
    /// - `ApiKeyError::NotFound` if the prefix does not exist
    /// - `ApiKeyError::AlreadyRevoked` if already revoked
    /// - `ApiKeyError::Sql` on DB error
    async fn revoke(&self, prefix: &str) -> Result<(), ApiKeyError>;

    /// Atomically revokes the old key and creates a new one.
    ///
    /// Executed in a single `BEGIN/COMMIT` SQLite transaction:
    /// - INSERT new hashed secret
    /// - UPDATE old: `SET revoked_at = now()`
    ///
    /// If the COMMIT fails, neither the new key nor the revocation is persisted.
    ///
    /// Returns [`ApiKeyMaterial`] for the new key (to be displayed ONCE ONLY).
    ///
    /// # Errors
    /// - `ApiKeyError::NotFound` if the source prefix is unknown
    /// - `ApiKeyError::AlreadyRevoked` if the source key is already revoked
    /// - `ApiKeyError::Sql` if the transaction fails
    async fn rotate(&self, prefix: &str) -> Result<ApiKeyMaterial, ApiKeyError>;
}

// ── Implémentation SQLite ──────────────────────────────────────────────────────

/// [`ApiKeyStore`] implementation backed by SQLite (sqlx pool).
///
/// The database must be initialized via [`SqliteApiKeyStore::init`] before use,
/// which applies the embedded migrations (including `migrations/V0001__create_api_keys.sql`).
#[derive(Clone)]
pub struct SqliteApiKeyStore {
    pool: SqlitePool,
}

impl SqliteApiKeyStore {
    /// Opens (or creates) a `SqliteApiKeyStore` at `db_path`.
    ///
    /// Runs the embedded sqlx migrations at startup (idempotent).
    /// Logs a WARN if the `api_keys` table already contains rows (non-destructive re-init).
    ///
    /// # Errors
    /// - `ApiKeyError::Sql` if the connection or migration fails
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

    /// Creates a `SqliteApiKeyStore` backed by an in-memory SQLite database (tests only).
    ///
    /// The database is reset on each call — no persistence.
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

    /// Generates a 256-bit API key secret (32 bytes → 64 hex chars).
    ///
    /// The `ak_` prefix plus 64 hex chars yields 67 chars total.
    fn generate_secret() -> String {
        let mut bytes = [0u8; SECRET_LEN / 2]; // 32 octets → 64 chars hex
        OsRng.fill_bytes(&mut bytes);
        format!("{}{}", KEY_PREFIX, hex::encode(&bytes))
    }

    /// Derives the display prefix from the full secret.
    ///
    /// `prefix = "ak_" + secret[3..11]` (8 hex chars after the `ak_` prefix).
    /// Unique by construction: the secret is generated by a CSPRNG (256 bits).
    ///
    /// # UTF-8 safety
    ///
    /// `secret` is untrusted input on the auth path (`verify`). A naive byte slice
    /// `&secret[..11]` would panic if byte index 11 falls inside a multi-byte UTF-8
    /// character (e.g. `ak_€€€€€`) — a reachable client-triggered DoS. We therefore
    /// truncate on the **largest char boundary ≤ 11 bytes**. A well-formed key
    /// (`ak_` + hex) is pure ASCII, so the result is unchanged; a malformed multi-byte
    /// secret simply yields a shorter prefix that fails the DB lookup downstream
    /// (fail-closed: a malformed prefix never authenticates).
    fn derive_prefix(secret: &str) -> &str {
        // Largest char boundary at or below byte 11 — never panics, never splits a char.
        let mut end = 11.min(secret.len());
        while end > 0 && !secret.is_char_boundary(end) {
            end -= 1;
        }
        &secret[..end]
    }

    /// Hashes a secret via argon2id (PHC string format).
    ///
    /// Cost: m=19456 KiB / t=2 / p=1 (defaults of the `argon2` crate).
    fn hash_secret(secret: &str) -> Result<String, ApiKeyError> {
        let salt = SaltString::generate(&mut ArgonOsRng);
        let argon2 = Argon2::default();
        argon2
            .hash_password(secret.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| ApiKeyError::ArgonHash(e.to_string()))
    }

    /// Verifies a secret against a stored argon2id hash (constant-time).
    fn verify_secret(secret: &str, hash: &str) -> Result<bool, ApiKeyError> {
        let parsed_hash =
            PasswordHash::new(hash).map_err(|e| ApiKeyError::ArgonHash(e.to_string()))?;
        Ok(Argon2::default()
            .verify_password(secret.as_bytes(), &parsed_hash)
            .is_ok())
    }

    /// Returns a pre-computed dummy argon2id hash (PHC string), cached process-wide.
    ///
    /// # Timing-oracle mitigation
    ///
    /// `verify` looks a key up by display prefix before the argon2 compare. On a
    /// missing prefix it would otherwise return `NotFound` after only a cheap SELECT,
    /// whereas an existing prefix with a wrong secret pays the (deliberately costly)
    /// argon2 verification — leaking *prefix existence* through response latency.
    /// Running a dummy argon2 compare against this constant hash on the not-found path
    /// equalizes the work performed (the same `verify_password` cost), removing the
    /// observable timing difference. The argon2 compare itself remains constant-time.
    fn dummy_hash() -> &'static str {
        static DUMMY: OnceLock<String> = OnceLock::new();
        DUMMY
            .get_or_init(|| {
                // Hash a fixed throwaway value once. Cost matches a real `verify_secret`
                // (same default argon2 params). The value is irrelevant — never matched.
                Self::hash_secret("ak_timing_oracle_dummy_secret_value")
                    .unwrap_or_else(|_| String::new())
            })
            .as_str()
    }

    /// Returns the current time as epoch seconds.
    fn now_epoch() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Maps a SQLite row to an [`ApiKey`].
    ///
    /// The 10 arguments correspond 1:1 to the 10 columns of the `api_keys` table,
    /// avoiding an extra intermediate struct allocation.
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
        // P0 cross-tenant (Lot 6) : refus à la création de toute clé non-main tant
        // que le vault est mono-physique. Garde code-level (pas de CHECK SQL — SQLite
        // n'autorise pas ADD CONSTRAINT ; cette garde est réversible et suffisante car
        // `create` est l'UNIQUE chemin d'insertion d'api_keys).
        if tenant_id != "main" {
            tracing::warn!(
                owner = owner,
                tenant = %tenant_id,
                "création api_key refusée : tenant ≠ main (invariant mono-vault)"
            );
            return Err(ApiKeyError::InvalidTenant(tenant_id));
        }
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
            None => {
                // Timing-oracle mitigation : exécuter un compare argon2 factice pour
                // égaliser le temps de réponse avec le chemin "préfixe existe mais
                // secret faux", afin de ne pas révéler l'existence d'un préfixe via la
                // latence. Le résultat est volontairement ignoré.
                let _ = Self::verify_secret(secret, Self::dummy_hash());
                return Err(ApiKeyError::NotFound);
            }
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

        let update_result = sqlx::query(
            "UPDATE api_keys SET revoked_at = ? WHERE prefix = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(prefix)
        .execute(&mut *tx)
        .await?;

        // Vérifier que la révocation de l'ancienne clé a bien eu lieu.
        // rows_affected == 0 indique une race condition (révoquée entre le SELECT et l'UPDATE).
        // Dans ce cas on roll-back la transaction pour éviter d'insérer une nouvelle clé
        // dont l'ancienne resterait active dans les caches.
        if update_result.rows_affected() == 0 {
            tx.rollback().await?;
            return Err(ApiKeyError::AlreadyRevoked);
        }

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
    async fn create_non_main_tenant_is_refused() {
        // P0 cross-tenant (Lot 6) : impossible de créer une clé non-main en mono-vault.
        let store = make_store().await;
        let result = store
            .create(
                "evil-owner",
                vec!["admin".to_string()],
                "evil".to_string(),
                None,
            )
            .await;
        assert!(
            matches!(result, Err(ApiKeyError::InvalidTenant(t)) if t == "evil"),
            "création clé tenant='evil' doit être refusée (InvalidTenant)"
        );
    }

    #[tokio::test]
    async fn create_main_tenant_still_works() {
        // Contre-épreuve : zéro breaking pour le tenant "main".
        let store = make_store().await;
        let material = store
            .create(
                "main-owner",
                vec!["admin".to_string()],
                "main".to_string(),
                None,
            )
            .await
            .expect("création clé main doit réussir");
        let key = store.verify(&material.secret).await.expect("verify OK");
        assert_eq!(key.tenant_id, "main");
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

    /// FIX 1 — un secret multi-octets sur le chemin `verify` ne doit jamais paniquer
    /// (`byte index N is not a char boundary`). Le slice de `derive_prefix` doit
    /// tronquer sur une frontière de caractère, et rejeter proprement (fail-closed).
    #[tokio::test]
    async fn verify_multibyte_secret_does_not_panic() {
        let store = make_store().await;

        // Cas court : `ak_` + caractères multi-octets — l'octet 11 tombe au milieu
        // d'un `€` (3 octets). Doit retourner NotFound, pas paniquer.
        let short = "ak_€€€€€";
        let result = store.verify(short).await;
        assert!(
            matches!(result, Err(ApiKeyError::NotFound)),
            "secret multi-octets court → NotFound (sans panic), obtenu : {result:?}"
        );

        // Cas long : préfixe ASCII valide mais corps non-hex multi-octets.
        let long = format!("ak_{}", "€".repeat(40));
        let result = store.verify(&long).await;
        assert!(
            matches!(result, Err(ApiKeyError::NotFound)),
            "secret multi-octets long → NotFound (sans panic), obtenu : {result:?}"
        );

        // Cas frontière : octet 11 pile sur le début d'un caractère multi-octets.
        // `ak_aaaaaaaa` = 11 octets ASCII, puis `€`. Pas de panic.
        let boundary = "ak_aaaaaaaa€€€";
        let result = store.verify(boundary).await;
        assert!(
            matches!(result, Err(ApiKeyError::NotFound)),
            "secret frontière multi-octets → NotFound (sans panic), obtenu : {result:?}"
        );
    }

    /// ASSESSMENT timing-oracle — un préfixe bien formé mais inexistant emprunte le
    /// chemin `None` (dummy argon2 compare) et retourne NotFound sans panic.
    #[tokio::test]
    async fn verify_wellformed_unknown_prefix_returns_not_found() {
        let store = make_store().await;
        // Préfixe ASCII valide (ak_ + 8 hex) mais jamais inséré en base.
        let secret = "ak_deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let result = store.verify(secret).await;
        assert!(
            matches!(result, Err(ApiKeyError::NotFound)),
            "préfixe bien formé inconnu → NotFound (via dummy compare), obtenu : {result:?}"
        );
    }

    /// FIX 1 — `derive_prefix` ne panique jamais et tronque sur frontière de char.
    #[test]
    fn derive_prefix_is_char_safe() {
        // ASCII bien formé : préfixe complet de 11 octets.
        assert_eq!(
            SqliteApiKeyStore::derive_prefix("ak_0123456789abcdef"),
            "ak_01234567"
        );
        // Multi-octets : tronque AVANT le `€` coupé — jamais au milieu d'un char.
        let p = SqliteApiKeyStore::derive_prefix("ak_€€€€€");
        assert!(p.is_char_boundary(p.len()));
        assert!("ak_€€€€€".starts_with(p));
        // Secret plus court que 11 octets : retourne tout.
        assert_eq!(SqliteApiKeyStore::derive_prefix("ak_"), "ak_");
        // Vide : pas de panic.
        assert_eq!(SqliteApiKeyStore::derive_prefix(""), "");
    }
}
