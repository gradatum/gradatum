//! SSOT helpers pour les chemins canoniques du layout Gradatum.
//!
//! Toute dérivation de chemin à partir de `storage.root` ou `vault_dir` DOIT passer
//! par ces helpers. Un `root.join(...)` manuel dans `main.rs`, worker, vault ou admin
//! est interdit — ce module est la source unique de vérité du layout disque.
//!
//! ## Layout
//!
//! ```text
//! {storage_root}/                    ← argument de vault_index_path / queue_db_path
//!   vault/                           ← argument de vault_dir_index_path
//!     .gradatum/
//!       index.db          ← vault_index_path(storage_root)
//!                            vault_dir_index_path(storage_root/vault)
//!   db/
//!     queue.sqlite        ← queue_db_path(storage_root)
//! ```
//!
//! ## Quelle fonction utiliser ?
//!
//! | Contexte                                     | Fonction                        |
//! |----------------------------------------------|---------------------------------|
//! | server, worker, admin avec `storage.root`    | `vault_index_path(root)`        |
//! | registry, worker index_marker avec `vault/`  | `vault_dir_index_path(vault_dir)` |
//! | server, worker, admin avec `storage.root`    | `queue_db_path(root)`           |
//!
//! ## Invariants garantis par les tests golden
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

/// Chemin canonique de l'index SQLite FTS5, depuis `storage.root`.
///
/// Doit être utilisé partout où un composant a besoin de localiser
/// `index.db` à partir de `storage.root`. Jamais de `root.join(...)` manuel.
///
/// Pour les composants qui reçoivent directement le répertoire `vault/`
/// (registry, index_marker worker), voir [`vault_dir_index_path`].
///
/// # Exemple
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

/// Chemin canonique de l'index SQLite FTS5, depuis le répertoire `vault/`.
///
/// À utiliser quand le contexte dispose du répertoire `vault/` directement
/// (ex: `Vault::create`/`Vault::open` dans `gradatum-vault`, ou l'index_marker
/// du worker qui reçoit `--vault`). Équivalent à `vault_dir.join(".gradatum/index.db")`.
///
/// Jamais de `.join(".gradatum").join("index.db")` manuel — utiliser ce helper.
///
/// # Exemple
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

/// Chemin canonique de la queue SQLite.
///
/// Doit être utilisé partout où un composant a besoin de localiser
/// `queue.sqlite` à partir de `storage.root`. Jamais de `root.join(...)` manuel.
///
/// # Exemple
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
}
