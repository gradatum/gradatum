//! Drift detection between the SQLite index and `.md` files on disk.
//!
//! Delegates to `gradatum-index::drift::scan_phase_a`.
//!
//! ## Phase-A scan
//!
//! - Verifies `file_checksums` entries: size + 4 KB prefix, then full SHA-256.
//! - Returns a `DriftScanResult` with counters and the list of missing files.
//!
//! ## Untracked-file detection (phase B)
//!
//! Filesystem walk for `.md` files absent from `file_checksums`, plus reconstruction
//! (re-parse + re-index + `AuditEvent::DriftFixed`) are deferred.

use gradatum_core::error::GradatumError;
use gradatum_index::drift::{DriftScanResult, scan_phase_a};

use crate::registry::Vault;

impl Vault {
    /// Triggers a phase-A drift scan across the entire vault.
    ///
    /// ## Phase-A algorithm (3 levels)
    ///
    /// For each entry in the `file_checksums` table:
    /// 1. **Size + 4 KB prefix**: if identical → short-circuit (file likely unchanged).
    /// 2. **Full SHA-256**: full hash if size/prefix differ.
    ///    - Match → `level3_full_hash_match` (cosmetic modification).
    ///    - Mismatch → `level3_full_hash_mismatch` (drift confirmed, reconstruction required).
    ///
    /// Files absent on disk are collected in `DriftScanResult::missing`.
    ///
    /// ## OpenDAL data path
    ///
    /// `scan_phase_a` receives `&self.storage` (`FileStorage` OpenDAL) — drift I/O
    /// goes through the `Storage` abstraction.
    ///
    /// ## Phase B / Phase C
    ///
    /// Detection of untracked files (present on disk, absent from `file_checksums`)
    /// and reconstruction (re-parse + re-index) are deferred.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::Storage` if reading checksums from SQLite fails.
    /// - `GradatumError::Storage` if reading an existing file fails (permissions, etc.).
    pub async fn drift_check(&self) -> Result<DriftScanResult, GradatumError> {
        scan_phase_a(self.storage.as_ref(), &self.index).await
    }
}
