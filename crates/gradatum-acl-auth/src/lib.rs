//! # gradatum-acl-auth
//!
//! Vérification de credentials bearer (argon2id) + enforcement de scopes par vault.
//!
//! ## Stability
//!
//! `0.x` — aucune garantie de stabilité API. Tous les traits publics sont tagués
//! `#[stability::unstable]` selon RELEASE-POLICY.md AM1.
//!
//! ## Architecture (flow Path 2)
//!
//! ```text
//! Operator                     Consumer
//!   api-key create
//!     -> SqliteApiKeyStore
//!          generate(256-bit secret)
//!          argon2id hash
//!          persist row
//!          stdout: ak_xxx (UNE SEULE FOIS)
//!
//!                              POST /auth/exchange
//!                              Authorization: Bearer ak_xxx
//!                               -> verify(ak_xxx) -> owner + scopes + tenant_id
//!                               -> JwtService::sign(sub, scopes, TokenScope::Service, tenant_id)
//!                               -> JWT cached by Consumer
//!                               -> Authorization: Bearer <JWT> on /api/v1/*
//! ```
//!
//! ## Securite
//!
//! - Secrets JAMAIS stockes en clair -- argon2id hash uniquement en DB.
//! - `verify()` est constant-time (argon2 crate garantit la comparaison CT).
//! - `revoke()` retourne `ApiKeyError::NotFound` si le prefixe est inconnu.
//! - `rotate()` est atomique (BEGIN/COMMIT SQLite).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod api_key;

pub use api_key::{
    ApiKey, ApiKeyError, ApiKeyMaterial, ApiKeyStore, SqliteApiKeyStore, KEY_PREFIX, SECRET_LEN,
};

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
