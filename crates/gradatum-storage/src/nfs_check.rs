//! NFS rejection guard.
//!
//! ## Behaviour
//!
//! - **Linux**: calls `statfs(2)` on the given path (or its parent if the path does not yet exist).
//!   If `f_type == NFS_SUPER_MAGIC (0x6969)`, returns `Err(StorageError::Core(VaultOnNfs))`.
//! - **Non-Linux**: logs a `warn` and returns `Ok(())`.
//!   Rationale: the target deployment is Linux-only. The NFS check relies on
//!   Linux-specific behaviour (`statfs` is not standardised by POSIX and
//!   `NFS_SUPER_MAGIC` is not portable).
//!
//! ## `NFS_SUPER_MAGIC` constant
//!
//! Canonical value: `0x6969` (see `linux/magic.h`, `statfs(2)` man page).
//! `nix` does not expose this constant publicly in `nix::sys::statfs` (verified ≤0.31) —
//! the literal is used directly to avoid a dependency on a private `nix` API.

use std::path::Path;

use crate::error::StorageError;

/// `NFS_SUPER_MAGIC` as defined in `linux/magic.h`.
///
/// Value: `0x6969` — returned in `statfs.f_type` for NFS mounts.
/// See: `man 2 statfs`, Linux kernel >= 2.4.
/// Note: `nix` ≥0.30 still does not expose this constant publicly — the literal is retained.
#[cfg(target_os = "linux")]
const NFS_SUPER_MAGIC: i64 = 0x6969_i64;

/// Verifies that `path` resides on a local filesystem (not NFS).
///
/// Called by `FileStorage::new()` before constructing the OpenDAL `Operator`.
/// Enforces the invariant that the vault root must not reside on NFS.
///
/// ## Path strategy
///
/// If `path` does not yet exist (new vault initialisation), the function walks
/// up the ancestor chain until it finds a path that exists, then calls `statfs`
/// on that ancestor. This handles deep paths where several intermediate directories
/// have not been created yet (e.g. `new_vault/data/blobs` — none of the three
/// components exist yet). If no ancestor exists (should not occur on a reachable
/// filesystem), the original path is used and `statfs` will return an error.
///
/// ## Non-Linux
///
/// On macOS / Windows the check is skipped with a warning.
/// `NFS_SUPER_MAGIC` is not defined on those platforms.
///
/// # Errors
///
/// - `StorageError::Io` if `statfs` fails (permission denied, invalid path).
/// - `StorageError::Core(GradatumError::VaultOnNfs { path })` if NFS is detected.
pub fn ensure_local_filesystem(path: &Path) -> Result<(), StorageError> {
    #[cfg(target_os = "linux")]
    {
        use gradatum_core::error::GradatumError;
        use nix::sys::statfs::statfs;

        // Walk up the ancestor chain to find the first existing path.
        // Rationale: the vault directory tree may not yet exist (new vault init),
        // and multiple levels may be absent (e.g. `vault/data/blobs`).
        // We need to `statfs` something that actually exists.
        let check_target = {
            let mut candidate = path.to_path_buf();
            while !candidate.exists() {
                match candidate.parent() {
                    Some(parent) if parent != candidate => {
                        candidate = parent.to_path_buf();
                    }
                    // Root or no parent — use the original path and let statfs fail.
                    _ => {
                        candidate = path.to_path_buf();
                        break;
                    }
                }
            }
            candidate
        };

        let st = statfs(&check_target)
            .map_err(|e| StorageError::Io(format!("statfs({check_target:?}): {e}")))?;

        // `filesystem_type()` returns `FsType(c_long)` — extract the raw value.
        let f_type = st.filesystem_type().0 as i64;

        if f_type == NFS_SUPER_MAGIC {
            return Err(StorageError::Core(GradatumError::VaultOnNfs {
                path: path.to_path_buf(),
            }));
        }

        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Non-Linux: NFS check not implemented — explicit log for traceability.
        tracing::warn!(
            path = %path.display(),
            "ensure_local_filesystem: plateforme non-Linux, NFS check ignoré"
        );
        Ok(())
    }
}
