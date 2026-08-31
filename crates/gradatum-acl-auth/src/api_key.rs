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

use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::OsRng as ArgonOsRng;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use gradatum_core::scope::{AgentId, TenantId};
use rand::RngCore;
use rand::rngs::OsRng;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha384};
use tokio::sync::Mutex;
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

/// The closed set of scopes that grant write access.
///
/// - `write` — the nominal write scope;
/// - `admin` — operator keys (a superset of `write`);
/// - `service` — internal service agents.
///
/// Matching is exact string equality: any other value, including near-misses such
/// as `vault_write`, grants no write access. A key whose scopes are disjoint from
/// this set is strictly read-only and is refused on every write path once
/// `multi_tenant.enabled = true`.
///
/// This constant is the single source of truth shared by the server (which enforces
/// it on each request) and by `gradatum-admin api-key create` (which refuses to mint
/// a key that would silently lack the write access its scopes appear to describe).
///
/// Typed as a slice rather than `[&str; 3]` so that the number of write scopes is not
/// frozen into the public signature: adding a fourth one stays a minor release.
pub const WRITE_SCOPES: &[&str] = &["write", "admin", "service"];

/// Returns `true` if `scopes` contains at least one scope from [`WRITE_SCOPES`].
///
/// Comparison is exact string equality — see [`WRITE_SCOPES`] for why near-misses
/// such as `vault_write` return `false`.
///
/// # Examples
///
/// ```
/// use gradatum_acl_auth::has_write_scope;
///
/// assert!(has_write_scope(&["admin".to_owned()]));
/// assert!(!has_write_scope(&["vault_write".to_owned()]));
/// assert!(!has_write_scope(&[]));
/// ```
#[must_use]
pub fn has_write_scope(scopes: &[String]) -> bool {
    scopes.iter().any(|s| WRITE_SCOPES.contains(&s.as_str()))
}

/// The scope that grants the privilege to write (and read) **any agent's soul note**
/// (`identity/*`), not merely one's own.
///
/// Deliberately **distinct** from every member of [`WRITE_SCOPES`] (`write`/`admin`/`service`):
/// a key declared "full vault" through `admin` must **not** silently inherit the power to
/// overwrite another agent's sovereign identity. Soul-write privilege is a separate, explicit
/// grant.
///
/// The disjointness cuts both ways: a key bearing **only** `identity_write` holds no member of
/// [`WRITE_SCOPES`], so it grants no ordinary write access and is refused on every non-soul
/// write path. Soul-write must be **combined** with a write scope to write anything besides a
/// soul note — it never stands alone as a general write credential.
///
/// This constant is the **single source of truth** shared by the server (which enforces it on
/// each identity read/write path via `TrustContext::has_scope`) and by `gradatum-admin
/// api-key create` (which grants it). Two independent string literals would let a typo
/// (`identity_write` vs `identity-write`) become a silently non-functional grant.
///
/// Owning one's **own** soul is an identity property (`caller_sub == agent`), never gated by
/// this scope: an agent without it still reads and writes `identity/<its-own-sub>`.
pub const IDENTITY_WRITE_SCOPE: &str = "identity_write";

// ── Erreurs ────────────────────────────────────────────────────────────────────

