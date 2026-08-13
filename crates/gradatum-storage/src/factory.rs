//! Storage factory — builds a `Storage` backend from configuration.
//!
//! [`build_storage`] is the **single point** where the concrete OpenDAL service is
//! selected. The selection is a closed `match` on the configured service name, and each
//! arm does nothing but hand OpenDAL its own typed builder. No operation code
//! (`read`/`write`/`list`/…) ever branches on the provider — that boundary belongs to
//! OpenDAL, not to gradatum.
//!
//! ## Credentials
//!
//! Object services load their credentials from the process environment via OpenDAL's
//! native credential chain (e.g. `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`,
//! `AWS_ENDPOINT_URL`). Gradatum reads no secret and forwards no secret: only the
//! declarative, non-secret connection parameters from [`StorageBackendConfig`] are set
//! on the builder.
//!
//! ## Feature gating
//!
//! Each object arm sits behind its Cargo feature. Compiled without the feature, no SDK
//! for that service enters the dependency tree, and requesting the service at runtime
//! yields a clear [`StorageError::ConfigInvalid`] naming it — never a silent no-op.

use std::path::Path;

use gradatum_core::config::StorageBackendConfig;

use crate::error::StorageError;
#[cfg(any(feature = "fs", feature = "s3"))]
use crate::file::FileStorage;
use crate::storage_trait::Storage;

/// Builds the storage backend for a vault's Markdown notes from configuration.
///
/// - `cfg` — the `[storage]` section (defaults to `service = "fs"` when absent).
/// - `local_root` — the vault's local root path, used by the `fs` backend. Object
///   backends ignore it (their location comes entirely from `cfg`).
///
/// Returns a `Box<dyn Storage>`: the concrete backend is chosen at construction and
/// hidden behind the trait object, so every consumer stays backend-agnostic. Dynamic
/// dispatch is free here — the trait is `#[async_trait]` (each call is already boxed)
/// and every method is millisecond-scale I/O.
///
/// This is a **factory**: any subsystem that needs a handle to the same backend (the
/// vault, and — once routed through the trait — the audit writer) calls it. The
/// underlying OpenDAL `Operator` is internally `Arc`-backed, so independent handles over
/// the same configuration are cheap and equivalent; no shared ownership is required.
///
/// # Errors
///
/// - [`StorageError::ConfigInvalid`] — unknown service, feature not enabled in this
///   build, or a missing required parameter (e.g. `bucket` for `s3`).
/// - [`StorageError::OpenDal`] — the backend rejected the construction.
///
/// Backend-specific connection or credential failures surface later, on first access,
/// as [`StorageError::OpenDal`] — with a message that names what is missing and never
/// contains a secret.
#[cfg_attr(not(feature = "fs"), allow(unused_variables))]
pub fn build_storage(
    cfg: &StorageBackendConfig,
    local_root: &Path,
) -> Result<Box<dyn Storage>, StorageError> {
    match cfg.service.as_str() {
        // The filesystem arm: build the local backend, nothing more.
        #[cfg(feature = "fs")]
        "fs" => Ok(Box::new(FileStorage::new(local_root)?)),

        // The S3 arm: hand OpenDAL its typed S3 builder. S3-compatible providers
        // (AWS, OVH, MinIO, Ceph, Scaleway) are all reached through a configurable
        // endpoint — a single arm covers them, by design.
        #[cfg(feature = "s3")]
        "s3" => Ok(Box::new(build_s3(cfg)?)),

        // Unknown service name, or a service whose Cargo feature is not enabled in this
        // build. Fail loudly and name it — a configuration switch that silently does
        // nothing is worse than no switch.
        other => Err(StorageError::ConfigInvalid(format!(
            "unsupported or disabled storage service {other:?} \
             (is its Cargo feature enabled in this build?)"
        ))),
    }
}

/// Builds an S3-backed `Storage` from configuration.
///
/// Only non-secret connection parameters are set on the builder. Credentials are never
/// touched here — OpenDAL's S3 service loads them from the environment.
#[cfg(feature = "s3")]
fn build_s3(cfg: &StorageBackendConfig) -> Result<FileStorage, StorageError> {
    use opendal::{Operator, services};

    let bucket = cfg.bucket.as_deref().ok_or_else(|| {
        StorageError::ConfigInvalid("storage service \"s3\" requires a 'bucket'".to_owned())
    })?;

    let mut builder = services::S3::default().bucket(bucket);
    if let Some(endpoint) = cfg.endpoint.as_deref() {
        builder = builder.endpoint(endpoint);
    }
    if let Some(region) = cfg.region.as_deref() {
        builder = builder.region(region);
    }
    if let Some(root) = cfg.root.as_deref() {
        builder = builder.root(root);
    }
    // Credentials are deliberately NOT set: `disable_config_load` stays false, so the
    // S3 service loads `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (and, if the
    // endpoint is omitted, `AWS_ENDPOINT_URL`) from the environment on its own.

    let op = Operator::new(builder).map_err(|e| StorageError::OpenDal(e.to_string()))?;
    // `root` is diagnostic only; for S3 it carries the configured key prefix, if any.
    let root_label = std::path::PathBuf::from(cfg.root.clone().unwrap_or_default());
    Ok(FileStorage::from_operator(op, root_label))
}
