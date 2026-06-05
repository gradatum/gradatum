//! Guard de rejet NFS — caveat C11.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §0.3 C11.
//!
//! ## Comportement
//!
//! - **Linux** : appel `statfs(2)` sur le chemin fourni (ou son parent si inexistant).
//!   Si `f_type == NFS_SUPER_MAGIC (0x6969)`, retourne `Err(StorageError::Core(VaultOnNfs))`.
//! - **Non-Linux** : log `warn` + retour `Ok(())`.
//!   Justification : Phase 1 = développement local Linux uniquement. Le check NFS est
//!   spécifique à Linux (statfs POSIX non standardisé + NFS_SUPER_MAGIC non portatif).
//!
//! ## Constante NFS_SUPER_MAGIC
//!
//! Valeur canonique : `0x6969` (cf. `linux/magic.h`, `statfs(2)` man page).
//! `nix` n'expose pas cette constante publiquement dans `nix::sys::statfs` (vérifié ≤0.31) —
//! on utilise le littéral directement (évite une dépendance sur l'API privée de nix).

use std::path::Path;

use crate::error::StorageError;

/// `NFS_SUPER_MAGIC` tel que défini dans `linux/magic.h`.
///
/// Valeur : `0x6969` — retournée dans `statfs.f_type` pour les montages NFS.
/// Ref : `man 2 statfs`, noyau Linux >= 2.4.
/// Note : nix ≥0.30 n'expose toujours pas cette constante publiquement — littéral conservé.
#[cfg(target_os = "linux")]
const NFS_SUPER_MAGIC: i64 = 0x6969_i64;

/// Vérifie que `path` réside sur un filesystem local (non NFS).
///
/// Appelé par `FileStorage::new()` avant la construction de l'`Operator` OpenDAL.
/// Implémente le caveat C11 : le vault root NE PEUT PAS être sur NFS.
///
/// ## Stratégie de chemin
///
/// Si `path` n'existe pas encore (init d'un nouveau vault), on vérifie le parent immédiat.
/// Cela permet de rejeter un vault planifié sur NFS avant même sa création.
///
/// ## Non-Linux
///
/// Sur macOS / Windows (environnements de dev uniquement en Phase 1), le check est skippé
/// avec un warning. NFS_SUPER_MAGIC n'est pas défini sur ces plateformes.
///
/// # Erreurs
///
/// - `StorageError::Io` si `statfs` échoue (permission, chemin invalide).
/// - `StorageError::Core(GradatumError::VaultOnNfs { path })` si NFS détecté.
pub fn ensure_local_filesystem(path: &Path) -> Result<(), StorageError> {
    #[cfg(target_os = "linux")]
    {
        use gradatum_core::error::GradatumError;
        use nix::sys::statfs::statfs;

        // Si le chemin n'existe pas encore, vérifier le parent.
        // Pourquoi : un nouveau vault peut pointer vers un répertoire non encore créé.
        // Pourquoi parent() ou path : si pas de parent (racine `/`), vérifier `/` lui-même.
        let check_target = if path.exists() {
            path.to_path_buf()
        } else {
            path.parent().unwrap_or(path).to_path_buf()
        };

        let st = statfs(&check_target)
            .map_err(|e| StorageError::Io(format!("statfs({check_target:?}): {e}")))?;

        // `filesystem_type()` retourne `FsType(c_long)` — on extrait la valeur brute.
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
        // Non-Linux : Phase 1 = plateformes de dev uniquement.
        // Note: RFC-0002 promotes Windows to secondary tier post-v0.1.0-alpha.
        // See docs/RFC/RFC-0002-cross-platform-support.md.
        // Le check NFS n'est pas implémenté — log explicite pour traçabilité.
        tracing::warn!(
            path = %path.display(),
            "ensure_local_filesystem: plateforme non-Linux, NFS check ignoré (Phase 1 dev-only)"
        );
        Ok(())
    }
}