/// Errors returned by API key store operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiKeyError {
    /// No key matching the given prefix or secret was found.
    #[error("API key not found")]
    NotFound,

    /// The key has already been revoked.
    #[error("API key already revoked")]
    AlreadyRevoked,

    /// argon2id hashing failed (should not occur under normal conditions).
    #[error("argon2id hashing error: {0}")]
    ArgonHash(String),

    /// Underlying SQLite error.
    #[error("SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),

    /// The blocking thread failed (panic or cancellation) — impossible in practice.
    #[error("api key store blocking thread failed")]
    Blocking,

    /// Migration state error — the `_sqlx_migrations` tracking table is dirty
    /// (`success = false`) or an applied migration's SHA-384 checksum differs from
    /// the embedded file. Refuses startup rather than risking a replay.
    #[error("api key store migration failed: {0}")]
    Migration(String),

    /// Cryptographic secret generation failed.
    #[error("cryptographic error: {0}")]
    Crypto(String),

    /// Unsupported tenant at creation time.
    ///
    /// While the vault is single-physical (`"main"`), creating a key for a
    /// tenant other than `"main"` is rejected: such a key would be refused at
    /// the `/auth/exchange` endpoint anyway. This is a code-level guard
    /// (no SQL constraint) and is reversible once true multi-tenant support is
    /// implemented.
    #[error("unsupported tenant (mono-vault): '{0}' ≠ 'main'")]
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
    /// Key owner — the credential-borne agent identity, typed [`AgentId`].
    ///
    /// Rebuilt without re-validation from the SQLite `owner` column (CLI `--owner`),
    /// whose value is already trusted: `ApiKeyStore::create` is the single write path.
    /// This is the value the middleware copies into `TrustContext::BearerToken.sub`
    /// after argon2id verification — never a client-supplied header.
    pub owner: AgentId,
    /// Authorized scopes (e.g. `["admin"]`, extensible).
    pub scopes: Vec<String>,
    /// Target tenant identifier (**principal**, typed [`TenantId`]) — a principal
    /// dimension distinct from the `VaultId` namespace. Rebuilt without re-validation
    /// from the SQLite `tenant_id` column, whose value is already trusted.
    pub tenant_id: TenantId,
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
/// Covered by the crate's `2.x` SemVer guarantee — see the [crate-level stability
/// section](crate#stability). Concrete implementations must implement all methods.
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
    /// `owner` is an [`AgentId`], never a bare string. Callers coming from untrusted input
    /// (the `--owner` CLI argument) must obtain the value through [`AgentId::parse`];
    /// server-side callers rebuilding an already-trusted value may use [`AgentId::new`].
    ///
    /// # Errors
    /// - `ApiKeyError::ArgonHash` if argon2id hashing fails (should not occur under normal conditions)
    /// - `ApiKeyError::Sql` if the SQLite insert fails
    async fn create(
        &self,
        owner: &AgentId,
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

    /// Lists API keys (secrets excluded), scoped to `tenant_filter`.
    ///
    /// `tenant_filter`: `None` = all tenants (backward-compatible) ;
    /// `Some(t)` = only keys for tenant `t` (multi-tenant isolation, P1 #4).
    /// If `include_revoked` is `false`, returns only active keys.
    async fn list(
        &self,
        include_revoked: bool,
        tenant_filter: Option<&str>,
    ) -> Result<Vec<ApiKey>, ApiKeyError>;

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

    /// Returns `true` if the registry holds at least one **active** (non-revoked) key.
    ///
    /// Answers a single question: *has this installation ever been provisioned?* It
    /// lets a caller tell an uninitialised registry (no key at all — the operator
    /// must run the provisioning step) from a rejected credential (keys exist, this
    /// one is not among them), so the two can be reported differently instead of
    /// collapsing into one opaque failure.
    ///
    /// ## Scope
    ///
    /// Emptiness is measured **globally**, across every tenant: a registry populated
    /// for another tenant is not an empty registry. The result is a floor, never a
    /// coverage claim — `true` means *at least one* active key exists, never that
    /// every declared identity owns one (identities deliberately left without a key
    /// are a supported state).
    ///
    /// ## Default body
    ///
    /// Derived from [`ApiKeyStore::list`], so no external implementor breaks: `list`
    /// with `include_revoked = false` already filters revoked keys out, and a `None`
    /// tenant filter already spans every tenant. The default loads the whole store
    /// into memory (each [`ApiKey`] carries an argon2id hash) — implementations
    /// backed by a queryable store should override it with an existence check.
    ///
    /// # Errors
    /// - Propagates whatever the underlying store fails with — `ApiKeyError::Sql`
    ///   for the SQLite-backed implementation.
    #[must_use]
    async fn has_any_active(&self) -> Result<bool, ApiKeyError> {
        Ok(!self.list(false, None).await?.is_empty())
    }
}

// ── Implémentation SQLite ──────────────────────────────────────────────────────

/// [`ApiKeyStore`] implementation backed by SQLite (rusqlite connection).
///
/// The database must be initialized via [`SqliteApiKeyStore::init`] before use,
/// which applies the embedded migrations (including `migrations/V0001__create_api_keys.sql`).
#[derive(Clone)]
pub struct SqliteApiKeyStore {
    conn: Arc<Mutex<Connection>>,
    /// Allows creating keys for a tenant other than `"main"`.
    ///
    /// `false` by default, which preserves the single-vault invariant. The operator turns
    /// it on explicitly through [`SqliteApiKeyStore::with_non_main_tenants`] (loopback
    /// admin CLI) once the multi-tenant substrate is provisioned. JWT issuance stays
    /// governed downstream by the `tenant_vault_grants` allow-list, at `/auth/exchange`
    /// and in the middleware, which are fail-closed.
    allow_non_main_tenants: bool,
}

impl SqliteApiKeyStore {
    /// Opens (or creates) a `SqliteApiKeyStore` at `db_path`.
    ///
    /// Runs the embedded migrations at startup, honoring the sqlx `_sqlx_migrations`
    /// tracking table (an already-applied migration is never replayed).
    /// Logs a WARN if the `api_keys` table already contains rows (non-destructive re-init).
    ///
    /// # Errors
    /// - `ApiKeyError::Sql` if the connection fails
    /// - `ApiKeyError::Migration` if the tracking table is dirty or a checksum mismatches
    /// - `ApiKeyError::Blocking` if the blocking thread panics
    pub async fn init(db_path: &std::path::Path) -> Result<Self, ApiKeyError> {
        let path = db_path.to_path_buf();
        // Connexion rusqlite dédiée — motif de pont synchrone/asynchrone repris des magasins
        // du serveur (proactive_recall_store / note_usage_store / read_usage_store) et du
        // sous-lot 1 (base de révocation) : ouverture sur fil bloquant, connexion unique
        // sous verrou `tokio::sync::Mutex`, verrou `blocking_lock()` tenu au minimum.
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, ApiKeyError> {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;

            // WAL AVANT toute migration : SQLite interdit PRAGMA journal_mode=WAL dans une
            // transaction (le runner applique chaque migration dans une transaction, comme
            // sqlx::migrate!). On applique donc WAL au niveau de la connexion avant.
            // `busy_timeout` 5 s et `synchronous` au défaut SQLite (FULL) sont conservés,
            // identiques aux réglages sqlx d'origine.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "busy_timeout", 5000i32)?;

            // Migrations embarquées dans le répertoire `migrations/` de ce crate, honorant
            // la table de suivi sqlx `_sqlx_migrations` (P0 F-145) : une migration déjà
            // appliquée n'est JAMAIS rejouée.
            let applied = run_migrations(&conn)?;
            if applied > 0 {
                tracing::info!(applied, "api_keys migrations applied");
            }

            Ok(conn)
        })
        .await
        .map_err(|_| ApiKeyError::Blocking)??;

