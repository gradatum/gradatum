//! SecretsProvider — abstraction DI pour les secrets applicatifs (F-13).
//!
//! Objectifs :
//! - Accès explicite aux secrets via [`SecretsProvider::get`] (auditabilité).
//! - Contenu jamais exposé dans les logs ou via `Debug` (masquage `SecretBytes`).
//! - Zeroize-on-drop garanti via [`secrecy::SecretBox`].
//! - Impls : [`EnvSecretsProvider`] (lecture env) + [`FileSecretsProvider`] (lecture fichier).
//!
//! ## Convention de nommage
//!
//! La clé `"jwt-signing-key"` correspond à :
//! - Env : `GRADATUM_SECRET_JWT_SIGNING_KEY`
//! - File : `<base_dir>/jwt-signing-key.secret`
//!
//! ## Sécurité
//!
//! - `SecretBytes` : [`Debug`] masqué, zeroize-on-drop via [`secrecy::SecretBox`].
//!   Le contenu n'est accessible que via [`secrecy::ExposeSecret::expose_secret`].
//! - [`FileSecretsProvider`] : refuse de lire si les permissions sont trop ouvertes
//!   (mode & 0o077 != 0 → [`SecretsError::Permissions`]).
//! - Écriture atomique via fichier temporaire + rename + chmod 600.
//!   **Le chmod 600 est appliqué AVANT le rename** : aucune fenêtre world-readable.

use std::env;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use secrecy::SecretBox;

/// Bytes secrets zeroize-on-drop.
///
/// Le contenu brut est accessible via [`secrecy::ExposeSecret::expose_secret`].
/// [`Debug`] affiche `"<secret>"` — jamais le contenu.
///
/// ```rust,no_run
/// use secrecy::ExposeSecret;
/// use gradatum_core::secrets::SecretBytes;
///
/// let sb = SecretBytes::from_vec(vec![1, 2, 3]);
/// // Debug masqué :
/// assert_eq!(format!("{:?}", sb), "SecretBytes(<secret>)");
/// // Accès explicite :
/// assert_eq!(sb.expose_secret(), &[1u8, 2, 3]);
/// ```
pub struct SecretBytes(SecretBox<[u8]>);

impl SecretBytes {
    /// Construit un `SecretBytes` depuis un vecteur d'octets.
    ///
    /// Les octets seront zéroïsés en mémoire à la destruction.
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self(SecretBox::from(bytes.into_boxed_slice()))
    }
}

impl secrecy::ExposeSecret<[u8]> for SecretBytes {
    fn expose_secret(&self) -> &[u8] {
        secrecy::ExposeSecret::expose_secret(&self.0)
    }
}

/// `Debug` masqué : jamais le contenu.
impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBytes(<secret>)")
    }
}

/// Erreurs du système de secrets.
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    /// Clé absente (fichier ou variable d'env inexistant).
    #[error("secret '{key}' introuvable")]
    NotFound {
        /// Nom de la clé demandée.
        key: String,
    },

    /// Erreur d'entrée/sortie lors de la lecture du secret.
    #[error("erreur I/O lors de la lecture du secret '{key}': {source}")]
    Io {
        /// Nom de la clé concernée.
        key: String,
        /// Erreur I/O sous-jacente.
        #[source]
        source: io::Error,
    },

    /// Permissions trop ouvertes sur le fichier secret (> 600).
    ///
    /// Un fichier secret avec group/world read/write/execute est refusé
    /// pour prévenir les fuites de secrets par l'accès concurrent ou l'historique.
    #[error(
        "permissions trop ouvertes sur le fichier secret '{key}' \
        (attendu: mode ≤ 0o600, trouvé: 0o{mode:04o})"
    )]
    Permissions {
        /// Nom de la clé concernée.
        key: String,
        /// Mode Unix constaté (octal).
        mode: u32,
    },

    /// Opération non supportée par ce provider (ex. rotate, list sur EnvSecretsProvider),
    /// ou clé invalide (contient `/`, `..`, ou est vide — V5 path-traversal guard).
    #[error("opération non supportée ou clé invalide pour ce SecretsProvider: {operation}")]
    Unsupported {
        /// Nom de l'opération ou description du problème.
        operation: String,
    },
}

