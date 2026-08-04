//! Scope identifiers for distributed overrides.
//!
//! ## Design
//!
//! Three scope axes cover all use cases:
//! - `Vault`  : override applicable to all bearers of a vault.
//! - `Locus`  : override restricted to a sub-vault ACL scope.
//! - `Bearer` : override personalised for a specific bearer (requires `gradatum-acl-auth`).
//!
//! Newtype wrappers (`VaultId`, `LocusId`, `BearerId`) are `#[serde(transparent)]` →
//! compatible with `String` YAML/JSON values from predecessor v1.x without migration.
//!
//! ## Design decision
//!
//! OpenDAL-friendly layout requires `VaultId` and `LocusId` to be newtypes rather than
//! type aliases to guarantee type safety at inter-layer boundaries.

use serde::{Deserialize, Serialize};

/// Maximum length of a `VaultId` in bytes (DoS protection cap).
pub const VAULT_ID_MAX_LEN: usize = 64;

/// Vault identifier (UI alias: `vault`).
///
/// Multi-tenancy mandatory — equivalent to the `tenant_id` SQLite column.
/// `#[serde(transparent)]` → serialised/deserialised as a bare `String`.
///
/// Replaces a plain `pub type VaultId = String` while preserving YAML
/// round-trip compatibility (transparent serialisation).
///
/// ## Construction
///
/// - [`VaultId::new`] and the [`From`] impls are **not validated**: they accept any string.
///   They exist for internal use where the value is already trusted (migrations, tests,
///   SQLite row reconstruction). **Do not use at external input boundaries.**
/// - [`VaultId::parse`] is the validated constructor: use it whenever the `vault_id`
///   comes from untrusted input (HTTP request, CLI argument, YAML deserialization).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VaultId(String);