        // Warn log si rows préexistent (A3 — re-init non-destructive).
        let conn = Arc::new(Mutex::new(conn));
        let count: i64 = {
            let conn = Arc::clone(&conn);
            tokio::task::spawn_blocking(move || -> Result<i64, ApiKeyError> {
                let conn = conn.blocking_lock();
                let n = conn
                    .query_row("SELECT COUNT(*) FROM api_keys", [], |row| row.get(0))
                    .optional()?
                    .unwrap_or(0);
                Ok(n)
            })
            .await
            .map_err(|_| ApiKeyError::Blocking)??
        };

        if count > 0 {
            warn!(
                rows = count,
                "api_keys table exists with {} rows — non-destructive re-init", count
            );
        }

        Ok(Self {
            conn,
            allow_non_main_tenants: false,
        })
    }

    /// Creates a `SqliteApiKeyStore` backed by an in-memory SQLite database (tests only).
    ///
    /// The database is reset on each call — no persistence.
    #[cfg(test)]
    pub async fn in_memory() -> Result<Self, ApiKeyError> {
        let conn = tokio::task::spawn_blocking(|| -> Result<Connection, ApiKeyError> {
            let conn = Connection::open_in_memory()?;
            let applied = run_migrations(&conn)?;
            if applied > 0 {
                tracing::info!(applied, "api_keys migrations applied (in-memory)");
            }
            Ok(conn)
        })
        .await
        .map_err(|_| ApiKeyError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            allow_non_main_tenants: false,
        })
    }

    /// Allows creating keys for a tenant other than `"main"`.
    ///
    /// An **explicit** lift of the single-vault guard, reserved for the operator through
    /// the loopback admin CLI and its dedicated flag. Without this opt-in, `create`
    /// rejects every non-`main` key with [`ApiKeyError::InvalidTenant`]. JWT issuance for
    /// such keys stays governed downstream by the fail-closed `tenant_vault_grants`
    /// allow-list: a key belonging to an unprovisioned tenant never obtains a token.
    #[must_use]
    pub fn with_non_main_tenants(mut self) -> Self {
        self.allow_non_main_tenants = true;
        self
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
        let id_ulid = Ulid::from_string(&id).unwrap_or_else(|_| Ulid::generate());
        ApiKey {
            id: id_ulid,
            prefix,
            hash,
            // La colonne SQLite `owner` est déjà de confiance (insérée par `create`,
            // seul chemin d'écriture) → `new` sans revalidation. Byte-identical.
            owner: AgentId::new(owner),
            scopes,
            // La colonne SQLite `tenant_id` est déjà de confiance (insérée par `create`,
            // seul chemin d'écriture) → `new` sans revalidation. Byte-identical.
            tenant_id: TenantId::new(tenant_id),
            created_at,
            last_used_at,
            revoked_at,
            description,
        }
    }
}

// ── Migrations ────────────────────────────────────────────────────────────────

/// Version of the single embedded migration — the numeric prefix of the filename
/// `20260506000001_create_api_keys.sql` (same parsing as sqlx: everything before the
/// first `_` is the version, measured in sqlx-core 0.8.6/src/migrate/source.rs).
const MIGRATION_VERSION: i64 = 20_260_506_000_001;

/// Description recorded by sqlx for this migration: the part of the name after the first
/// `_`, with the `.sql` extension removed and `_` replaced by spaces → `create api keys`.
const MIGRATION_DESCRIPTION: &str = "create api keys";

/// SQL body of the single migration, embedded at compile time.
///
/// ⚠️ DO NOT MODIFY `migrations/20260506000001_create_api_keys.sql`: sqlx (and this runner)
/// compute the SHA-384 checksum of the migration over its exact contents. A change would
/// invalidate the checksum on databases where it is already applied → startup refusal
/// (VersionMismatch) — an applied migration is immutable.
const MIGRATION_SQL: &str = include_str!("../migrations/20260506000001_create_api_keys.sql");

/// Applies the pending migrations, honoring the `_sqlx_migrations` tracking table
/// kept by sqlx (schema measured in sqlx-sqlite 0.8.6/src/migrate.rs:72-79).
///
/// "Already applied" decision: the `version` column (PK) — a version already present is
/// NEVER replayed. sqlx fidelity (the `Migrate` trait):
///   - a `success = false` row → dirty database → startup refusal (MigrateError::Dirty);
///   - an applied migration whose SHA-384 checksum differs from the embedded file →
///     startup refusal (MigrateError::VersionMismatch).
///
/// Returns the number of migrations applied (0 on an up-to-date database).
fn run_migrations(conn: &Connection) -> Result<usize, ApiKeyError> {
    // Schéma exact de sqlx (sqlx-sqlite 0.8.6/src/migrate.rs) — no-op si déjà présente.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        );",
    )?;

    // Base sale : une migration marquée en échec → refus de démarrage (parité
    // MigrateError::Dirty). Ne peut survenir que d'une écriture manuelle : l'application
    // sqlx est transactionnelle (migration + enregistrement dans la même transaction).
    let dirty: Option<i64> = conn
        .query_row(
            "SELECT version FROM _sqlx_migrations WHERE success = false ORDER BY version LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(version) = dirty {
        return Err(ApiKeyError::Migration(format!(
            "dirty migration base: migration {version} marked as failed (success = false)"
        )));
    }

    // Migrations déjà appliquées (version + checksum), comme sqlx list_applied_migrations.
    let mut applied: Vec<(i64, Vec<u8>)> = Vec::new();
    {
        let mut stmt =
            conn.prepare("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            applied.push(row?);
        }
    }

    let checksum = Sha384::digest(MIGRATION_SQL.as_bytes()).to_vec();

    if let Some((_, stored)) = applied.iter().find(|(v, _)| *v == MIGRATION_VERSION) {
        // Migration déjà appliquée : vérifier que le fichier n'a pas bougé depuis
        // (immuable post-application). Ne JAMAIS rejouer.
        if stored != &checksum {
            return Err(ApiKeyError::Migration(format!(
                "migration {MIGRATION_VERSION} already applied but its content changed \
                 (SHA-384 checksum differs) — refusing startup"
            )));
        }
        return Ok(0);
    }

    // Application dans une transaction unique (migration + enregistrement), comme sqlx :
    // jamais de migration exécutée deux fois. `unchecked_transaction` car la connexion est
    // dédiée (pas de transaction imbriquée possible).
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(MIGRATION_SQL)?;
    tx.execute(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (?1, ?2, TRUE, ?3, -1)",
        params![MIGRATION_VERSION, MIGRATION_DESCRIPTION, checksum],
    )?;
    tx.commit()?;

    tracing::info!(version = MIGRATION_VERSION, "api_keys migration applied");
    Ok(1)
}

