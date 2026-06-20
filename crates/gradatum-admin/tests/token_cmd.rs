//! Tests d'intégration AUTH-T4 — sous-commande `gradatum-admin token issue`.
//!
//! Vérifie que :
//! - Les clés Ed25519 générées par `init` sont utilisables pour signer des tokens JWT
//! - Un token signé avec la clé privée est vérifiable avec la clé publique correspondante
//! - Les scopes et tenant_id sont bien embarqués dans le token

use ed25519_dalek::pkcs8::{DecodePrivateKey, DecodePublicKey};
use ed25519_dalek::{SigningKey, VerifyingKey};
use gradatum_auth::jwt::{JwtService, TokenScope};
use tempfile::TempDir;

/// Génère une paire de clés Ed25519 et les écrit dans un TempDir.
///
/// Reproduit la logique de `init::generate_jwt_keys()` pour les tests.
fn generate_keypair(dir: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
    use pkcs8::LineEnding;
    use rand::rngs::OsRng;

    let mut csprng = OsRng;
    let signing = SigningKey::generate(&mut csprng);
    let verifying = signing.verifying_key();

    let priv_path = dir.path().join("jwt.private.pem");
    let pub_path = dir.path().join("jwt.public.pem");

    let priv_pem = signing.to_pkcs8_pem(LineEnding::LF).expect("priv pem");
    std::fs::write(&priv_path, priv_pem.as_bytes()).expect("write priv");

    let pub_pem = verifying
        .to_public_key_pem(LineEnding::LF)
        .expect("pub pem");
    std::fs::write(&pub_path, pub_pem.as_bytes()).expect("write pub");

    (priv_path, pub_path)
}

/// Signature d'un token JWT service depuis une clé PEM et vérification.
#[test]
fn token_issue_roundtrip() {
    let dir = TempDir::new().expect("tempdir");
    let (priv_path, _) = generate_keypair(&dir);

    let pem = std::fs::read_to_string(&priv_path).expect("lecture clé privée");
    let signing = SigningKey::from_pkcs8_pem(&pem).expect("décodage PKCS8 PEM");

    let jwt = JwtService::new(
        signing,
        "test-kid".to_string(),
        "gradatum".to_string(),
        3600,
        86400,
    );

    let scopes = vec!["vault_read".to_string(), "vault_search".to_string()];
    let token = jwt
        .sign("mcp-stub", &scopes, TokenScope::Service, "main")
        .expect("signature doit réussir");

    let claims = jwt.verify(&token).expect("vérification doit réussir");

    assert_eq!(claims.sub, "mcp-stub");
    assert_eq!(claims.tenant_id, "main");
    assert!(claims.scopes.contains(&"vault_read".to_string()));
    assert!(claims.scopes.contains(&"vault_search".to_string()));
}

/// La clé publique dérivée de la privée doit être cohérente avec le fichier PEM public.
#[test]
fn keypair_coherent() {
    let dir = TempDir::new().expect("tempdir");
    let (priv_path, pub_path) = generate_keypair(&dir);

    let priv_pem = std::fs::read_to_string(&priv_path).expect("lecture clé privée");
    let pub_pem = std::fs::read_to_string(&pub_path).expect("lecture clé publique");

    let signing = SigningKey::from_pkcs8_pem(&priv_pem).expect("décodage privée");
    let verifying_from_private = signing.verifying_key();
    let verifying_from_file =
        VerifyingKey::from_public_key_pem(&pub_pem).expect("décodage publique");

    assert_eq!(
        verifying_from_private.as_bytes(),
        verifying_from_file.as_bytes(),
        "clé publique dérivée == fichier jwt.public.pem"
    );
}

/// Token avec TTL personnalisé — vérifier que `exp - iat == ttl_secs`.
#[test]
fn token_custom_ttl() {
    let dir = TempDir::new().expect("tempdir");
    let (priv_path, _) = generate_keypair(&dir);

    let pem = std::fs::read_to_string(&priv_path).expect("lecture");
    let signing = SigningKey::from_pkcs8_pem(&pem).expect("décodage");

    let ttl_secs = 7200_u64;
    let jwt = JwtService::new(
        signing,
        "test-kid".to_string(),
        "gradatum".to_string(),
        3600,
        ttl_secs,
    );

    let token = jwt
        .sign(
            "worker",
            &["vault_write".into()],
            TokenScope::Service,
            "main",
        )
        .expect("signature");

    let claims = jwt.verify(&token).expect("vérification");
    let ttl_effective = claims.exp - claims.iat;
    assert_eq!(
        ttl_effective, ttl_secs,
        "TTL effectif doit correspondre au ttl_secs configuré"
    );
}

/// Token avec tenant non-main : tenant_id préservé dans les claims.
#[test]
fn token_tenant_id_preserved() {
    let dir = TempDir::new().expect("tempdir");
    let (priv_path, _) = generate_keypair(&dir);

    let pem = std::fs::read_to_string(&priv_path).expect("lecture");
    let signing = SigningKey::from_pkcs8_pem(&pem).expect("décodage");

    let jwt = JwtService::new(
        signing,
        "test-kid".to_string(),
        "gradatum".to_string(),
        3600,
        86400,
    );

    let token = jwt
        .sign(
            "agent-1",
            &["vault_read".into()],
            TokenScope::Service,
            "staging",
        )
        .expect("signature");

    let claims = jwt.verify(&token).expect("vérification");
    assert_eq!(claims.tenant_id, "staging");
}