impl VaultId {
    /// Constructs a `VaultId` from a string **without validation**.
    ///
    /// Intended for internal use where the value is already trusted (migrations,
    /// tests, SQLite row reconstruction). Use [`VaultId::parse`] at external input
    /// boundaries.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation of the vault id.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse-don't-validate: constructs a `VaultId` with strict validation.
    ///
    /// Rules (any violation → `ValidationError::InvalidVaultId`):
    /// - non-empty;
    /// - length ≤ [`VAULT_ID_MAX_LEN`] bytes (DoS protection cap);
    /// - restricted charset: `[a-z0-9-]` only (lowercase, digits, hyphens);
    /// - no leading or trailing hyphen.
    ///
    /// ## When to use
    ///
    /// Use `parse` at **untrusted input boundaries** where the vault id comes from user
    /// input or an external source. The primary such boundary in the Gradatum stack is
    /// the `gradatum-admin` CLI (argument `--tenant`). On the HTTP server path, the
    /// tenant identity comes from the `tenant_id` JWT claim, which is already validated
    /// and trusted by the time it reaches application code — JWT verification is the
    /// effective enforcement point, so server handlers call [`VaultId::new`] on that
    /// already-validated string rather than parsing it again.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::ValidationError::InvalidVaultId`] with an explanatory message.
    pub fn parse(s: &str) -> Result<Self, crate::error::ValidationError> {
        use crate::error::ValidationError;

        if s.is_empty() {
            return Err(ValidationError::InvalidVaultId(
                "empty vault_id".to_string(),
            ));
        }
        if s.len() > VAULT_ID_MAX_LEN {
            return Err(ValidationError::InvalidVaultId(format!(
                "vault_id too long ({} > {VAULT_ID_MAX_LEN} bytes)",
                s.len()
            )));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(ValidationError::InvalidVaultId(format!(
                "forbidden charset in {s:?} (allowed: a-z 0-9 -)"
            )));
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(ValidationError::InvalidVaultId(format!(
                "leading or trailing dash forbidden in {s:?}"
            )));
        }
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for VaultId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Constructs a `VaultId` from an owned `String` **without validation**.
/// Use [`VaultId::parse`] at external input boundaries.
impl From<String> for VaultId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Constructs a `VaultId` from a `&str` **without validation**.
/// Use [`VaultId::parse`] at external input boundaries.
impl From<&str> for VaultId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for VaultId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// Allows `assert_eq!(fm.vault_id, "main")` in existing tests.
impl PartialEq<&str> for VaultId {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

impl PartialEq<str> for VaultId {
    fn eq(&self, other: &str) -> bool {
        self.0.as_str() == other
    }
}

// ── TenantId — principal dimension, distinct from VaultId (namespace) ──

/// Tenant identifier — the **principal dimension** (`tenant_id` JWT claim).
///
/// Semantically **distinct** from [`VaultId`]: `TenantId` says *who* is acting (the
/// authenticated principal, taken from the mandatory `tenant_id` JWT claim, or from
/// the API key record on the direct api-key path), whereas `VaultId` says *on which
/// namespace* (the vault being targeted). One tenant may hold grants on several
/// vaults (see [`VaultGrant`]); the two axes must never be conflated —
/// `tenant_id ≠ vault_id`.
///
/// `#[serde(transparent)]` newtype → serialised and deserialised as a bare `String`,
/// so the wire format needs no migration (an exact mirror of [`VaultId`]).
///
/// ## Construction
///
/// - [`TenantId::new`] and the [`From`] impls are **not validated**: they accept any
///   string. They exist for internal use where the value is already trusted (SQLite
///   row reconstruction, tests, an already-verified JWT claim).
///   **Do not use at untrusted input boundaries.**
/// - [`TenantId::parse`] is the validated constructor: use it whenever the principal
///   comes from untrusted input (CLI argument, deserialization).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TenantId(String);

impl TenantId {
    /// Constructs a `TenantId` **without validation**.
    ///
    /// Intended for internal use where the value is already trusted (an already-verified
    /// JWT claim, SQLite row reconstruction, tests). Use [`TenantId::parse`] at
    /// untrusted input boundaries.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation of the tenant id.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse-don't-validate: constructs a `TenantId` with strict validation.
    ///
    /// Applies **exactly the same rules as [`VaultId::parse`]**, including the same
    /// [`VAULT_ID_MAX_LEN`] cap: non-empty, length ≤ cap bytes (DoS protection),
    /// charset `[a-z0-9-]`, no leading or trailing hyphen.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::ValidationError::InvalidVaultId`] with an explanatory
    /// message. Reusing the `InvalidVaultId` variant is deliberate: it covers
    /// identifiers with a vault-compatible charset, and no dedicated `InvalidTenantId`
    /// variant is introduced as long as no caller treats the two differently.
    pub fn parse(s: &str) -> Result<Self, crate::error::ValidationError> {
        use crate::error::ValidationError;

        if s.is_empty() {
            return Err(ValidationError::InvalidVaultId(
                "empty tenant_id".to_string(),
            ));
        }
        if s.len() > VAULT_ID_MAX_LEN {
            return Err(ValidationError::InvalidVaultId(format!(
                "tenant_id too long ({} > {VAULT_ID_MAX_LEN} bytes)",
                s.len()
            )));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(ValidationError::InvalidVaultId(format!(
                "forbidden charset in {s:?} (allowed: a-z 0-9 -)"
            )));
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(ValidationError::InvalidVaultId(format!(
                "leading or trailing dash forbidden in {s:?}"
            )));
        }
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Constructs a `TenantId` from an owned `String` **without validation**.
/// Use [`TenantId::parse`] at untrusted input boundaries.
impl From<String> for TenantId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Constructs a `TenantId` from a `&str` **without validation**.
/// Use [`TenantId::parse`] at untrusted input boundaries.
impl From<&str> for TenantId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for TenantId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<&str> for TenantId {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

impl PartialEq<str> for TenantId {
    fn eq(&self, other: &str) -> bool {
        self.0.as_str() == other
    }
}

// ── AgentId — credential-borne identity, distinct from TenantId (principal) ──

/// Maximum length of an `AgentId` in bytes (DoS protection cap).
pub const AGENT_ID_MAX_LEN: usize = 64;

/// Agent identity — the value **carried by the credential**, never supplied by the client.
///
/// Semantically **distinct** from [`TenantId`]: `TenantId` says *which principal
/// namespace* is acting (the `tenant_id` JWT claim / api-key column), whereas `AgentId`
/// says *which agent* holds the credential. Several agents may share one tenant; the two
/// axes must never be conflated — `agent_id ≠ tenant_id`.
///
/// The value has exactly two origins, both server-side:
/// - the `api_keys.owner` column, read from SQLite **after** argon2id verification
///   (`gradatum-acl-auth::ApiKey::owner`);
/// - the `sub` claim of a JWT whose signature has already been verified
///   (`gradatum-auth::Claims::sub`).
///
/// It is **never** read from a request header, a query parameter or a request body.
///
/// `#[serde(transparent)]` newtype → serialised and deserialised as a bare `String`,
/// so the wire format needs no migration (an exact mirror of [`TenantId`]).
///
/// ## Construction
///
/// - [`AgentId::new`] and the [`From`] impls are **not validated**: they accept any
///   string. They exist for internal use where the value is already trusted (SQLite
///   row reconstruction, tests, an already-verified JWT claim).
///   **Do not use at untrusted input boundaries.**
/// - [`AgentId::parse`] is the validated constructor: use it whenever the agent identity
///   comes from untrusted input (CLI argument, deserialization).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(String);

impl AgentId {
    /// Constructs an `AgentId` **without validation**.
    ///
    /// Intended for internal use where the value is already trusted (an already-verified
    /// JWT claim, SQLite row reconstruction, tests). Use [`AgentId::parse`] at untrusted
    /// input boundaries.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation of the agent id.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse-don't-validate: constructs an `AgentId` with strict validation.
    ///
    /// Rules (any violation → `ValidationError::InvalidAgentId`):
    /// - non-empty;
    /// - length ≤ [`AGENT_ID_MAX_LEN`] bytes (DoS protection cap);
    /// - restricted charset: `[a-z0-9-]` only (lowercase, digits, hyphens);
    /// - no leading or trailing hyphen.
    ///
    /// The last rule is **aligned** on [`VaultId::parse`] and [`TenantId::parse`], although
    /// an agent id is never a path segment. Filesystem safety is only one of the two things
    /// that rule buys; the other is **canonicity**, and canonicity is what an agent identity
    /// lives on. The value is matched by exact string equality against the `identity` field
    /// of the ACL preset, and it is rendered in padded columns (`api-key list`) and in logs,
    /// where `-engine` and `engine-` are visually indistinguishable from `engine`. Accepting
    /// them would mint exactly the credential this type exists to prevent: one that
    /// authenticates and is then denied everywhere, silently.
    ///
    /// ## When to use
    ///
    /// Use `parse` at **untrusted input boundaries** where the agent identity comes from
    /// user input or an external source — chiefly the `gradatum-admin` CLI (argument
    /// `--owner`). On the two server-side paths the value is already trusted by the time
    /// it reaches application code (argon2id-verified DB row, signature-verified JWT
    /// claim), so those call [`AgentId::new`] rather than parsing again.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::ValidationError::InvalidAgentId`] with an explanatory
    /// message.
    pub fn parse(s: &str) -> Result<Self, crate::error::ValidationError> {
        use crate::error::ValidationError;

        if s.is_empty() {
            return Err(ValidationError::InvalidAgentId(
                "empty agent_id".to_string(),
            ));
        }
        if s.len() > AGENT_ID_MAX_LEN {
            return Err(ValidationError::InvalidAgentId(format!(
                "agent_id too long ({} > {AGENT_ID_MAX_LEN} bytes)",
                s.len()
            )));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(ValidationError::InvalidAgentId(format!(
                "forbidden charset in {s:?} (allowed: a-z 0-9 -)"
            )));
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(ValidationError::InvalidAgentId(format!(
                "leading or trailing dash forbidden in {s:?}"
            )));
        }
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Constructs an `AgentId` from an owned `String` **without validation**.
/// Use [`AgentId::parse`] at untrusted input boundaries.
impl From<String> for AgentId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Constructs an `AgentId` from a `&str` **without validation**.
/// Use [`AgentId::parse`] at untrusted input boundaries.
impl From<&str> for AgentId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<&str> for AgentId {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

impl PartialEq<str> for AgentId {
    fn eq(&self, other: &str) -> bool {
        self.0.as_str() == other
    }
}

/// Locus identifier — sub-vault ACL scope.
///
/// Optional in `Frontmatter`: `None` = vault root scope.
/// `#[serde(transparent)]` → serialised/deserialised as a bare `String`.
///
/// ## Construction
///
/// - [`LocusId::new`] and the [`From`] impls are **not validated**: they accept any string.
///   They exist for internal use where the value is already trusted. **Do not use at
///   external input boundaries.**
/// - [`LocusId::parse`] is the exclusive validated constructor: use it whenever the
///   `locus_id` comes from untrusted input (HTTP request, CLI argument, YAML deserialization).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocusId(String);

impl LocusId {
    /// Constructs a `LocusId` from a string **without validation**.
    ///
    /// Intended for internal use where the value is already trusted. Use
    /// [`LocusId::parse`] at external input boundaries.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation of the locus id.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse-don't-validate: constructs a `LocusId` with strict validation.
    ///
    /// Rules (any violation → `ValidationError::InvalidLocusId`):
    /// - non-empty;
    /// - length ≤ [`LOCUS_MAX_LEN`] bytes (DoS protection cap);
    /// - restricted charset: `[a-z0-9-/]` only (lowercase, digits, hyphens, slashes);
    /// - **anti-traversal**: no `..`, no leading/trailing slash, no `//`.
    ///
    /// Guarantees that an accepted locus is a safe logical path (no directory traversal,
    /// no empty segments), usable as an ACL prefix and as a physical path without
    /// additional escaping.
    ///
    /// # Errors
    /// Returns `ValidationError::InvalidLocusId` with an explanatory message.
    pub fn parse(s: &str) -> Result<Self, crate::error::ValidationError> {
        use crate::error::ValidationError;

        if s.is_empty() {
            return Err(ValidationError::InvalidLocusId("empty locus".to_string()));
        }
        if s.len() > LOCUS_MAX_LEN {
            return Err(ValidationError::InvalidLocusId(format!(
                "locus too long ({} > {LOCUS_MAX_LEN} bytes)",
                s.len()
            )));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '/')
        {
            return Err(ValidationError::InvalidLocusId(format!(
                "forbidden charset in {s:?} (allowed: a-z 0-9 - /)"
            )));
        }
        // Anti-traversal : pas de remontée, pas de segment vide, pas de slash terminal/initial.
        if s.contains("..") {
            return Err(ValidationError::InvalidLocusId(format!(
                "directory traversal forbidden in {s:?}"
            )));
        }
        if s.starts_with('/') || s.ends_with('/') || s.contains("//") {
            return Err(ValidationError::InvalidLocusId(format!(
                "leading/trailing slash or empty segment forbidden in {s:?}"
            )));
        }
        Ok(Self(s.to_string()))
    }
}

/// Maximum length of a `LocusId` in bytes (DoS protection cap).
pub const LOCUS_MAX_LEN: usize = 128;

impl std::fmt::Display for LocusId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for LocusId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for LocusId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for LocusId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Bearer identifier.
///
/// Used for per-user/per-agent personalised overrides.
/// Consumed by `OverrideScope::Bearer` and `AclPolicy`; full implementation via `gradatum-acl-auth`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BearerId(String);

impl BearerId {
    /// Constructs a `BearerId` from a string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation of the bearer id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BearerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for BearerId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for BearerId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Scope of a distributed override.
///
/// Determines the perimeter to which an override applies in the `note_overrides` table.
/// One active override per `(vault, note, scope, type)`.
///
/// ## Tenant isolation
///
/// The `Locus` and `Bearer` variants carry the owning `vault` explicitly. This
/// vault is bound to the `vault_id` column of `note_overrides`, whose
/// PRIMARY KEY is composite `(vault_id, note_id, scope_kind, scope_id, override_type)`
/// (migration 0034). Without it, two vaults sharing a colliding `note_id` (ULID) at the
/// same locus/bearer would collide on the row key — enabling cross-vault clobber (write)
/// and cross-read (read). `Vault` needs no separate field: its `VaultId` *is* the scope.
///
/// Serialised with `#[serde(tag = "kind", content = "id")]`:
/// - `Vault` → `{ "kind": "vault", "id": "main" }`;
/// - `Locus` → `{ "kind": "locus", "id": { "vault": "main", "locus": "decisions" } }`;
/// - `Bearer` → `{ "kind": "bearer", "id": { "vault": "main", "bearer": "agent-x" } }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "kebab-case")]
pub enum OverrideScope {
    /// Override applicable to the entire vault.
    Vault(VaultId),
    /// Override restricted to a sub-vault ACL scope, within a specific vault.
    Locus {
        /// Vault owning this locus-scoped override (tenant isolation key).
        vault: VaultId,
        /// Sub-vault ACL scope.
        locus: LocusId,
    },
    /// Override personalised for a specific bearer, within a specific vault
    /// (requires `gradatum-acl-auth`).
    Bearer {
        /// Vault owning this bearer-scoped override (tenant isolation key).
        vault: VaultId,
        /// Bearer identity.
        bearer: BearerId,
    },
}

// ── VaultGrant — allow-list tenant↔vault (C1, F-63) ──────────────────────────

/// Access level granted to a tenant on a vault (allow-list `tenant_vault_grants`).
///
/// Stored in SQLite as `'read'` / `'write'` (CHECK constraint, migration 0030).
/// `Write` implies read: a write grant covers both directions on the vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum GrantAccess {
    /// Read-only access to the vault.
    Read,
    /// Read + write access to the vault.
    Write,
}

impl GrantAccess {
    /// Stable string representation for DB storage (CHECK constraint, migration 0030).
    #[must_use]
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    /// Parses the DB representation. `None` on any unknown value (fail-closed:
    /// a corrupted row must never silently grant access).
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }

    /// Returns `true` when this grant authorises write operations on the vault.
    #[must_use]
    pub fn allows_write(self) -> bool {
        matches!(self, Self::Write)
    }
}

/// Grant of a tenant onto a vault — one row of the `tenant_vault_grants` allow-list.
///
/// Part of the multi-vault substrate. The name `VaultGrant` was chosen over
/// `VaultScope`, which is already an alias of [`crate::job::JobScope`].
/// The allow-list is consulted by the auth middleware and on every scoped write path;
/// the absence of a grant is a refusal (fail-closed).
///
/// ## Section scoping
///
/// [`VaultGrant::section`] narrows the grant down to a single section of the vault:
/// - `None` (the historical default, `section` column `NULL`) → a **vault-wide**
///   grant, with exactly the original semantics;
/// - `Some("lessons-learned")` → a **section-scoped** grant: the tenant only ever
///   sees that one section of the target vault.
///
/// The coverage rule lives in [`VaultGrant::covers_section`] — the single source of
/// truth used by the server read and write guards, never re-implemented at call sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VaultGrant {
    /// Granted tenant — the principal this grant applies to, typed [`TenantId`].
    ///
    /// `TenantId` is `#[serde(transparent)]`: the wire format is a bare JSON string,
    /// strictly identical to the `String` this field used to hold — no migration, and
    /// the SQLite column stays a plain `TEXT`.
    ///
    /// The typing matches the two records that carry the authenticated principal in
    /// memory: [`crate::trust::TrustContext::BearerToken`] (field `tenant_id`) and
    /// `ApiKey::tenant_id` in `gradatum-acl-auth`. It is **not** uniform across the
    /// whole codebase, and does not claim to be: the JWT claim itself
    /// (`Claims::tenant_id` in `gradatum-auth`) and the `/auth/exchange` response body
    /// (`ExchangeResponse::tenant_id`) are still `String`. Typing those is deferred —
    /// nothing here depends on it.
    pub tenant_id: TenantId,
    /// Target vault of the grant.
    pub vault_id: VaultId,
    /// Access level on the vault.
    pub access: GrantAccess,
    /// Targeted section of the vault, or `None` for a vault-wide grant.
    ///
    /// `#[serde(default)]`: a payload written before this field existed deserialises
    /// to `None`, i.e. a vault-wide grant with the historical behaviour.
    #[serde(default)]
    pub section: Option<String>,
}

impl VaultGrant {
    /// Constructs a **vault-wide** grant (`section = None`, the historical semantics).
    ///
    /// The struct is `#[non_exhaustive]`, so literal construction is only possible
    /// inside `gradatum-core` — downstream crates must go through this constructor.
    ///
    /// `impl Into<TenantId>` keeps every existing call site source-compatible
    /// (`&str` and `String` both convert), while the stored field is typed.
    pub fn new(tenant_id: impl Into<TenantId>, vault_id: VaultId, access: GrantAccess) -> Self {
        Self::new_scoped(tenant_id, vault_id, access, None)
    }

    /// Constructs a grant optionally bounded to a single `section`.
    ///
    /// Passing `section = None` is strictly equivalent to [`VaultGrant::new`].
    pub fn new_scoped(
        tenant_id: impl Into<TenantId>,
        vault_id: VaultId,
        access: GrantAccess,
        section: Option<String>,
    ) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            vault_id,
            access,
            section,
        }
    }

    /// Returns `true` if this grant covers the requested scope (fail-closed).
    ///
    /// | grant `section` | `requested`   | covered | reason |
    /// |---|---|---|---|
    /// | `None`          | anything      | ✅ | vault-wide grant: covers every section |
    /// | `Some(s)`       | `Some(s)`     | ✅ | exact scope |
    /// | `Some(_)`       | `Some(other)` | ❌ | out of scope |
    /// | `Some(_)`       | `None`        | ❌ | a vault-wide request needs a vault-wide grant |
    ///
    /// The last row is the fail-closed invariant of the model: a grant bounded to one
    /// section NEVER opens a path whose scope is not explicitly that section — this is
    /// what keeps cross-vault search, the timeline and the write paths closed.
    #[must_use]
    pub fn covers_section(&self, requested: Option<&str>) -> bool {
        match (self.section.as_deref(), requested) {
            (None, _) => true,
            (Some(granted), Some(requested)) => granted == requested,
            (Some(_), None) => false,
        }
    }
}

// ── AgentVaultGrant — allow-list agent↔vault (B6, plan v1.0.0) ─────────────────

/// Grant of an agent onto a vault — one row of the `agent_vault_grants` allow-list.
///
/// Mirrors [`VaultGrant`] at the agent level: same structure (vault, access, section)
/// but keyed by agent identity ([`AgentId`]) instead of tenant ([`TenantId`]).
///
/// # Relationship with [`VaultGrant`]
///
/// `agent_vault_grants` duplicates the `tenant_vault_grants` pattern one level down:
/// - **tenant grant** = what vaults a tenant may access;
/// - **agent grant** = what vaults an agent may access within its tenant.
///
/// The effective access is the intersection: `min(tenant_grant, agent_grant)` —
/// an agent can only restrict, never broaden, what its tenant allows (invariant 2
/// of the rights model). Absence of a row is a refusal (fail-closed).
///
/// # Section scoping
///
/// [`AgentVaultGrant::section`] narrows the grant down to a single section of the vault:
/// - `None` → vault-wide grant;
/// - `Some("lessons-learned")` → section-scoped grant.
///
/// The coverage rule lives in [`AgentVaultGrant::covers_section`] — single source of
/// truth, never re-implemented at call sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentVaultGrant {
    /// Granted agent — the credential bearer this grant applies to, typed [`AgentId`].
    pub agent_id: AgentId,
    /// Target vault of the grant.
    pub vault_id: VaultId,
    /// Access level on the vault.
    pub access: GrantAccess,
    /// Targeted section of the vault, or `None` for a vault-wide grant.
    #[serde(default)]
    pub section: Option<String>,
}

impl AgentVaultGrant {
    /// Constructs a **vault-wide** grant (`section = None`).
    pub fn new(agent_id: impl Into<AgentId>, vault_id: VaultId, access: GrantAccess) -> Self {
        Self::new_scoped(agent_id, vault_id, access, None)
    }

    /// Constructs a grant optionally bounded to a single `section`.
    ///
    /// Passing `section = None` is strictly equivalent to [`AgentVaultGrant::new`].
    pub fn new_scoped(
        agent_id: impl Into<AgentId>,
        vault_id: VaultId,
        access: GrantAccess,
        section: Option<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            vault_id,
            access,
            section,
        }
    }

    /// Returns `true` if this grant covers the requested scope (fail-closed).
    ///
    /// Same semantics as [`VaultGrant::covers_section`] — single source of truth.
    #[must_use]
    pub fn covers_section(&self, requested: Option<&str>) -> bool {
        match (self.section.as_deref(), requested) {
            (None, _) => true,
            (Some(granted), Some(requested)) => granted == requested,
            (Some(_), None) => false,
        }
    }
}

// ── TenantStatus — lifecycle of a tenant/vault ───────────────────────────────

/// Lifecycle status of a tenant (`tenants` table, migrations 0030/0031).
///
/// - `Active` — grants are visible, jobs iterate over the tenant, reads and writes
///   are allowed.
/// - `Suspended` — immediate refusal: the `tenants.status = 'active'` join performed
///   by the grant lookup returns nothing. Reversible.
/// - `Deleted` — soft delete, refused just as immediately; the physical purge of the
///   notes is left to the existing purge jobs, so no destructive schema change is ever
///   applied to `notes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TenantStatus {
    /// Operational tenant.
    Active,
    /// Frozen tenant — immediate refusal, reversible.
    Suspended,
    /// Soft-deleted tenant — physical purge deferred.
    Deleted,
}

impl TenantStatus {
    /// Stable database representation (matches the migration `CHECK` constraint).
    #[must_use]
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Deleted => "deleted",
        }
    }