/// Trait DI pour l'accès aux secrets applicatifs (F-13).
///
/// Chaque implémentation lit un secret par clé normalisée et retourne
/// un [`SecretBytes`] zeroize-on-drop dont le contenu est masqué dans les logs.
///
/// `Send + Sync` requis pour l'injection dans [`AppState`](crate::AppState).
pub trait SecretsProvider: Send + Sync {
    /// Lit la valeur d'un secret par sa clé normalisée.
    ///
    /// # Erreurs
    ///
    /// - [`SecretsError::NotFound`] : la clé n'existe pas dans ce provider.
    /// - [`SecretsError::Io`] : erreur d'accès au backend (fichier, réseau…).
    /// - [`SecretsError::Permissions`] : permissions trop ouvertes ([`FileSecretsProvider`]).
    /// - [`SecretsError::Unsupported`] : opération non disponible pour ce provider.
    fn get(&self, key: &str) -> Result<SecretBytes, SecretsError>;
}

// ── EnvSecretsProvider ────────────────────────────────────────────────────────

/// Provider lisant les secrets depuis les variables d'environnement.
///
/// Convention : la clé `"jwt-signing-key"` est cherchée dans la variable
/// `GRADATUM_SECRET_JWT_SIGNING_KEY` (tirets → underscores, majuscules).
///
/// La valeur de la variable est interprétée comme des bytes UTF-8 bruts.
/// Pour une clé Ed25519 encodée en hexadécimal, utiliser [`FileSecretsProvider`]
/// qui lit les bytes binaires directement depuis un fichier.
pub struct EnvSecretsProvider;

impl EnvSecretsProvider {
    /// Construit le nom de la variable d'environnement pour une clé donnée.
    ///
    /// `"jwt-signing-key"` → `"GRADATUM_SECRET_JWT_SIGNING_KEY"`.
    pub(crate) fn env_var_name(key: &str) -> String {
        let upper = key.to_uppercase().replace('-', "_");
        format!("GRADATUM_SECRET_{upper}")
    }
}

impl SecretsProvider for EnvSecretsProvider {
    fn get(&self, key: &str) -> Result<SecretBytes, SecretsError> {
        let var_name = Self::env_var_name(key);
        match env::var(&var_name) {
            Ok(val) => Ok(SecretBytes::from_vec(val.into_bytes())),
            Err(env::VarError::NotPresent) => Err(SecretsError::NotFound {
                key: key.to_string(),
            }),
            Err(env::VarError::NotUnicode(_)) => Err(SecretsError::Io {
                key: key.to_string(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "valeur de variable d'env non-UTF-8",
                ),
            }),
        }
    }
}

// ── FileSecretsProvider ───────────────────────────────────────────────────────

/// Provider lisant les secrets depuis des fichiers dans un répertoire de base.
///
/// Convention : la clé `"jwt-signing-key"` est lue depuis `<base_dir>/jwt-signing-key.secret`.
///
/// ## Sécurité
///
/// - Refuse de lire un fichier dont le mode Unix est trop ouvert
///   (mode & 0o077 != 0 → [`SecretsError::Permissions`]).
/// - Écriture atomique via [`FileSecretsProvider::write_atomic`] :
///   tmp + chmod 600 + rename (aucune fenêtre world-readable).
pub struct FileSecretsProvider {
    /// Répertoire de base contenant les fichiers `.secret`.
    base_dir: PathBuf,
}

