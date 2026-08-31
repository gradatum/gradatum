//! Drift detection between the SQLite index and `.md` files on disk.
//!
//! Delegates to `gradatum-index::drift::scan_phase_a`.
//!
//! ## Phase-A scan
//!
//! - Verifies `file_checksums` entries: size + 4 KB prefix, then full SHA-256.
//! - Returns a `DriftScanResult` with counters and the list of missing files.
//!
//! ## Untracked-file detection (phase B) + vector dimension
//!
//! `scan_phase_a` now also walks the filesystem for `.md` note files absent from
//! `file_checksums` (`DriftScanResult::untracked`, direction disk → index) and counts
//! live notes with no embedding (`DriftScanResult::live_notes_without_vector`, vector
//! dimension). **Detection only** — reconstruction (re-parse + re-index + re-embed) remains
//! deferred to its dedicated, gated entry point: the scan signals, it never repairs.

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
    /// ## Both directions + vector dimension
    ///
    /// The scan also reports files present on disk but absent from `file_checksums`
    /// (`untracked`) and live notes without an embedding (`live_notes_without_vector`).
    /// Reconstruction (re-parse + re-index + re-embed) stays deferred to its dedicated
    /// gated entry point — the scan detects, it does not repair.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::Storage` if reading checksums from SQLite fails.
    /// - `GradatumError::Storage` if reading an existing file fails (permissions, etc.).
    pub async fn drift_check(&self) -> Result<DriftScanResult, GradatumError> {
        // `&self.storage` (NoteWriteGuard) coerces to `&dyn Storage` for the read-only scan.
        scan_phase_a(&self.storage, &self.index).await
    }
}