    /// Parses the database representation. Returns `None` on an unknown value —
    /// fail-closed, so that a corrupted row can never reactivate a tenant.
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tenant_status_tests {
    use super::*;

    /// Round-trip DB pour chaque variante + inconnu → None (fail-closed).
    #[test]
    fn tenant_status_db_round_trip_and_unknown() {
        for st in [
            TenantStatus::Active,
            TenantStatus::Suspended,
            TenantStatus::Deleted,
        ] {
            assert_eq!(TenantStatus::from_db_str(st.as_db_str()), Some(st));
        }
        assert_eq!(TenantStatus::from_db_str("ACTIVE"), None);
        assert_eq!(TenantStatus::from_db_str(""), None);
    }
}

// ── AclCheckedVaultId — witness that the target vault's ACL was checked ──────

/// A target vault whose **read** ACL has been evaluated — a compile-time witness.
///
/// The low-level read functions exposed to the cross-vault path
/// (`search_fts_with_snippet`, `search_semantic`, `timeline`, the batch title/status/anchor
/// lookups) no longer accept a bare `vault_id`. This type proves at the call site that the
/// ACL of the **target** vault was evaluated — never just the caller's own, which was the
/// historical hole this type closes.
///
/// ## What is actually guaranteed (and where it stops)
///
/// Rust cannot prove across crates that an access check really happened: the guarantee is
/// **anti-forgetting**, not absolute. A `vault_id` coming from a request can no longer be
/// passed *silently* into a read — it has to go through one of the named constructors
/// below, each greppable during review:
///
/// - [`AclCheckedVaultId::attest_read_checked`] — the caller attests it evaluated the
///   target's Read ACL (and, when `multi_tenant.enabled = true`, the per-vault grant);
/// - [`AclCheckedVaultId::for_system_task`] — a system context outside any HTTP request
///   (periodic job, offline operator CLI, internal loopback surface), where the scope is
///   guaranteed by the orchestrator rather than by a per-request ACL.
///
/// To audit the surface, `grep -rn "attest_read_checked\|attest_write_checked\|for_system_task"`
/// enumerates every entry point — reads and ULID-addressed mutations alike — that builds
/// the witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclCheckedVaultId(VaultId);

impl AclCheckedVaultId {
    /// Attests that the Read ACL of the **target** `vault` has just evaluated to `Allow`.
    ///
    /// Calling contract: invoke it only right after an
    /// `acl.evaluate(trust, AclOp::Read, locus(target))` returned `Allow` — never on the
    /// caller's own locus when the target differs from the caller.
    #[must_use]
    pub fn attest_read_checked(vault: VaultId) -> Self {
        Self(vault)
    }