impl FileSecretsProvider {
    /// Crée un nouveau `FileSecretsProvider` avec le répertoire de base donné.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Valide qu'une clé est sûre à utiliser comme composant de chemin (V5 — path-traversal guard).
    ///
    /// Rejette : clé vide, clé contenant `/`, clé contenant `..` (composant isolé ou en sous-chemin).
    /// Une clé valide est un identifiant plat de type `"jwt-signing-key"` ou `"ma-cle-123"`.
    ///
    /// # Erreurs
    ///
    /// Retourne `SecretsError::Unsupported` si la clé est vide, contient `/` ou `..`.
    pub(crate) fn validate_key(key: &str) -> Result<(), SecretsError> {
        if key.is_empty() {
            return Err(SecretsError::Unsupported {
                operation: "clé vide — la clé ne peut pas être une chaîne vide".to_string(),
            });
        }
        // Refuser tout slash : la clé doit être un nom de fichier plat, pas un chemin.
        if key.contains('/') {
            return Err(SecretsError::Unsupported {
                operation: format!(
                    "clé '{key}' invalide — les slashes '/' sont interdits (path traversal)"
                ),
            });
        }
        // Refuser les composants de remontée de répertoire.
        if key.split('/').any(|c| c == "..") || key == ".." {
            return Err(SecretsError::Unsupported {
                operation: format!("clé '{key}' invalide — '..' est interdit (path traversal)"),
            });
        }
        Ok(())
    }

    /// Chemin du fichier secret pour une clé donnée.
    ///
    /// `"jwt-signing-key"` → `<base_dir>/jwt-signing-key.secret`.
    pub(crate) fn file_path(&self, key: &str) -> PathBuf {
        self.base_dir.join(format!("{key}.secret"))
    }

