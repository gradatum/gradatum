//! # gradatum-acl-auth
//!
//! Bearer credential verification (argon2id) and per-vault scope enforcement.
//!
//! ## Stability
//!
//! `2.0.0` — public API under [SemVer 2.0.0](https://semver.org): backward-compatible
//! additions only within `2.x`, breaking changes deferred to the next major. See
//! [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Architecture
//!
//! ```text
//! Operator                     Consumer
//!   api-key create
//!     -> SqliteApiKeyStore
//!          generate(256-bit secret)
//!          argon2id hash
//!          persist row
//!          stdout: ak_xxx (ONCE ONLY)
//!
//!                              POST /auth/exchange
//!                              Authorization: Bearer ak_xxx
//!                               -> verify(ak_xxx) -> owner + scopes + tenant_id
//!                               -> JwtService::sign(sub, scopes, TokenScope::Service, tenant_id)
//!                               -> JWT cached by Consumer
//!                               -> Authorization: Bearer <JWT> on /api/v1/*
//! ```
//!
//! ## Security
//!
//! - Secrets are never stored in plaintext — argon2id hash only, persisted in the DB.
//! - `verify()` performs a constant-time argon2id compare (guaranteed by the `argon2`
//!   crate). The not-found path runs a dummy argon2 compare so that a missing prefix
//!   and a wrong secret take comparable time (no prefix-existence timing oracle).
//! - `revoke()` returns `ApiKeyError::NotFound` if the prefix is unknown.
//! - `rotate()` is atomic (BEGIN/COMMIT SQLite).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod api_key;

pub use api_key::{
    ApiKey, ApiKeyError, ApiKeyMaterial, ApiKeyStore, IDENTITY_WRITE_SCOPE, KEY_PREFIX, SECRET_LEN,
    SqliteApiKeyStore, WRITE_SCOPES, has_write_scope,
};

/// Crate version sourced from `workspace.package.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