    /// Attests that the **Write** ACL of the **target** `vault` has just evaluated to
    /// `Allow` — required for ULID-addressed mutations on the vault-shared `notes` table.
    ///
    /// Calling contract: invoke it only after `acl.evaluate(trust, AclOp::Write, locus(target))`
    /// returned `Allow` (plus, when `multi_tenant.enabled = true`, the tenant's write grant
    /// on its own vault). It separates the write path from the read path
    /// ([`Self::attest_read_checked`]) on purpose: the ULID-addressed mutations
    /// (`downgrade_note`, `patch_note_status`, `update_note_locus`) demand this witness so
    /// that an `AND vault_id = ?` filter is always applied. Without it, a legitimate remote
    /// tenant could mutate a note in any vault simply by naming its ULID.
    #[must_use]
    pub fn attest_write_checked(vault: VaultId) -> Self {
        Self(vault)
    }

    /// Builds the witness for a **system** context (job, offline CLI, loopback surface).
    ///
    /// Outside an HTTP request there is no per-request ACL: the scope is guaranteed by the
    /// job orchestrator, which iterates over active vaults one at a time, or by the admin
    /// surface (loopback plus operator token).
    #[must_use]
    pub fn for_system_task(vault: VaultId) -> Self {
        Self(vault)
    }