/// Detects the UNIQUE violation on `api_keys.prefix`.
///
/// Extended code SQLITE_CONSTRAINT_UNIQUE = 2067 (19 | (8<<8)). `prefix` is the only
/// UNIQUE constraint on the table (`id` is PRIMARY KEY, code 1555): a 2067 at INSERT
/// can only be a prefix collision.
fn is_prefix_collision(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, _) if err.extended_code == 2067
    )
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
        owner: &AgentId,
        scopes: Vec<String>,
        tenant_id: String,
        description: Option<String>,
    ) -> Result<ApiKeyMaterial, ApiKeyError> {
        // Garde cross-tenant : refus à la création de toute clé non-main tant
        // que le vault est mono-physique. Garde code-level (pas de CHECK SQL — SQLite
        // n'autorise pas ADD CONSTRAINT ; cette garde est réversible et suffisante car
        // `create` est l'UNIQUE chemin d'insertion d'api_keys). Levée contrôlée C3a :
        // opt-in explicite `with_non_main_tenants` (opérateur, CLI admin) uniquement.
        if tenant_id != "main" && !self.allow_non_main_tenants {
            tracing::warn!(
                owner = %owner,
                tenant = %tenant_id,
                "api_key creation refused: tenant ≠ main (single-vault invariant)"
            );
            return Err(ApiKeyError::InvalidTenant(tenant_id));
        }
        let scopes_json = serde_json::to_string(&scopes)
            .map_err(|e| ApiKeyError::Crypto(format!("scopes JSON serialization failed: {e}")))?;
        let owner = owner.to_string();
        let conn = Arc::clone(&self.conn);

        let material = tokio::task::spawn_blocking(move || -> Result<ApiKeyMaterial, ApiKeyError> {
            let conn = conn.blocking_lock();
            // INSERT avec gestion de collision de préfixe (retry si UNIQUE constraint fail).
            // En pratique, la probabilité de collision sur 32 bits est ~1/4 milliard — log seul.
            let now = Self::now_epoch();
            let mut first_attempt = true;
            loop {
                let secret = Self::generate_secret();
                let prefix = Self::derive_prefix(&secret).to_string();
                let hash = Self::hash_secret(&secret)?; // argon2 sur fil bloquant
                let id = Ulid::generate().to_string();

                let result = conn.execute(
                    "INSERT INTO api_keys (id, prefix, hash, owner, scopes_json, tenant_id, created_at, description) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![id, prefix, hash, owner, scopes_json, tenant_id, now, description],
                );

                match result {
                    Ok(_) => {
                        debug!(
                            owner = %owner,
                            prefix = %prefix,
                            tenant = %tenant_id,
                            "API key created"
                        );
                        return Ok(ApiKeyMaterial { secret, prefix });
                    }
                    Err(e) if first_attempt && is_prefix_collision(&e) => {
                        // Collision de préfixe (P1-1 spec V2) — quasi-impossible en pratique.
                        // Retry avec un nouveau secret.
                        warn!("API key prefix collision detected — retrying generation");
                        first_attempt = false;
                    }
                    Err(e) => return Err(ApiKeyError::Sql(e)),
                }
            }
        })
        .await
        .map_err(|_| ApiKeyError::Blocking)??;

        Ok(material)
    }

    async fn verify(&self, secret: &str) -> Result<ApiKey, ApiKeyError> {
        // Fast-fail si le secret ne commence pas par le bon préfixe.
        if !secret.starts_with(KEY_PREFIX) || secret.len() < KEY_PREFIX.len() + 1 {
            return Err(ApiKeyError::NotFound);
        }

        // Chercher par préfixe display pour limiter la portée de la vérification CT.
        let prefix = Self::derive_prefix(secret);
        let prefix_owned = prefix.to_owned();
        let conn = Arc::clone(&self.conn);

        let row = tokio::task::spawn_blocking(move || -> Result<Option<(String, String, String, String, String, String, i64, Option<i64>, Option<i64>, Option<String>)>, ApiKeyError> {
            let conn = conn.blocking_lock();
            let row = conn
                .query_row(
                    "SELECT id, prefix, hash, owner, scopes_json, tenant_id, created_at, last_used_at, revoked_at, description \
                     FROM api_keys WHERE prefix = ?1",
                    params![prefix_owned],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                            row.get(9)?,
                        ))
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
        .map_err(|_| ApiKeyError::Blocking)??;

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
        let prefix = prefix.to_owned();
        let conn = Arc::clone(&self.conn);
        let _ = tokio::task::spawn_blocking(move || -> Result<(), ApiKeyError> {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE api_keys SET last_used_at = ?1 WHERE prefix = ?2",
                params![now, prefix],
            )?;
            Ok(())
        })
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

    async fn list(
        &self,
        include_revoked: bool,
        tenant_filter: Option<&str>,
    ) -> Result<Vec<ApiKey>, ApiKeyError> {
        // P1 #4 : filtre tenant_id. `None` = tous les tenants (backward-compat) ;
        // `Some(t)` = isole les clés API au tenant `t`.
        let tenant_filter = tenant_filter.map(str::to_owned);
        let conn = Arc::clone(&self.conn);

        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String, String, String, String, String, i64, Option<i64>, Option<i64>, Option<String>)>, ApiKeyError> {
            let conn = conn.blocking_lock();
            let sql = if include_revoked {
                "SELECT id, prefix, hash, owner, scopes_json, tenant_id, created_at, last_used_at, revoked_at, description \
                 FROM api_keys WHERE (?1 IS NULL OR tenant_id = ?1) ORDER BY created_at DESC"
            } else {
                "SELECT id, prefix, hash, owner, scopes_json, tenant_id, created_at, last_used_at, revoked_at, description \
                 FROM api_keys WHERE revoked_at IS NULL AND (?1 IS NULL OR tenant_id = ?1) ORDER BY created_at DESC"
            };
            let mut stmt = conn.prepare(sql)?;
            let mut out = Vec::new();
            {
                let rows = stmt.query_map(params![tenant_filter], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                    ))
                })?;
                for row in rows {
                    out.push(row?);
                }
            }
            Ok(out)
        })
        .await
        .map_err(|_| ApiKeyError::Blocking)??;

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
        let conn = Arc::clone(&self.conn);

        // Chercher la clé pour vérifier son existence et son état actuel.
        let prefix_owned = prefix.to_owned();
        let revoked_at =
            tokio::task::spawn_blocking(move || -> Result<Option<Option<i64>>, ApiKeyError> {
                let conn = conn.blocking_lock();
                let row = conn
                    .query_row(
                        "SELECT revoked_at FROM api_keys WHERE prefix = ?1",
                        params![prefix_owned],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(row)
            })
            .await
            .map_err(|_| ApiKeyError::Blocking)??;

        match revoked_at {
            None => return Err(ApiKeyError::NotFound),
            Some(Some(_)) => return Err(ApiKeyError::AlreadyRevoked),
            Some(None) => {}
        }

        let now = Self::now_epoch();
        let prefix_owned = prefix.to_owned();
        let conn = Arc::clone(&self.conn);
        let affected = tokio::task::spawn_blocking(move || -> Result<usize, ApiKeyError> {
            let conn = conn.blocking_lock();
            let n = conn.execute(
                "UPDATE api_keys SET revoked_at = ?1 WHERE prefix = ?2 AND revoked_at IS NULL",
                params![now, prefix_owned],
            )?;
            Ok(n)
        })
        .await
        .map_err(|_| ApiKeyError::Blocking)??;

        if affected == 0 {
            // Race condition : révoquée entre le SELECT et l'UPDATE.
            return Err(ApiKeyError::AlreadyRevoked);
        }

        debug!(prefix = prefix, "API key revoked");
        Ok(())
    }

    async fn rotate(&self, prefix: &str) -> Result<ApiKeyMaterial, ApiKeyError> {
        let conn = Arc::clone(&self.conn);

        // Vérifier source : doit exister + non révoquée.
        let prefix_owned = prefix.to_owned();
        let source = tokio::task::spawn_blocking(
            move || -> Result<Option<(String, String, Option<i64>)>, ApiKeyError> {
                let conn = conn.blocking_lock();
                let row = conn
                    .query_row(
                        "SELECT owner, tenant_id, revoked_at FROM api_keys WHERE prefix = ?1",
                        params![prefix_owned],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                Ok(row)
            },
        )
        .await
        .map_err(|_| ApiKeyError::Blocking)??;

        let (owner, tenant_id, revoked_at) = match source {
            None => return Err(ApiKeyError::NotFound),
            Some(r) => r,
        };

        if revoked_at.is_some() {
            return Err(ApiKeyError::AlreadyRevoked);
        }

        // Copier les scopes de l'ancienne clé.
        let prefix_owned = prefix.to_owned();
        let conn = Arc::clone(&self.conn);
        let scopes_json =
            tokio::task::spawn_blocking(move || -> Result<Option<String>, ApiKeyError> {
                let conn = conn.blocking_lock();
                let s = conn
                    .query_row(
                        "SELECT scopes_json FROM api_keys WHERE prefix = ?1",
                        params![prefix_owned],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(s)
            })
            .await
            .map_err(|_| ApiKeyError::Blocking)??;

        let scopes_json = match scopes_json {
            Some(s) => s,
            None => return Err(ApiKeyError::NotFound),
        };

        // Générer le nouveau secret.
        let new_secret = Self::generate_secret();
        let new_prefix = Self::derive_prefix(&new_secret).to_string();
        let new_hash = Self::hash_secret(&new_secret)?;
        let new_id = Ulid::generate().to_string();
        let now = Self::now_epoch();

        // Transaction atomique : INSERT new + UPDATE old (P1-5 spec V2), le tout sur fil
        // bloquant sous le verrou unique — le même motif de pont que les autres opérations.
        let prefix_owned = prefix.to_owned();
        let conn = Arc::clone(&self.conn);
        let material =
            tokio::task::spawn_blocking(move || -> Result<ApiKeyMaterial, ApiKeyError> {
                let mut conn = conn.blocking_lock();
                let tx = conn.transaction()?;

                tx.execute(
                    "INSERT INTO api_keys (id, prefix, hash, owner, scopes_json, tenant_id, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![new_id, new_prefix, new_hash, owner, scopes_json, tenant_id, now],
                )?;

                let update_result = tx.execute(
                    "UPDATE api_keys SET revoked_at = ?1 WHERE prefix = ?2 AND revoked_at IS NULL",
                    params![now, prefix_owned],
                )?;

                // Vérifier que la révocation de l'ancienne clé a bien eu lieu.
                // update_result == 0 indique une race condition (révoquée entre le SELECT
                // et l'UPDATE). Dans ce cas on roll-back la transaction pour éviter
                // d'insérer une nouvelle clé dont l'ancienne resterait active dans les caches.
                if update_result == 0 {
                    tx.rollback()?;
                    return Err(ApiKeyError::AlreadyRevoked);
                }

                tx.commit()?;
                Ok(ApiKeyMaterial {
                    secret: new_secret,
                    prefix: new_prefix,
                })
            })
            .await
            .map_err(|_| ApiKeyError::Blocking)??;

        debug!(
            old_prefix = prefix,
            new_prefix = %material.prefix,
            "API key rotated"
        );
        Ok(material)
    }

    /// Overrides the trait's default body — same answer, without loading the store.
    ///
    /// The default derived from `list` materializes EVERY active key in memory, each
    /// carrying an argon2id hash, only to read whether the set is empty. `EXISTS` stops
    /// at the first matching row and returns a single integer. The default stays for the
    /// contract (external implementors); this override is here for the cost.
    ///
    /// No `tenant_id` filter: the emptiness measured is global, as in the trait.
    async fn has_any_active(&self) -> Result<bool, ApiKeyError> {
        let conn = Arc::clone(&self.conn);
        let exists = tokio::task::spawn_blocking(move || -> Result<i64, ApiKeyError> {
            let conn = conn.blocking_lock();
            let n = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM api_keys WHERE revoked_at IS NULL)",
                [],
                |row| row.get(0),
            )?;
            Ok(n)
        })
        .await
        .map_err(|_| ApiKeyError::Blocking)??;
        Ok(exists != 0)
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
                &AgentId::new("test-owner"),
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

    /// L'opt-in `with_non_main_tenants` lève la garde qui interdit les tenants non-`main` — la clé
    /// est créée avec son tenant propre (l'autorisation d'émission JWT reste en aval).
    #[tokio::test]
    async fn create_non_main_tenant_allowed_with_optin() {
        let store = make_store().await.with_non_main_tenants();
        let material = store
            .create(
                &AgentId::new("research-agent"),
                vec!["read".to_string()],
                "research".to_string(),
                None,
            )
            .await
            .expect("création clé tenant='research' avec opt-in");
        let key = store.verify(&material.secret).await.expect("verify OK");
        assert_eq!(key.tenant_id, "research");
        assert_eq!(key.owner, "research-agent");
    }

    #[tokio::test]
    async fn create_non_main_tenant_is_refused() {
        // Garde cross-tenant : impossible de créer une clé non-main en mono-vault.
        let store = make_store().await;
        let result = store
            .create(
                &AgentId::new("evil-owner"),
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
                &AgentId::new("main-owner"),
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
            .create(&AgentId::new("owner"), vec![], "main".to_string(), None)
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
            .create(
                &AgentId::new("owner"),
                vec!["admin".into()],
                "main".to_string(),
                None,
            )
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
            .create(
                &AgentId::new("owner"),
                vec!["admin".into()],
                "main".to_string(),
                None,
            )
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
            .create(&AgentId::new("owner"), vec![], "main".to_string(), None)
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
            .create(&AgentId::new("owner1"), vec![], "main".to_string(), None)
            .await
            .expect("k1");
        let _k2 = store
            .create(&AgentId::new("owner2"), vec![], "main".to_string(), None)
            .await
            .expect("k2");

        store.revoke(&k1.prefix).await.expect("revoke k1");

        let active = store.list(false, None).await.expect("list active");
        assert_eq!(active.len(), 1, "1 clé active attendue");
        assert_eq!(active[0].owner, "owner2");

        let all = store.list(true, None).await.expect("list all");
        assert_eq!(all.len(), 2, "2 clés total attendues");
    }

    /// `list()` avec `tenant_filter = Some(t)` ne retourne que les clés du tenant `t` (P1 #4).
    #[tokio::test]
    async fn list_filters_by_tenant() {
        let store = make_store().await.with_non_main_tenants();
        // Créer des clés sur deux tenants différents.
        store
            .create(
                &AgentId::new("owner-a"),
                vec![],
                "tenant-a".to_string(),
                None,
            )
            .await
            .expect("clé tenant-a");
        store
            .create(
                &AgentId::new("owner-a2"),
                vec![],
                "tenant-a".to_string(),
                None,
            )
            .await
            .expect("clé 2 tenant-a");
        store
            .create(
                &AgentId::new("owner-b"),
                vec![],
                "tenant-b".to_string(),
                None,
            )
            .await
            .expect("clé tenant-b");

        // Sans filtre → toutes les clés sont visibles (backward-compat).
        let all = store.list(false, None).await.expect("list all");
        assert_eq!(all.len(), 3);

        // Filtré sur tenant-a → 2 clés.
        let ta = store
            .list(false, Some("tenant-a"))
            .await
            .expect("list tenant-a");
        assert_eq!(ta.len(), 2, "2 clés pour tenant-a attendues");
        for k in &ta {
            assert_eq!(k.tenant_id.as_str(), "tenant-a");
        }

        // Filtré sur tenant-b → 1 clé.
        let tb = store
            .list(false, Some("tenant-b"))
            .await
            .expect("list tenant-b");
        assert_eq!(tb.len(), 1);
        assert_eq!(tb[0].tenant_id.as_str(), "tenant-b");

        // Tenant inconnu → 0 clé.
        let empty = store
            .list(false, Some("tenant-inexistant"))
            .await
            .expect("list tenant inconnu");
        assert!(empty.is_empty());
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

    /// R5 — registre vierge : aucune clé n'a jamais été créée.
    ///
    /// C'est l'état « installation non initialisée » que la discrimination 503/401
    /// doit pouvoir nommer ; il ne doit pas se confondre avec « credential invalide ».
    #[tokio::test]
    async fn has_any_active_returns_false_on_empty_store() {
        let store = make_store().await;
        let result = store.has_any_active().await.expect("has_any_active");
        assert!(!result, "magasin vide → false, obtenu : {result}");
    }

    /// R5 — une clé active suffit : la mesure porte sur l'existence d'AU MOINS une
    /// clé, jamais sur la couverture de chaque identité déclarée (le registre réel
    /// porte des identités volontairement sans clé, `admin` par exemple).
    #[tokio::test]
    async fn has_any_active_returns_true_with_one_active_key() {
        let store = make_store().await;
        store
            .create(&AgentId::new("owner1"), vec![], "main".to_string(), None)
            .await
            .expect("create");
        let result = store.has_any_active().await.expect("has_any_active");
        assert!(result, "une clé active → true, obtenu : {result}");
    }

    /// R5 — cas discriminant : un registre dont l'unique clé a été RÉVOQUÉE est
    /// vide au sens de la discrimination. Les deux tests précédents passeraient
    /// avec un `COUNT(*)` naïf ; seul celui-ci prouve que les révoquées sont
    /// filtrées. La clé est révoquée par le vrai chemin `revoke`, jamais par une
    /// ligne pré-révoquée insérée à la main — c'est ce chemin qu'on couvre.
    #[tokio::test]
    async fn has_any_active_returns_false_when_only_key_is_revoked() {
        let store = make_store().await;
        let material = store
            .create(&AgentId::new("owner1"), vec![], "main".to_string(), None)
            .await
            .expect("create");
        store.revoke(&material.prefix).await.expect("revoke");
        let result = store.has_any_active().await.expect("has_any_active");
        assert!(!result, "unique clé révoquée → false, obtenu : {result}");
    }

    /// R5 — la vacuité mesurée est GLOBALE : une clé appartenant à un autre tenant
    /// que celui de l'appelant rend le registre non vide. Un registre peuplé pour
    /// un autre tenant n'est pas un registre vierge.
    #[tokio::test]
    async fn has_any_active_ignores_tenant_scoping() {
        let store = make_store().await.with_non_main_tenants();
        store
            .create(&AgentId::new("owner1"), vec![], "autre".to_string(), None)
            .await
            .expect("create tenant ≠ main");
        let result = store.has_any_active().await.expect("has_any_active");
        assert!(result, "clé d'un autre tenant → true, obtenu : {result}");
    }

    /// Implémenteur de test qui NE surcharge PAS `has_any_active`.
    ///
    /// `SqliteApiKeyStore` surcharge la méthode : les tests ci-dessus exercent donc
    /// la requête `EXISTS`, jamais le corps par défaut du trait. Or c'est ce corps
    /// que tout implémenteur externe hérite — c'est lui la promesse SemVer. Ce
    /// délégué le rend exécutable : il transmet les cinq méthodes requises au
    /// magasin SQLite et laisse `has_any_active` au défaut dérivé de `list`.
    struct DefaultBodyStore(SqliteApiKeyStore);

    #[async_trait::async_trait]
    impl ApiKeyStore for DefaultBodyStore {
        async fn create(
            &self,
            owner: &AgentId,
            scopes: Vec<String>,
            tenant_id: String,
            description: Option<String>,
        ) -> Result<ApiKeyMaterial, ApiKeyError> {
            self.0.create(owner, scopes, tenant_id, description).await
        }
        async fn verify(&self, secret: &str) -> Result<ApiKey, ApiKeyError> {
            self.0.verify(secret).await
        }
        async fn list(
            &self,
            include_revoked: bool,
            tenant_filter: Option<&str>,
        ) -> Result<Vec<ApiKey>, ApiKeyError> {
            self.0.list(include_revoked, tenant_filter).await
        }
        async fn revoke(&self, prefix: &str) -> Result<(), ApiKeyError> {
            self.0.revoke(prefix).await
        }
        async fn rotate(&self, prefix: &str) -> Result<ApiKeyMaterial, ApiKeyError> {
            self.0.rotate(prefix).await
        }
        // has_any_active : volontairement NON surchargée — c'est l'objet du test.
    }

    /// Le corps par défaut du trait rend les mêmes verdicts que la surcharge SQL.
    ///
    /// Un seul test pour les trois états car l'assertion logique est unique : la
    /// PARITÉ entre les deux implémentations. Les états eux-mêmes sont déjà couverts
    /// un par un ci-dessus, contre la surcharge.
    #[tokio::test]
    async fn has_any_active_default_body_matches_sqlite_override() {
        let store = DefaultBodyStore(make_store().await);

        let empty = store.has_any_active().await.expect("vide");
        let material = store
            .create(&AgentId::new("owner1"), vec![], "main".to_string(), None)
            .await
            .expect("create");
        let active = store.has_any_active().await.expect("active");
        store.revoke(&material.prefix).await.expect("revoke");
        let revoked = store.has_any_active().await.expect("révoquée");

        assert_eq!(
            (empty, active, revoked),
            (false, true, false),
            "corps par défaut : (vide, une active, une révoquée) attendu (false, true, false)"
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

    // ── Preuve P0 (F-145 sous-lot 2) : non-rejeu des migrations ─────────────────
    //
    // La base des clés d'API est la SEULE des trois bases sqlx à porter une table de suivi
    // de migration (`sqlx::migrate!` → `_sqlx_migrations`). Le remplaçant rusqlite doit
    // honorer cette table : une migration déjà appliquée (version présente) n'est JAMAIS
    // rejouée. Les tests ci-dessous fabriquent des bases jetables et prouvent le verdict.

    /// P0 — une base portant la table de suivi REMPLIE comme en production (créée par
    /// `sqlx::migrate!`, version 20260506000001 enregistrée avec son checksum SHA-384) ne
    /// fait RIEN rejouer par `SqliteApiKeyStore::init`.
    #[tokio::test]
    async fn init_does_not_replay_migrations_on_production_like_base() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("api_keys.sqlite");

        // Fabriquer la base « comme en production » : schéma exact de sqlx
        // (sqlx-sqlite 0.8.6/src/migrate.rs) + ligne appliquée + table api_keys déjà créée.
        let conn = Connection::open(&path).expect("open fixture base");
        conn.execute_batch(
            "CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            );
            CREATE TABLE api_keys (
                id TEXT PRIMARY KEY, prefix TEXT NOT NULL UNIQUE, hash TEXT NOT NULL,
                owner TEXT NOT NULL, scopes_json TEXT NOT NULL, tenant_id TEXT NOT NULL,
                created_at INTEGER NOT NULL, last_used_at INTEGER, revoked_at INTEGER,
                description TEXT
            );",
        )
        .expect("fixture schema");
        let checksum = Sha384::digest(MIGRATION_SQL.as_bytes()).to_vec();
        conn.execute(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (?1, ?2, TRUE, ?3, -1)",
            params![MIGRATION_VERSION, MIGRATION_DESCRIPTION, checksum],
        )
        .expect("fixture migration row");
        drop(conn);

        // Lancer le remplaçant (init → run_migrations).
        let _store = SqliteApiKeyStore::init(&path)
            .await
            .expect("init sur base production-like");

        // Rien n'a été rejoué : la table de suivi n'a toujours qu'UNE ligne.
        let conn = Connection::open(&path).expect("reopen");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _sqlx_migrations", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(
            n, 1,
            "aucune migration rejouée : la table de suivi est intacte"
        );
    }

    /// Contre-preuve — une base VIERGE reçoit exactement une application, et le second
    /// appel n'applique plus rien.
    #[test]
    fn migration_runner_applies_fresh_migration_then_skips() {
        let conn = Connection::open_in_memory().expect("in-memory");

        let applied = run_migrations(&conn).expect("first run");
        assert_eq!(
            applied, 1,
            "base vierge : la migration est appliquée une fois"
        );

        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'api_keys'",
                [],
                |row| row.get(0),
            )
            .expect("tables");
        assert_eq!(tables, 1, "la table api_keys existe après la migration");

        let tracked: i64 = conn
            .query_row("SELECT COUNT(*) FROM _sqlx_migrations", [], |row| {
                row.get(0)
            })
            .expect("tracked");
        assert_eq!(
            tracked, 1,
            "la migration est enregistrée dans _sqlx_migrations"
        );

        let applied2 = run_migrations(&conn).expect("second run");
        assert_eq!(applied2, 0, "second appel : rien à rejouer");
    }

    /// Fidélité sqlx — une base sale (ligne `success = false`) refuse le démarrage.
    #[test]
    fn migration_runner_refuses_dirty_base() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            );",
        )
        .expect("schema");
        conn.execute(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (?1, ?2, FALSE, x'00', -1)",
            params![MIGRATION_VERSION, MIGRATION_DESCRIPTION],
        )
        .expect("dirty row");

        let err = run_migrations(&conn).expect_err("base sale → refus");
        assert!(
            matches!(err, ApiKeyError::Migration(_)),
            "refus de démarrage attendu, obtenu : {err:?}"
        );
    }

    /// Le checksum embarqué correspond au fichier sur disque (le même que sqlx lit) :
    /// la preuve de non-rejeu repose sur l'identité des octets, pas sur une copie dérivée.
    #[test]
    fn embedded_migration_matches_disk_file() {
        let disk = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/20260506000001_create_api_keys.sql"
        ))
        .expect("lire la migration sur disque");
        let disk_checksum = Sha384::digest(&disk).to_vec();
        let embedded_checksum = Sha384::digest(MIGRATION_SQL.as_bytes()).to_vec();
        assert_eq!(
            disk_checksum, embedded_checksum,
            "le fichier embarqué (include_str!) doit être byte-identique au fichier sur disque"
        );
    }

    /// Fidélité sqlx — une migration appliquée dont le fichier a changé (checksum différent)
    /// refuse le démarrage : une migration appliquée est immuable.
    #[test]
    fn migration_runner_refuses_modified_applied_migration() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            );",
        )
        .expect("schema");
        conn.execute(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (?1, ?2, TRUE, x'00', -1)",
            params![MIGRATION_VERSION, MIGRATION_DESCRIPTION],
        )
        .expect("row");

        let err = run_migrations(&conn).expect_err("checksum différent → refus");
        assert!(
            matches!(err, ApiKeyError::Migration(_)),
            "refus de démarrage attendu, obtenu : {err:?}"
        );
    }
}
