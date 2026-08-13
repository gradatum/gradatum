//! # gradatum-storage
//!
//! Storage trait abstraction over OpenDAL. Ships a local filesystem backend and, selected
//! by configuration, an S3 object backend — see *Implementations* below.
//!
//! Note: the concrete type is still named [`FileStorage`] for backend compatibility; it is
//! the generic OpenDAL wrapper (any backend), not filesystem-specific. A rename is deferred.
//!
//! ## Core trait
//!
//! [`Storage`] — async primitives: Read/Write/List/Delete/Stat/Exists.
//!
//! ## Implementations
//!
//! - [`FileStorage`] — the generic OpenDAL wrapper (any backend). `FileStorage::new`
//!   builds a local filesystem operator (feature `fs`, default).
//! - [`build_storage`] — selects the backend from configuration: local `fs`, or S3 object
//!   storage (feature `s3`). GCS / Azure are declared, not yet wired in the factory.
//!
//! ## Stability
//!
//! `2.0.0` — public API under [SemVer 2.0.0](https://semver.org); backward-compatible
//! additions only within `2.x`. See
//! [`RELEASE-POLICY.md`](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod error;
pub mod storage_trait;

// Generic OpenDAL wrapper (backend-agnostic) — always compiled.
pub mod file;

// Config-driven backend selection.
pub mod factory;

// Public re-exports.
pub use error::StorageError;
pub use storage_trait::{Storage, StorageEntry};

pub use factory::build_storage;
pub use file::FileStorage;

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Installs, once and in order, the process-wide defaults that OpenDAL's object
/// (HTTP) backends require: the crypto provider first, then the HTTP transport.
///
/// OpenDAL 0.58 made its HTTP transport pluggable (apache/opendal#7900): a build
/// without an installed transport rejects **every** S3/GCS/Azure operation *before*
/// the first network packet, with a permanent `ConfigInvalid` — never a silent no-op.
/// This function installs the transport explicitly, following OpenDAL's own guidance
/// for applications that manage their process-wide transport themselves (facade
/// `opendal::lib.rs`).
///
/// ## Ordering — why it matters
///
/// The reqwest transport is built with `rustls-no-provider`: it carries no crypto
/// provider and reads the process-default `rustls::crypto::CryptoProvider`. On a
/// deployment **without** TLS termination (loopback, the default), nothing else
/// installs a provider — the server's TLS path (`load_tls_config`) is only taken when
/// `[server.tls]` is configured. So the provider MUST be installed here, unconditionally,
/// before the transport. `aws_lc_rs` is the deliberate choice (single provider across the
/// process — same as `axum-server`), avoiding a second (`ring`) provider.
///
/// ## Idempotence & scope
///
/// Both installs are first-installed-wins and safe to call repeatedly. Call once at
/// startup on **every process that builds an object `Operator` directly**: the server
/// (at boot, off the TLS path) and the object-backend integration tests. The worker does
/// **not** build one — it routes all persistence through the server's `/internal/v1/`
/// API (worker-flip), so it needs no call.
///
/// No-op when this build has no object backend feature enabled (`fs`-only build): no
/// HTTP transport is needed and no crypto provider is pulled.
pub fn install_object_backend_defaults() {
    #[cfg(feature = "cloud-http")]
    {
        // 1. Crypto provider first (see "Ordering" above). Ignore "already installed":
        //    first-wins, and every install site in the codebase uses aws_lc_rs.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        // 2. Then the HTTP transport. First-installed-wins; later calls are ignored.
        opendal_http_transport_reqwest::install_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