    /// The verified target vault.
    #[must_use]
    pub fn vault_id(&self) -> &VaultId {
        &self.0
    }

    /// String representation of the verified target vault.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for AclCheckedVaultId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod acl_checked_vault_id_tests {
    use super::*;

    /// Le témoin restitue le vault tel quel par les deux constructeurs.
    #[test]
    fn witness_carries_vault_through_both_constructors() {
        let a = AclCheckedVaultId::attest_read_checked(VaultId::new("main"));
        let b = AclCheckedVaultId::for_system_task(VaultId::new("code-gradatum"));
        assert_eq!(a.as_str(), "main");
        assert_eq!(b.vault_id().as_str(), "code-gradatum");
    }
}

#[cfg(test)]
mod vault_grant_tests {
    use super::*;

    /// Round-trip DB : `as_db_str` ↔ `from_db_str` pour chaque variante.
    #[test]
    fn grant_access_db_round_trip() {
        for access in [GrantAccess::Read, GrantAccess::Write] {
            assert_eq!(
                GrantAccess::from_db_str(access.as_db_str()),
                Some(access),
                "round-trip DB pour {access:?}"
            );
        }
    }

    /// Valeur DB inconnue → `None` (fail-closed, jamais un grant par défaut).
    #[test]
    fn grant_access_unknown_db_value_is_none() {
        for bad in ["", "admin", "WRITE", "rw"] {
            assert_eq!(
                GrantAccess::from_db_str(bad),
                None,
                "{bad:?} doit être None"
            );
        }
    }