    /// Vérifie que les permissions du fichier sont ≤ 0o600.
    ///
    /// Retourne `SecretsError::Permissions` si le bit group ou world est positionné.
    fn check_permissions(&self, key: &str, path: &Path) -> Result<(), SecretsError> {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|e| SecretsError::Io {
            key: key.to_string(),
            source: e,
        })?;
        let mode = meta.permissions().mode();
        // Accepter uniquement owner read/write — group et world doivent être 0.
        if mode & 0o077 != 0 {
            return Err(SecretsError::Permissions {
                key: key.to_string(),
                mode,
            });
        }
        Ok(())
    }

    /// Vérifie que le répertoire `base_dir` n'est pas writable par group ou world (V4).
    ///
    /// Un répertoire de secrets avec les bits `group-write` ou `world-write` ouverts
    /// permet à un tiers de créer des fichiers `.secret` parasites ou de remplacer
    /// des secrets existants via rename. On refuse de lire dans ce contexte.
    ///
    /// Retourne `SecretsError::Permissions` si `mode & 0o022 != 0`.
    fn check_base_dir_permissions(&self, key: &str) -> Result<(), SecretsError> {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&self.base_dir).map_err(|e| SecretsError::Io {
            key: key.to_string(),
            source: e,
        })?;
        let mode = meta.permissions().mode();
        // Bits group-write (0o020) et world-write (0o002) : suffisants pour créer/remplacer.
        if mode & 0o022 != 0 {
            return Err(SecretsError::Permissions {
                key: key.to_string(),
                mode,
            });
        }
        Ok(())
    }

    /// Écrit des bytes dans un fichier de manière atomique.
    ///
    /// Séquence (V1 — mode 0o600 à la création, AVANT tout write) :
    /// 1. Créer le répertoire parent si absent, puis le restreindre à 0o700 (V2).
    /// 2. Ouvrir `<path>.secret.tmp` avec `O_CREAT|O_EXCL` et mode 0o600 atomique.
    ///    Si le `.tmp` zombie existe (crash précédent) → le supprimer best-effort,
    ///    puis réessayer. Si le second essai échoue → remonter l'erreur.
    /// 3. Écrire les bytes dans le fichier déjà à 0o600 (aucune fenêtre world-readable).
    /// 4. `rename(<path>.tmp, <path>)` — atomique sur le même système de fichiers.
    ///
    /// # Erreurs
    ///
    /// Retourne [`SecretsError::Io`] si l'écriture ou le rename échoue.
    /// Retourne [`SecretsError::Unsupported`] si la clé est invalide (V5).
    pub fn write_atomic(&self, key: &str, bytes: &[u8]) -> Result<(), SecretsError> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        // V5 : valider la clé avant tout accès filesystem.
        Self::validate_key(key)?;

        let final_path = self.file_path(key);
        let tmp_path = self.base_dir.join(format!("{key}.secret.tmp"));

        // Étape 1 : créer le répertoire parent si absent.
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SecretsError::Io {
                key: key.to_string(),
                source: e,
            })?;
            // V2 : restreindre le répertoire à 0o700 (owner only) après création.
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| SecretsError::Io {
                    key: key.to_string(),
                    source: e,
                },
            )?;
        }

        // Étape 2 : ouvrir le fichier tmp avec O_CREAT|O_EXCL et mode 0o600 ATOMIQUE (V1).
        // Mode 0o600 est positionné à l'O_CREAT → aucune fenêtre world-readable,
        // même sous umask permissif.
        let open_tmp = |path: &PathBuf| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true) // O_EXCL : échoue si le .tmp zombie existe déjà
                .mode(0o600)
                .open(path)
                .map_err(|e| SecretsError::Io {
                    key: key.to_string(),
                    source: e,
                })
        };

        let mut f = match open_tmp(&tmp_path) {
            Ok(f) => f,
            Err(SecretsError::Io { ref source, .. })
                if source.kind() == io::ErrorKind::AlreadyExists =>
            {
                // Fichier .tmp zombie (crash précédent) — le supprimer best-effort, puis réessayer.
                let _ = std::fs::remove_file(&tmp_path);
                open_tmp(&tmp_path)?
            }
            Err(e) => return Err(e),
        };

        // Étape 3 : écriture des bytes (fichier déjà à 0o600 — aucune fenêtre).
        f.write_all(bytes).map_err(|e| SecretsError::Io {
            key: key.to_string(),
            source: e,
        })?;
        // Flush explicite avant rename pour garantir la durabilité.
        f.flush().map_err(|e| SecretsError::Io {
            key: key.to_string(),
            source: e,
        })?;
        // Drop explicite du File handle avant le rename (libère le fd).
        drop(f);

        // Étape 4 : rename atomique tmp → final.
        std::fs::rename(&tmp_path, &final_path).map_err(|e| SecretsError::Io {
            key: key.to_string(),
            source: e,
        })?;

        Ok(())
    }
}

impl SecretsProvider for FileSecretsProvider {
    fn get(&self, key: &str) -> Result<SecretBytes, SecretsError> {
        // V5 : valider la clé avant tout accès filesystem.
        Self::validate_key(key)?;

        let path = self.file_path(key);

        // Fichier absent → NotFound (avant tout check de permissions).
        // Cas normal au premier boot : write_atomic n'a pas encore été appelé,
        // le répertoire peut être en 0o755 — ce sera corrigé par write_atomic.
        if !path.exists() {
            return Err(SecretsError::NotFound {
                key: key.to_string(),
            });
        }

        // V4 : vérifier que le répertoire base_dir n'est pas writable par group/world.
        // Applicable seulement si le fichier existe : un répertoire writable permet à
        // un tiers de remplacer/créer des fichiers .secret via rename ou création directe.
        self.check_base_dir_permissions(key)?;

        // Vérification des permissions du fichier avant lecture.
        self.check_permissions(key, &path)?;

        // Lecture du contenu.
        let bytes = std::fs::read(&path).map_err(|e| SecretsError::Io {
            key: key.to_string(),
            source: e,
        })?;

        Ok(SecretBytes::from_vec(bytes))
    }
}

