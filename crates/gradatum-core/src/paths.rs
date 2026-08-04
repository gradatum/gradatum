//! Single source of truth for the canonical paths of the Gradatum on-disk layout.
//!
//! Every path derived from `storage.root` or from a `vault/` directory MUST go through
//! these helpers. Hand-written `root.join(...)` expressions in the server, the worker,
//! the vault or the admin CLI are forbidden — this module is the only place where the
//! on-disk layout is spelled out.
//!
//! ## Layout
//!
//! ```text
//! {storage_root}/                    ← argument of vault_index_path / queue_db_path
//!   vault/                           ← argument of vault_dir_index_path
//!     .gradatum/
//!       index.db          ← vault_index_path(storage_root)
//!                            vault_dir_index_path(storage_root/vault)
//!   db/
//!     queue.sqlite        ← queue_db_path(storage_root)
//! ```
//!
//! ## Which helper to call
//!
//! | Context                                        | Helper                            |
//! |------------------------------------------------|-----------------------------------|
//! | server, worker, admin holding `storage.root`   | `vault_index_path(root)`          |
//! | registry, worker index marker holding `vault/` | `vault_dir_index_path(vault_dir)` |
//! | server, worker, admin holding `storage.root`   | `queue_db_path(root)`             |
//!
//! ## Invariants pinned by the golden tests
//!
//! `vault_index_path(Path::new("/var/lib/gradatum"))
//!   == PathBuf::from("/var/lib/gradatum/vault/.gradatum/index.db")`
//!
//! `vault_dir_index_path(Path::new("/var/lib/gradatum/vault"))
//!   == PathBuf::from("/var/lib/gradatum/vault/.gradatum/index.db")`
//!
//! `queue_db_path(Path::new("/var/lib/gradatum"))
//!   == PathBuf::from("/var/lib/gradatum/db/queue.sqlite")`

use std::path::{Path, PathBuf};

/// Canonical path of the FTS5 SQLite index, derived from `storage.root`.
///
/// Use it wherever a component has to locate `index.db` from `storage.root`;
/// never write `root.join(...)` by hand.
///
/// Components that already hold the `vault/` directory (the registry, the worker
/// index marker) should call [`vault_dir_index_path`] instead.
///
/// # Example
///
/// ```
/// use std::path::{Path, PathBuf};
/// use gradatum_core::paths::vault_index_path;
///
/// let p = vault_index_path(Path::new("/var/lib/gradatum"));
/// assert_eq!(p, PathBuf::from("/var/lib/gradatum/vault/.gradatum/index.db"));
/// ```
#[must_use]
pub fn vault_index_path(root: &Path) -> PathBuf {
    root.join("vault").join(".gradatum").join("index.db")
}

/// Canonical path of the FTS5 SQLite index, derived from the `vault/` directory.
///
/// Use it when the caller already holds the `vault/` directory (for instance
/// `Vault::create`/`Vault::open` in `gradatum-vault`, or the worker index marker
/// fed by `--vault`). Equivalent to `vault_dir.join(".gradatum/index.db")`.
///
/// Never write `.join(".gradatum").join("index.db")` by hand — call this helper.
///
/// # Example
///
/// ```
/// use std::path::{Path, PathBuf};
/// use gradatum_core::paths::vault_dir_index_path;
///
/// let p = vault_dir_index_path(Path::new("/var/lib/gradatum/vault"));
/// assert_eq!(p, PathBuf::from("/var/lib/gradatum/vault/.gradatum/index.db"));
/// ```
#[must_use]
pub fn vault_dir_index_path(vault_dir: &Path) -> PathBuf {
    vault_dir.join(".gradatum").join("index.db")
}

/// Canonical path of the SQLite job queue.
///
/// Use it wherever a component has to locate `queue.sqlite` from `storage.root`;
/// never write `root.join(...)` by hand.
///
/// # Example
///
/// ```
/// use std::path::{Path, PathBuf};
/// use gradatum_core::paths::queue_db_path;
///
/// let p = queue_db_path(Path::new("/var/lib/gradatum"));
/// assert_eq!(p, PathBuf::from("/var/lib/gradatum/db/queue.sqlite"));
/// ```
#[must_use]
pub fn queue_db_path(root: &Path) -> PathBuf {
    root.join("db").join("queue.sqlite")
}