    /// Seul `Write` autorise l'écriture.
    #[test]
    fn only_write_allows_write() {
        assert!(GrantAccess::Write.allows_write());
        assert!(!GrantAccess::Read.allows_write());
    }

    /// `VaultGrant::new` porte les trois champs tels quels.
    #[test]
    fn vault_grant_new_carries_fields() {
        let g = VaultGrant::new("main", VaultId::new("main"), GrantAccess::Write);
        assert_eq!(g.tenant_id, "main");
        assert_eq!(g.vault_id.as_str(), "main");
        assert_eq!(g.access, GrantAccess::Write);
    }

    /// L3 (F-121) : `VaultGrant::new` reste un grant VAULT-ENTIER (`section = None`).
    #[test]
    fn vault_grant_new_is_vault_wide() {
        let g = VaultGrant::new("main", VaultId::new("main"), GrantAccess::Write);
        assert_eq!(g.section, None, "new() doit rester vault-entier (C1)");
    }

    /// L3 (F-121) : un grant vault-entier couvre toute portée demandée.
    #[test]
    fn vault_wide_grant_covers_any_section() {
        let g = VaultGrant::new("b", VaultId::new("main"), GrantAccess::Read);
        assert!(g.covers_section(None));
        assert!(g.covers_section(Some("lessons-learned")));
        assert!(g.covers_section(Some("decisions")));
    }

    /// L3 (F-121) : un grant section-scopé ne couvre QUE sa section.
    #[test]
    fn scoped_grant_covers_only_its_own_section() {
        let g = VaultGrant::new_scoped(
            "b",
            VaultId::new("main"),
            GrantAccess::Read,
            Some("lessons-learned".to_owned()),
        );
        assert!(g.covers_section(Some("lessons-learned")));
        assert!(!g.covers_section(Some("decisions")));
    }

    /// L3 (F-121) — fail-closed : un grant section-scopé ne satisfait JAMAIS une
    /// demande vault-entier (recherche cross-vault, timeline, chemin d'écriture).
    #[test]
    fn scoped_grant_never_covers_vault_wide_request() {
        let g = VaultGrant::new_scoped(
            "b",
            VaultId::new("main"),
            GrantAccess::Write,
            Some("lessons-learned".to_owned()),
        );
        assert!(!g.covers_section(None));
    }

