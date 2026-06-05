//! Chargement ou génération de la clé de signature JWT au boot de production.
//!
//! ## Logique load-or-generate
//!
//! 1. Si `<key_dir>/jwt-signing-key.secret` existe et a les permissions ≤ 0o600 :
//!    → charger les 32 bytes de seed, construire `JwtService`.
//! 2. Si le fichier est absent :
//!    → générer une nouvelle seed Ed25519 via `OsRng`,
//!    → écrire atomiquement (tmp + chmod 600 + rename),
//!    → `tracing::info!("clé JWT générée et persistée")` (JAMAIS la valeur).
//!    → construire `JwtService` avec la nouvelle clé.
//!
//! ## Sécurité
//!
//! - La seed ne transite que via [`secrecy::ExposeSecret`] — jamais en clair.
//! - `tracing::info!` n'affiche que le chemin du fichier, jamais le contenu.
//! - Écriture atomique garantie par [`FileSecretsProvider::write_atomic`] :
//!   chmod 600 appliqué AVANT le rename (aucune fenêtre world-readable).
//! - Si les permissions sont trop ouvertes, `load_or_generate_jwt_key` retourne `Err`.

use std::path::Path;

use anyhow::Context;
use gradatum_core::secrets::{FileSecretsProvider, SecretsError, SecretsProvider};

use gradatum_auth::jwt::JwtService;
use secrecy::ExposeSecret;

/// Clé normalisée pour la seed de signature JWT dans le `FileSecretsProvider`.
///
/// Chemin résolu : `<key_dir>/jwt-signing-key.secret`.
pub(crate) const JWT_SIGNING_KEY_NAME: &str = "jwt-signing-key";

/// Charge ou génère la clé de signature JWT Ed25519.
///
/// # Arguments
///
/// - `key_dir` : répertoire contenant (ou devant contenir) `jwt-signing-key.secret`.
///   En production : chemin dérivé du parent de `cfg.auth.jwt_private_key_path`
///   ou de `cfg.storage.root / "config"`.
/// - `kid` : identifiant de la clé (header `kid` dans les JWT émis).
/// - `audience` : audience exacte (ex. `"gradatum"`).
/// - `ttl_human_secs` : TTL tokens Human (R-A1).
/// - `ttl_service_secs` : TTL tokens Service (R-A1).
///
/// # Comportement
///
/// | État fichier | Résultat |
/// |---|---|
/// | Présent, perms ≤ 0o600 | Chargé, `JwtService` construit avec la clé existante |
/// | Absent | Généré, persisté atomiquement (chmod 600), `JwtService` construit |
/// | Présent, perms > 0o600 | `Err(SecretsError::Permissions)` |
/// | Présent, 32 bytes invalides | `Err` (bytes corrompus) |
///
/// # Erreurs
///
/// Retourne `anyhow::Error` en cas d'échec I/O, permissions trop ouvertes,
/// ou bytes corrompus (≠ 32 bytes).
///
/// # Sécurité
///
/// La seed Ed25519 n'est jamais loggée. Seul le chemin du fichier est tracé.
pub(crate) fn load_or_generate_jwt_key(
    key_dir: &Path,
    kid: String,
    audience: String,
    ttl_human_secs: u64,
    ttl_service_secs: u64,
) -> anyhow::Result<JwtService> {
    let provider = FileSecretsProvider::new(key_dir);

    let signing_bytes = match provider.get(JWT_SIGNING_KEY_NAME) {
        Ok(secret) => {
            // Fichier présent et permissions OK — charger la seed existante.
            tracing::info!(
                path = %key_dir.join(format!("{JWT_SIGNING_KEY_NAME}.secret")).display(),
                "clé JWT chargée depuis le fichier persistant"
            );
            secret
        }
        Err(SecretsError::NotFound { .. }) => {
            // Fichier absent — générer une nouvelle clé et la persister.
            let (seed, _signing_key) = JwtService::generate_signing_bytes();

            // Écriture atomique : tmp (mode 0o600 à l'O_CREAT) → rename.
            // `seed` est Zeroizing<[u8;32]> : effacée en mémoire dès ce bloc.
            provider
                .write_atomic(JWT_SIGNING_KEY_NAME, seed.as_ref())
                .with_context(|| {
                    format!(
                        "écriture de la clé JWT dans {}",
                        key_dir
                            .join(format!("{JWT_SIGNING_KEY_NAME}.secret"))
                            .display()
                    )
                })?;
            // `seed` droppée ici → Zeroizing::drop() écrase les 32 bytes.

            // V8 : WARN explicite — TOUS les tokens existants sont invalidés.
            tracing::warn!(
                path = %key_dir.join(format!("{JWT_SIGNING_KEY_NAME}.secret")).display(),
                "clé JWT générée et persistée — \
                TOUS LES TOKENS EXISTANTS SONT INVALIDÉS \
                (premier démarrage ou rotation manuelle du fichier .secret)"
            );

            // Relire depuis le FileSecretsProvider pour réutiliser le chemin canonique.
            // (La clé vient d'être écrite, pas de risque NotFound ici.)
            provider
                .get(JWT_SIGNING_KEY_NAME)
                .context("relecture de la clé JWT après génération")?
        }
        Err(e) => {
            // Permissions trop ouvertes ou I/O — propager l'erreur (fail-closed).
            return Err(e).context("lecture de la clé de signature JWT");
        }
    };

    // Construction du JwtService depuis les bytes de seed.
    // Jamais de log du contenu — seul le chemin est tracé ci-dessus.
    JwtService::from_signing_bytes(
        signing_bytes.expose_secret(),
        kid,
        audience,
        ttl_human_secs,
        ttl_service_secs,
    )
    .map_err(|e| anyhow::anyhow!("construction JwtService depuis seed : {e}"))
}