/// Canonical configuration directory, derived from `storage.root`.
///
/// It holds, among others, `jwt-signing-key.secret` (the JWT signing key),
/// `admin.bearer.txt`, `bearer.toml` and `server.toml`.
///
/// The server and `gradatum-admin` MUST both derive that directory through this
/// helper: if they disagree, the CLI signs tokens with a key the server does not
/// hold, and the server answers `401` on an operator path that the documentation
/// says should work.
///
/// # Example
///
/// ```
/// use std::path::{Path, PathBuf};
/// use gradatum_core::paths::config_dir;
///
/// let p = config_dir(Path::new("/var/lib/gradatum"));
/// assert_eq!(p, PathBuf::from("/var/lib/gradatum/config"));
/// ```
#[must_use]
pub fn config_dir(root: &Path) -> PathBuf {
    root.join("config")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Test golden byte-identique — preuve de l'invariant SSOT.
    ///
    /// Si ce test échoue, le layout a changé — mettre à jour TOUS les consommateurs
    /// (server/main.rs, worker/main.rs, admin/init.rs, server/config.rs,
    ///  vault/registry.rs, admin/backfill_embeddings.rs, etc.).
    #[test]
    fn vault_index_path_golden() {
        assert_eq!(
            vault_index_path(Path::new("/var/lib/gradatum")),
            PathBuf::from("/var/lib/gradatum/vault/.gradatum/index.db"),
            "invariant layout index.db — toute dérive = data-loss potentiel"
        );
    }

    /// Test golden — vault_dir_index_path == vault_index_path / même cible, autre point d'entrée.
    #[test]
    fn vault_dir_index_path_golden() {
        assert_eq!(
            vault_dir_index_path(Path::new("/var/lib/gradatum/vault")),
            PathBuf::from("/var/lib/gradatum/vault/.gradatum/index.db"),
            "invariant layout index.db via vault_dir — doit correspondre à vault_index_path(storage_root)"
        );
    }

    /// Cohérence croisée : vault_index_path(root) == vault_dir_index_path(root/vault).
    #[test]
    fn vault_index_path_equals_vault_dir_index_path() {
        let storage_root = Path::new("/var/lib/gradatum");
        assert_eq!(
            vault_index_path(storage_root),
            vault_dir_index_path(&storage_root.join("vault")),
            "les deux helpers doivent produire le même chemin depuis des points d'entrée distincts"
        );
    }

    /// Test golden byte-identique — preuve de l'invariant SSOT queue.
    #[test]
    fn queue_db_path_golden() {
        assert_eq!(
            queue_db_path(Path::new("/var/lib/gradatum")),
            PathBuf::from("/var/lib/gradatum/db/queue.sqlite"),
            "invariant layout queue.sqlite — toute dérive = perte jobs"
        );
    }

    /// Vérifie que les helpers sont composables avec des roots arbitraires.
    #[test]
    fn vault_index_path_arbitrary_root() {
        assert_eq!(
            vault_index_path(Path::new("/tmp/test-vault")),
            PathBuf::from("/tmp/test-vault/vault/.gradatum/index.db"),
        );
    }

    #[test]
    fn vault_dir_index_path_arbitrary_root() {
        assert_eq!(
            vault_dir_index_path(Path::new("/tmp/test-vault/vault")),
            PathBuf::from("/tmp/test-vault/vault/.gradatum/index.db"),
        );
    }

    #[test]
    fn queue_db_path_arbitrary_root() {
        assert_eq!(
            queue_db_path(Path::new("/tmp/test-vault")),
            PathBuf::from("/tmp/test-vault/db/queue.sqlite"),
        );
    }

    /// Test golden — invariant du répertoire de config (clé JWT partagée server/admin).
    ///
    /// Si ce test échoue, la clé de signature JWT change de place : le serveur et
    /// `gradatum-admin token issue` se désynchronisent et tous les jetons émis par
    /// la CLI sont rejetés en 401.
    #[test]
    fn config_dir_golden() {
        assert_eq!(
            config_dir(Path::new("/var/lib/gradatum")),
            PathBuf::from("/var/lib/gradatum/config"),
            "invariant layout config/ — toute dérive = split-brain clé JWT"
        );
    }
}