    /// Neutralité sur le fil (typage `tenant_id: TenantId`) — SENS SÉRIALISATION.
    ///
    /// `TenantId` étant `#[serde(transparent)]`, `tenant_id` sort en **chaîne JSON
    /// nue**, jamais en objet : le JSON produit est byte-identical à celui de la
    /// version où le champ était un `String`. C'est ce qui garantit qu'aucun
    /// consommateur (API, fixture, colonne SQLite TEXT) ne casse.
    #[test]
    fn vault_grant_serialises_tenant_id_as_a_bare_json_string() {
        let g = VaultGrant::new_scoped(
            "main",
            VaultId::new("archive"),
            GrantAccess::Write,
            Some("decisions".to_owned()),
        );
        let json = serde_json::to_string(&g).expect("VaultGrant est sérialisable — invariant test");
        assert_eq!(
            json,
            r#"{"tenant_id":"main","vault_id":"archive","access":"write","section":"decisions"}"#
        );
    }

    /// Neutralité sur le fil — SENS DÉSÉRIALISATION.
    ///
    /// Un payload écrit AVANT le typage (`tenant_id` chaîne nue, `section` absente
    /// = grant vault-entier d'avant la migration 0040) se relit à l'identique.
    #[test]
    fn vault_grant_deserialises_a_pre_typing_payload_unchanged() {
        let legacy = r#"{"tenant_id":"main","vault_id":"archive","access":"read"}"#;
        let g: VaultGrant =
            serde_json::from_str(legacy).expect("payload legacy relisible — invariant test");
        assert_eq!(
            g,
            VaultGrant::new("main", VaultId::new("archive"), GrantAccess::Read)
        );
    }
}

#[cfg(test)]
mod vault_id_parse_tests {
    use super::*;
    use crate::error::ValidationError;

    /// `VaultId::parse` accepte les identifiants valides.
    #[test]
    fn vault_id_parse_accepts_valid() {
        for ok in ["main", "a", "my-vault", "vault01", "v0-prod", "abc123"] {
            assert!(
                VaultId::parse(ok).is_ok(),
                "{ok:?} doit être accepté par VaultId::parse"
            );
        }
    }

    /// `VaultId::parse` rejette vide, charset interdit, tiret tête/queue, trop long.
    #[test]
    fn vault_id_parse_rejects_invalid() {
        let bad = [
            "",         // vide
            "Main",     // majuscule
            "my vault", // espace
            "my_vault", // underscore
            "my/vault", // slash
            "café",     // non-ascii
            "-main",    // tiret initial
            "main-",    // tiret terminal
        ];
        for b in bad {
            assert!(
                VaultId::parse(b).is_err(),
                "{b:?} doit être rejeté par VaultId::parse"
            );
        }
        // Trop long (> VAULT_ID_MAX_LEN).
        let long = "a".repeat(VAULT_ID_MAX_LEN + 1);
        assert!(
            VaultId::parse(&long).is_err(),
            "vault_id > max doit être rejeté"
        );
        // Exactement la borne → accepté.
        let at_limit = "a".repeat(VAULT_ID_MAX_LEN);
        assert!(
            VaultId::parse(&at_limit).is_ok(),
            "vault_id == max doit être accepté"
        );
    }

    /// `VaultId::parse` produit `ValidationError::InvalidVaultId` (jamais une autre variante).
    #[test]
    fn vault_id_parse_produces_correct_error_variant() {
        let err = VaultId::parse("").unwrap_err();
        assert!(
            matches!(err, ValidationError::InvalidVaultId(_)),
            "attendu InvalidVaultId, obtenu {:?}",
            err
        );

        let err = VaultId::parse("Bad-Vault!").unwrap_err();
        assert!(
            matches!(err, ValidationError::InvalidVaultId(_)),
            "attendu InvalidVaultId, obtenu {:?}",
            err
        );
    }
}

#[cfg(test)]
mod tenant_id_tests {
    use super::*;

    /// `TenantId` sérialise/désérialise comme un `String` nu (`#[serde(transparent)]`).
    #[test]
    fn tenant_id_serde_round_trip_is_bare_string() {
        let t = TenantId::new("main");
        let json = serde_json::to_string(&t).expect("sérialisation TenantId");
        assert_eq!(
            json, "\"main\"",
            "TenantId doit sérialiser comme un String nu"
        );
        let back: TenantId = serde_json::from_str(&json).expect("désérialisation TenantId");
        assert_eq!(back, t);
        // Désérialisation depuis un String nu (compat wire cross-dimension).
        let from_bare: TenantId = serde_json::from_str("\"tenant-a\"").expect("depuis String nu");
        assert_eq!(from_bare.as_str(), "tenant-a");
    }

    /// `parse("main")` accepte un principal valide ; `parse("")` le rejette.
    #[test]
    fn tenant_id_parse_accepts_valid_rejects_empty() {
        assert!(TenantId::parse("main").is_ok(), "main doit être accepté");
        assert!(
            TenantId::parse("").is_err(),
            "tenant_id vide doit être rejeté"
        );
    }

    /// `Display` et `as_str` restituent la même chaîne sous-jacente.
    #[test]
    fn tenant_id_display_and_as_str_are_consistent() {
        let t = TenantId::from("tenant-42");
        assert_eq!(t.as_str(), "tenant-42");
        assert_eq!(t.to_string(), "tenant-42");
        // PartialEq<&str> pour ergonomie des assertions.
        assert_eq!(t, "tenant-42");
    }
}

#[cfg(test)]
mod locus_parse_tests {
    use super::*;

    /// `LocusId::parse` accepts well-formed locus strings.
    #[test]
    fn locus_parse_accepts_valid() {
        for ok in ["knowledge", "knowledge/rust", "a", "a-b/c-d/e9", "x/y/z"] {
            assert!(LocusId::parse(ok).is_ok(), "{ok:?} doit être accepté");
        }
    }