// ── Tests unitaires ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use gradatum_auth::jwt::TokenScope;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn params() -> (String, String, u64, u64) {
        (
            "test-kid".to_string(),
            "gradatum-test".to_string(),
            3600,
            86400,
        )
    }

    #[test]
    fn premier_boot_genere_le_fichier_chmod_600() {
        let dir = TempDir::new().expect("tempdir");
        let (kid, aud, ttl_h, ttl_s) = params();

        let jwt = load_or_generate_jwt_key(dir.path(), kid, aud, ttl_h, ttl_s)
            .expect("load_or_generate doit réussir");

        // Le fichier doit exister.
        let key_path = dir.path().join(format!("{JWT_SIGNING_KEY_NAME}.secret"));
        assert!(
            key_path.exists(),
            "fichier clé doit être créé au premier boot"
        );

        // Les permissions doivent être exactement 0o600.
        let meta = std::fs::metadata(&key_path).expect("metadata");
        let mode = meta.permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "permissions doivent être 0o600, trouvé: 0o{mode:04o}"
        );

        // Le JwtService doit pouvoir signer.
        let token = jwt
            .sign("sub-test", &["read".to_string()], TokenScope::Human, "main")
            .expect("signe doit réussir");
        assert!(!token.is_empty());
    }

    #[test]
    fn second_boot_charge_la_meme_cle_jwt_valide_apres_restart() {
        // Ce test prouve la correction du bug P0 :
        // Un JWT signé par le premier boot reste valide sur le second boot
        // (même base_dir → même seed → même clé → même vérification).
        let dir = TempDir::new().expect("tempdir");
        let (kid, aud, ttl_h, ttl_s) = params();

        // Premier boot — génère la clé.
        let jwt_boot_1 =
            load_or_generate_jwt_key(dir.path(), kid.clone(), aud.clone(), ttl_h, ttl_s)
                .expect("premier boot doit réussir");

        // Token signé par le premier boot.
        let token = jwt_boot_1
            .sign(
                "user-persistance",
                &["read".to_string()],
                TokenScope::Human,
                "main",
            )
            .expect("signature boot 1");

        // Second boot — charge la clé persistée (même répertoire).
        let jwt_boot_2 =
            load_or_generate_jwt_key(dir.path(), kid.clone(), aud.clone(), ttl_h, ttl_s)
                .expect("second boot doit réussir");

        // Le token émis par le premier boot doit être valide sur le second.
        let claims = jwt_boot_2
            .verify(&token)
            .expect("le JWT doit survivre au 'restart' (bug P0 corrigé)");

        assert_eq!(claims.sub, "user-persistance");
        assert_eq!(claims.tenant_id, "main");
    }

    #[test]
    fn permissions_trop_ouvertes_retourne_erreur() {
        let dir = TempDir::new().expect("tempdir");
        let key_path = dir.path().join(format!("{JWT_SIGNING_KEY_NAME}.secret"));

        // Créer un fichier avec permissions trop ouvertes (644).
        std::fs::write(&key_path, vec![0u8; 32]).expect("écriture");
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");

        let (kid, aud, ttl_h, ttl_s) = params();
        let err = match load_or_generate_jwt_key(dir.path(), kid, aud, ttl_h, ttl_s) {
            Err(e) => e,
            Ok(_) => panic!("attendu Err sur permissions ouvertes, obtenu Ok"),
        };
        let err_str = format!("{err:#}");
        assert!(
            err_str.contains("Permissions") || err_str.contains("permissions"),
            "message d'erreur doit mentionner les permissions, obtenu: {err_str}"
        );
    }

    #[test]
    fn fichier_absent_puis_genere_est_relu_correctement() {
        let dir = TempDir::new().expect("tempdir");
        let (kid, aud, ttl_h, ttl_s) = params();

        // Aucun fichier → génération.
        let jwt = load_or_generate_jwt_key(dir.path(), kid, aud, ttl_h, ttl_s)
            .expect("doit générer et réussir");

        // Le service est opérationnel.
        let token = jwt
            .sign(
                "agent-x",
                &["read".into(), "write".into()],
                TokenScope::Service,
                "main",
            )
            .expect("signature");
        let claims = jwt.verify(&token).expect("vérification");
        assert_eq!(claims.sub, "agent-x");
    }

    /// Fichier de seed corrompu (≠ 32 bytes) → Err bien remonté.
    ///
    /// Couvre le cas d'un fichier tronqué/altéré après génération initiale :
    /// le serveur doit refuser de démarrer plutôt que d'utiliser une clé invalide.
    #[test]
    fn fichier_corrompu_n_bytes_retourne_erreur() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().expect("tempdir");
        let (kid, aud, ttl_h, ttl_s) = params();

        // Écrire un fichier secret avec un nombre de bytes invalide (31 au lieu de 32).
        let key_path = dir.path().join(format!("{JWT_SIGNING_KEY_NAME}.secret"));
        std::fs::write(&key_path, vec![0xABu8; 31]).expect("écriture fichier corrompu");
        // Le répertoire doit être à 0o700 (V2) sinon la lecture échoue sur V4 avant V5.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 700 répertoire");
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600 fichier");

        let result = load_or_generate_jwt_key(dir.path(), kid, aud, ttl_h, ttl_s);
        match result {
            Err(err) => {
                let err_str = format!("{err:#}");
                assert!(
                    err_str.contains("32")
                        || err_str.contains("bytes")
                        || err_str.contains("seed")
                        || err_str.contains("corrompu")
                        || err_str.contains("invalide"),
                    "message d'erreur doit mentionner la taille invalide, obtenu: {err_str}"
                );
            }
            Ok(_) => panic!("attendu Err sur bytes corrompus (31 bytes ≠ 32), obtenu Ok"),
        }
    }
}
