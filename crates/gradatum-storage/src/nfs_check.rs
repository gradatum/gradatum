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
/// If `path` does not yet exist (new vault initialisation), the immediate parent is checked.
/// This allows rejecting a vault planned on NFS before it is created.
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

        // If the path does not yet exist, check the parent.
        // Rationale: a new vault may point to a directory not yet created.
        // If there is no parent (filesystem root `/`), check `/` itself.
        let check_target = if path.exists() {
            path.to_path_buf()
        } else {
            path.parent().unwrap_or(path).to_path_buf()
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