    /// `LocusId::parse` rejects empty, forbidden charset, path traversal, trailing slash, and over-length inputs.
    #[test]
    fn locus_parse_rejects_invalid() {
        let bad = [
            "",               // vide
            "Knowledge",      // majuscule
            "knowledge rust", // espace
            "know_ledge",     // underscore
            "../etc",         // traversal
            "a/../b",         // traversal interne
            "/knowledge",     // slash initial
            "knowledge/",     // slash terminal
            "a//b",           // segment vide
            "café",           // non-ascii
        ];
        for b in bad {
            assert!(LocusId::parse(b).is_err(), "{b:?} doit être rejeté");
        }
        // Trop long (> LOCUS_MAX_LEN).
        let long = "a".repeat(LOCUS_MAX_LEN + 1);
        assert!(
            LocusId::parse(&long).is_err(),
            "locus > max doit être rejeté"
        );
        // Exactement la borne → accepté.
        let at_limit = "a".repeat(LOCUS_MAX_LEN);
        assert!(
            LocusId::parse(&at_limit).is_ok(),
            "locus == max doit être accepté"
        );
    }
}

#[cfg(test)]
mod agent_id_tests {
    use super::*;
    use crate::error::ValidationError;

    /// `AgentId` sérialise/désérialise comme un `String` nu (`#[serde(transparent)]`).
    #[test]
    fn agent_id_serde_round_trip_is_bare_string() {
        let a = AgentId::new("main-agent");
        let json = serde_json::to_string(&a).expect("sérialisation AgentId");
        assert_eq!(
            json, "\"main-agent\"",
            "AgentId doit sérialiser comme un String nu"
        );
        let back: AgentId = serde_json::from_str(&json).expect("désérialisation AgentId");
        assert_eq!(back, a);
        // Désérialisation depuis un String nu (compat wire pré-typage).
        let from_bare: AgentId = serde_json::from_str("\"claude-code\"").expect("depuis String nu");
        assert_eq!(from_bare.as_str(), "claude-code");
    }

    /// `parse` accepte les identités d'agent réellement en service.
    #[test]
    fn agent_id_parse_accepts_live_identities() {
        for ok in [
            "main-agent",
            "claude-code",
            "gradatum-worker",
            "engine",
            "a1",
        ] {
            assert!(AgentId::parse(ok).is_ok(), "{ok:?} doit être accepté");
        }
    }

    /// `parse` rejette vide, charset hors `[a-z0-9-]`, et au-delà du cap.
    #[test]
    fn agent_id_parse_rejects_invalid() {
        let bad = [
            "",            // vide
            "Main-Agent",  // majuscules
            "main_agent",  // underscore
            "main agent",  // espace
            "main/agent",  // slash
            "agent\u{e9}", // non-ascii
            "-engine",     // tiret initial (B6′b : aligné sur VaultId/TenantId)
            "engine-",     // tiret final
            "-",           // tiret seul
        ];
        for b in bad {
            assert!(AgentId::parse(b).is_err(), "{b:?} doit être rejeté");
        }
        // Exactement la borne → accepté ; un octet de plus → rejeté.
        let at_limit = "a".repeat(AGENT_ID_MAX_LEN);
        assert!(
            AgentId::parse(&at_limit).is_ok(),
            "agent_id == cap doit être accepté"
        );
        let too_long = "a".repeat(AGENT_ID_MAX_LEN + 1);
        assert!(
            AgentId::parse(&too_long).is_err(),
            "agent_id > cap doit être rejeté"
        );
    }

    /// La règle « pas de tiret initial/final » est ALIGNÉE sur `VaultId`/`TenantId` (B6′b).
    ///
    /// Discriminant : ce test échoue sur la version B6′a de `parse`, qui documentait
    /// l'écart et acceptait `-engine`. Il fixe l'arbitrage laissé ouvert — la forme
    /// canonique est la seule acceptée, parce que `-engine` est indistinguable de
    /// `engine` en colonne padée et en log, et qu'un `owner` non canonique produit
    /// exactement la clé menteuse que ce lot ferme.
    #[test]
    fn agent_id_parse_rejects_leading_and_trailing_dash_like_its_siblings() {
        for form in ["-engine", "engine-", "-engine-"] {
            assert!(
                AgentId::parse(form).is_err(),
                "{form:?} n'est pas canonique — doit être rejeté comme pour VaultId/TenantId"
            );
            assert!(
                VaultId::parse(form).is_err() && TenantId::parse(form).is_err(),
                "témoin : les types frères rejettent déjà {form:?}"
            );
        }
        // La forme canonique, elle, reste acceptée — la règle ne mord que sur les bords.
        assert!(AgentId::parse("engine").is_ok());
        assert!(AgentId::parse("main-agent").is_ok());
    }

    /// L'échec porte la variante dédiée `InvalidAgentId` — jamais `InvalidVaultId`.
    ///
    /// Discriminant : un `AgentId` mal formé ne doit pas être rapporté comme un
    /// vault_id invalide (message d'erreur mensonger côté opérateur).
    #[test]
    fn agent_id_parse_produces_its_own_error_variant() {
        let err = AgentId::parse("Bad Owner").expect_err("doit être rejeté");
        assert!(
            matches!(err, ValidationError::InvalidAgentId(_)),
            "variante attendue InvalidAgentId, obtenue {err:?}"
        );
    }

    /// `Display` et `as_str` restituent la même chaîne sous-jacente.
    #[test]
    fn agent_id_display_and_as_str_are_consistent() {
        let a = AgentId::new("engine");
        assert_eq!(a.to_string(), a.as_str());
        assert_eq!(a, "engine");
    }
}
