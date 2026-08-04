//! Helper drift scan — three-level pre-check before reconstruction.
//!
//! ## Algorithm
//!
//! For each entry in `file_checksums`:
//!
//! 1. **Level 1 — strict size**: if `on_disk_size == expected_size`
//!    AND `prefix-4 KB hash == expected_hash_prefix_4kb` → file is likely unchanged.
//!    Short-circuits ~99% of stable files (fast sequential reads).
//!
//! 2. **Level 3 — full SHA-256**: all other cases → full hash.
//!    Determines whether the file is actually modified (`mismatch`) or not (`match`).
//!
//! ## Caller responsibilities
//!
//! This helper returns a `DriftScanResult` with counters and the list of missing files.
//! Reconstruction (re-parse + re-index + re-embed) is the responsibility of the caller
//! (`gradatum-vault::drift_orchestrator`).
//!
//! Detection of "untracked" files (present on disk, absent from `file_checksums`)
//! is also the caller's responsibility — this module only checks existing entries.
//!
//! ## OpenDAL data path
//!
//! `scan_phase_a` accepts a `&dyn gradatum_storage::Storage` instead of a `&Path vault_root`.
//! The `relative_path` values from `file_checksums` entries are already relative —
//! directly compatible with the Storage contract (relative paths).
//! `stat` provides the on-disk size; `read` provides bytes for hashing.

use std::path::PathBuf;

use sha2::Digest as _;

use gradatum_core::error::GradatumError;
// list_file_checksums is a pub(crate) inherent method on SqliteIndex — no trait needed.
use gradatum_storage::{Storage, StorageError};

use crate::SqliteIndex;

/// Result of a drift scan.
///
/// Holds file counters at each verification level and the list of files
/// missing on disk.
#[derive(Debug, Default, Clone)]
pub struct DriftScanResult {
    /// Files whose size + prefix-4 KB hash match (likely unchanged).
    /// Do not require a full hash.
    pub level2_prefix_match: u64,

    /// Files whose full hash matches after size or prefix divergence.
    /// May indicate a cosmetic change (mtime only, padding, etc.).
    pub level3_full_hash_match: u64,

    /// Files whose full hash differs — drift confirmed, reconstruction required.
    pub level3_full_hash_mismatch: u64,

    /// Paths of files absent on disk (note referenced but file deleted).
    pub missing: Vec<PathBuf>,
}

/// Three-level drift scan: strict size → prefix-4 KB → full SHA-256.
///
/// Loads all `file_checksums` entries from `index`, then verifies each file
/// via `storage` (relative paths).
///
/// ## OpenDAL data path
///
/// `storage` is rooted at `vault_root`. The `relative_path` values from
/// `file_checksums` entries are directly usable as relative Storage paths.
/// - `stat(relative_path)` → on-disk size (equivalent to `fs::metadata`)
/// - `read(relative_path)` → full bytes (equivalent to `fs::read`)
///
/// ## Errors
///
/// Returns `GradatumError::Storage` if reading checksums fails.
/// Returns `GradatumError::Storage` if reading an existing file fails
/// (permissions, filesystem error). Missing files are collected in
/// `DriftScanResult::missing`, not reported as errors.
#[must_use = "DriftScanResult contient les informations de drift — ne pas ignorer"]
pub async fn scan_phase_a(
    storage: &dyn Storage,
    index: &SqliteIndex,
) -> Result<DriftScanResult, GradatumError> {
    let entries = index.list_file_checksums().await?;
    let mut result = DriftScanResult::default();

    for entry in &entries {
        let rel = &entry.relative_path;

        // Niveau 0 — existence : stat NotFound → fichier manquant.
        let meta = match storage.stat(rel).await {
            Ok(m) => m,
            Err(StorageError::NotFound(_)) => {
                // Fichier référencé dans file_checksums mais absent sur disque.
                // On conserve un PathBuf pour compatibilité avec DriftScanResult::missing.
                result.missing.push(PathBuf::from(rel));
                continue;
            }
            Err(e) => {
                return Err(GradatumError::Storage(format!(
                    "stat drift entry '{}': {e}",
                    rel
                )));
            }
        };

        let on_disk_size = meta.size;

        if on_disk_size == entry.expected_size {
            // Niveau 2 : prefix-4KB hash. ⚠️ `Storage::read` lit le fichier ENTIER —
            // seul le HASH porte sur les 4096 premiers bytes, pas la lecture. Le gain
            // du niveau 2 est donc le coût du hash, pas celui de l'I/O.
            let bytes = storage
                .read(rel)
                .await
                .map_err(|e| GradatumError::Storage(format!("read drift entry '{}': {e}", rel)))?;
            let prefix = compute_prefix_4kb_bytes(&bytes);
            if prefix == entry.expected_hash_prefix_4kb {
                // Fichier probablement inchangé — court-circuit
                result.level2_prefix_match += 1;
                continue;
            }
            // Prefix diffère malgré size identique → niveau 3 full hash (bytes déjà en mémoire)
            let full = compute_full_sha256_bytes(&bytes);
            if full == entry.expected_hash {
                result.level3_full_hash_match += 1;
            } else {
                result.level3_full_hash_mismatch += 1;
            }
        } else {
            // Size diffère → niveau 3 full hash
            let bytes = storage
                .read(rel)
                .await
                .map_err(|e| GradatumError::Storage(format!("read drift entry '{}': {e}", rel)))?;
            let full = compute_full_sha256_bytes(&bytes);
            if full == entry.expected_hash {
                result.level3_full_hash_match += 1;
            } else {
                result.level3_full_hash_mismatch += 1;
            }
        }
    }

    Ok(result)
}

/// SHA-256 hash of the first 4 KB of `bytes`.
///
/// Operates in memory (bytes already loaded by `storage.read()`).
/// Consumes at most `min(bytes.len(), 4096)` bytes.
fn compute_prefix_4kb_bytes(bytes: &[u8]) -> [u8; 32] {
    let prefix_len = bytes.len().min(4096);
    sha2::Sha256::digest(&bytes[..prefix_len]).into()
}

/// Full SHA-256 hash of `bytes`.
///
/// Suitable for Markdown notes typically under 1 MB.
/// Future evolution: streaming hash for large notes.
fn compute_full_sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_4kb_bytes_short_content() {
        // Vérifie que compute_prefix_4kb_bytes ne panique pas sur contenu < 4KB
        let hash = compute_prefix_4kb_bytes(b"hello");
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn full_sha256_bytes_empty() {
        let hash = compute_full_sha256_bytes(b"");
        // sha256("") = e3b0c44298fc1c149afb...
        assert_eq!(
            hash[0], 0xe3,
            "sha256 d'un contenu vide doit commencer par 0xe3"
        );
    }

    #[test]
    fn prefix_4kb_bytes_truncates_at_4096() {
        // Contenu de 5000 bytes → prefix = hash des 4096 premiers seulement
        let data = vec![0xABu8; 5000];
        let prefix = compute_prefix_4kb_bytes(&data);
        let full = compute_full_sha256_bytes(&data);
        // Les deux hashes doivent différer (contenus tronqués vs complets)
        assert_ne!(
            prefix, full,
            "prefix 4KB et full hash doivent différer pour >4KB"
        );
    }
}