// ── Tests unitaires ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use secrecy::ExposeSecret;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Crée un TempDir et le restreint immédiatement à 0o700 (owner only).
    ///
    /// Requis pour tous les tests qui exercent `get()` (V4 — check répertoire writable).
    /// Les répertoires créés par TempDir ont typiquement le mode 0o700 ou 0o755
    /// selon l'umask du processus — on normalise à 0o700 pour la reproductibilité.
    fn tempdir_secure() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0o700 tempdir");
        dir
    }

    // ── SecretBytes ───────────────────────────────────────────────────────────

    #[test]
    fn secret_bytes_debug_masque_le_contenu() {
        let sb = SecretBytes::from_vec(vec![0xde, 0xad, 0xbe, 0xef]);
        let debug_repr = format!("{:?}", sb);
        // Le contenu ne doit JAMAIS apparaître dans le Debug.
        assert!(
            !debug_repr.contains("de"),
            "Debug ne doit pas exposer les bytes"
        );
        assert!(
            !debug_repr.contains("ad"),
            "Debug ne doit pas exposer les bytes"
        );
        assert!(
            debug_repr.contains("<secret>"),
            "Debug doit afficher <secret>"
        );
    }

    #[test]
    fn secret_bytes_expose_secret_retourne_le_contenu() {
        let original = vec![1u8, 2, 3, 4, 5];
        let sb = SecretBytes::from_vec(original.clone());
        assert_eq!(sb.expose_secret(), original.as_slice());
    }

    // ── EnvSecretsProvider ────────────────────────────────────────────────────

    #[test]
    fn env_provider_nom_de_variable_canonique() {
        assert_eq!(
            EnvSecretsProvider::env_var_name("jwt-signing-key"),
            "GRADATUM_SECRET_JWT_SIGNING_KEY"
        );
        assert_eq!(
            EnvSecretsProvider::env_var_name("my-secret"),
            "GRADATUM_SECRET_MY_SECRET"
        );
    }

    #[test]
    fn env_provider_retourne_la_valeur_presente() {
        let key = "test-key-env-present";
        let var = EnvSecretsProvider::env_var_name(key);
        // Nettoyer avant le test pour éviter les interférences.
        std::env::remove_var(&var);
        std::env::set_var(&var, "valeur-test");

        let provider = EnvSecretsProvider;
        let result = provider.get(key).expect("doit réussir");
        assert_eq!(result.expose_secret(), b"valeur-test");

        // Nettoyage.
        std::env::remove_var(&var);
    }

    #[test]
    fn env_provider_retourne_not_found_si_absent() {
        let key = "test-key-env-absent-xyz";
        let var = EnvSecretsProvider::env_var_name(key);
        std::env::remove_var(&var);

        let provider = EnvSecretsProvider;
        let err = provider.get(key).expect_err("doit échouer");
        assert!(
            matches!(err, SecretsError::NotFound { .. }),
            "attendu NotFound, obtenu: {err:?}"
        );
    }

    // ── FileSecretsProvider ───────────────────────────────────────────────────

    #[test]
    fn file_provider_lit_un_fichier_existant_chmod_600() {
        // tempdir_secure : répertoire à 0o700 pour satisfaire la vérification V4.
        let dir = tempdir_secure();
        let provider = FileSecretsProvider::new(dir.path());

        let key = "ma-cle";
        let contenu = b"bytes-secrets-42";

        // Écriture directe avec chmod 600.
        let chemin = provider.file_path(key);
        std::fs::write(&chemin, contenu).expect("écriture");
        std::fs::set_permissions(&chemin, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600");

        let result = provider.get(key).expect("doit réussir");
        assert_eq!(result.expose_secret(), contenu);
    }

    #[test]
    fn file_provider_refuse_perms_ouvertes_644() {
        // tempdir_secure : répertoire à 0o700 — le refus doit venir du fichier (V1 check), pas du répertoire (V4).
        let dir = tempdir_secure();
        let provider = FileSecretsProvider::new(dir.path());

        let key = "cle-perms-ouvertes";
        let chemin = provider.file_path(key);
        std::fs::write(&chemin, b"secret").expect("écriture");
        std::fs::set_permissions(&chemin, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");

        let err = provider.get(key).expect_err("doit échouer");
        assert!(
            matches!(err, SecretsError::Permissions { .. }),
            "attendu Permissions (fichier 644), obtenu: {err:?}"
        );
    }

    #[test]
    fn file_provider_refuse_perms_ouvertes_640() {
        let dir = tempdir_secure();
        let provider = FileSecretsProvider::new(dir.path());

        let key = "cle-perms-640";
        let chemin = provider.file_path(key);
        std::fs::write(&chemin, b"secret").expect("écriture");
        std::fs::set_permissions(&chemin, std::fs::Permissions::from_mode(0o640))
            .expect("chmod 640");

        let err = provider.get(key).expect_err("doit échouer");
        assert!(
            matches!(err, SecretsError::Permissions { .. }),
            "attendu Permissions (fichier 640), obtenu: {err:?}"
        );
    }

    #[test]
    fn file_provider_retourne_not_found_si_absent() {
        let dir = tempdir_secure();
        let provider = FileSecretsProvider::new(dir.path());

        let err = provider.get("cle-inexistante").expect_err("doit échouer");
        assert!(
            matches!(err, SecretsError::NotFound { .. }),
            "attendu NotFound, obtenu: {err:?}"
        );
    }

    #[test]
    fn file_provider_write_atomic_cree_avec_chmod_600() {
        let dir = TempDir::new().expect("tempdir");
        let provider = FileSecretsProvider::new(dir.path());

        let key = "cle-atomique";
        let contenu = b"octet-secret-atomique";

        // write_atomic set lui-même les permissions du répertoire à 0o700.
        provider
            .write_atomic(key, contenu)
            .expect("écriture atomique doit réussir");

        // Vérifier que le fichier existe avec les bons bytes.
        let chemin = provider.file_path(key);
        assert!(
            chemin.exists(),
            "le fichier doit exister après write_atomic"
        );

        // Vérifier les permissions du fichier (0o600 strict — V1).
        let meta = std::fs::metadata(&chemin).expect("metadata");
        let mode = meta.permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "permissions fichier doivent être 0o600, trouvé: 0o{mode:04o}"
        );

        // Vérifier les permissions du répertoire (0o700 — V2).
        let dir_meta = std::fs::metadata(dir.path()).expect("metadata dir");
        let dir_mode = dir_meta.permissions().mode();
        assert_eq!(
            dir_mode & 0o777,
            0o700,
            "permissions répertoire doivent être 0o700, trouvé: 0o{dir_mode:04o}"
        );

        // Vérifier le contenu.
        let lu = std::fs::read(&chemin).expect("lecture");
        assert_eq!(lu, contenu);
    }

    #[test]
    fn file_provider_write_atomic_puis_get_reussit() {
        let dir = TempDir::new().expect("tempdir");
        let provider = FileSecretsProvider::new(dir.path());

        let key = "cle-roundtrip";
        let contenu = b"roundtrip-bytes";

        // write_atomic → répertoire à 0o700 → get() peut lire.
        provider.write_atomic(key, contenu).expect("écriture");
        let result = provider.get(key).expect("lecture après écriture");
        assert_eq!(result.expose_secret(), contenu);
    }

    #[test]
    fn file_provider_nom_de_fichier_canonique() {
        let provider = FileSecretsProvider::new("/var/lib/gradatum/secrets");
        assert_eq!(
            provider.file_path("jwt-signing-key"),
            std::path::PathBuf::from("/var/lib/gradatum/secrets/jwt-signing-key.secret")
        );
    }

    // ── V4 — check répertoire writable ───────────────────────────────────────

    #[test]
    fn file_provider_get_refuse_repertoire_world_writable() {
        // Répertoire 0o777 → group-write + world-write → V4 doit refuser.
        let dir = TempDir::new().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("chmod 777");
        let provider = FileSecretsProvider::new(dir.path());

        // Créer le fichier avec les bonnes permissions (0o600) — l'erreur vient du répertoire.
        let chemin = provider.file_path("cle-test");
        std::fs::write(&chemin, b"secret").expect("écriture");
        std::fs::set_permissions(&chemin, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600 fichier");

        let err = provider
            .get("cle-test")
            .expect_err("doit échouer (dir writable)");
        assert!(
            matches!(err, SecretsError::Permissions { .. }),
            "attendu Permissions (répertoire writable), obtenu: {err:?}"
        );
    }

    #[test]
    fn file_provider_get_refuse_repertoire_group_writable() {
        // Répertoire 0o750 → group-read seulement, pas group-write → OK.
        // Répertoire 0o770 → group-write → V4 refus.
        let dir = TempDir::new().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o770))
            .expect("chmod 770");
        let provider = FileSecretsProvider::new(dir.path());

        let chemin = provider.file_path("cle-grp-writable");
        std::fs::write(&chemin, b"secret").expect("écriture");
        std::fs::set_permissions(&chemin, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600 fichier");

        let err = provider
            .get("cle-grp-writable")
            .expect_err("doit échouer (dir group-writable)");
        assert!(
            matches!(err, SecretsError::Permissions { .. }),
            "attendu Permissions (répertoire group-writable), obtenu: {err:?}"
        );
    }

    // ── V5 — validate_key path traversal ─────────────────────────────────────

    #[test]
    fn validate_key_refuse_cle_vide() {
        let err = FileSecretsProvider::validate_key("").expect_err("doit refuser clé vide");
        assert!(matches!(err, SecretsError::Unsupported { .. }));
    }

    #[test]
    fn validate_key_refuse_slash() {
        let err =
            FileSecretsProvider::validate_key("../../etc/passwd").expect_err("doit refuser slash");
        assert!(matches!(err, SecretsError::Unsupported { .. }));
    }

    #[test]
    fn validate_key_refuse_slash_simple() {
        let err = FileSecretsProvider::validate_key("a/b").expect_err("doit refuser slash simple");
        assert!(matches!(err, SecretsError::Unsupported { .. }));
    }

    #[test]
    fn validate_key_refuse_double_point() {
        let err = FileSecretsProvider::validate_key("..").expect_err("doit refuser '..'");
        assert!(matches!(err, SecretsError::Unsupported { .. }));
    }

    #[test]
    fn validate_key_accepte_cles_valides() {
        assert!(FileSecretsProvider::validate_key("jwt-signing-key").is_ok());
        assert!(FileSecretsProvider::validate_key("ma-cle-123").is_ok());
        assert!(FileSecretsProvider::validate_key("api_key").is_ok());
        assert!(FileSecretsProvider::validate_key("secret").is_ok());
    }

    #[test]
    fn get_refuse_cle_avec_slash() {
        let dir = tempdir_secure();
        let provider = FileSecretsProvider::new(dir.path());
        let err = provider
            .get("../../etc/passwd")
            .expect_err("doit refuser path traversal via get()");
        assert!(
            matches!(err, SecretsError::Unsupported { .. }),
            "attendu Unsupported, obtenu: {err:?}"
        );
    }

    #[test]
    fn write_atomic_refuse_cle_avec_slash() {
        let dir = TempDir::new().expect("tempdir");
        let provider = FileSecretsProvider::new(dir.path());
        let err = provider
            .write_atomic("../../etc/cron.d/evil", b"payload")
            .expect_err("doit refuser path traversal via write_atomic()");
        assert!(
            matches!(err, SecretsError::Unsupported { .. }),
            "attendu Unsupported, obtenu: {err:?}"
        );
    }
}
