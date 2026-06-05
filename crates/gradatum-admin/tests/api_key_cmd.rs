//! Tests d'intégration AUTH-T3 — sous-commande `gradatum-admin api-key`.
//!
//! Vérifie que :
//! - `api-key create` génère un secret valide et persisté
//! - `api-key list` retourne les clés créées
//! - `api-key revoke` révoque une clé (AlreadyRevoked sur second appel)
//! - `api-key rotate` révoque l'ancienne et génère une nouvelle atomiquement

use gradatum_acl_auth::{ApiKeyStore, SqliteApiKeyStore};
use tempfile::TempDir;

/// Ouvre ou crée un store SQLite dans un TempDir.
async fn open_store(dir: &TempDir) -> SqliteApiKeyStore {
    let db_path = dir.path().join("api_keys.sqlite");
    SqliteApiKeyStore::init(&db_path)
        .await
        .expect("init store doit réussir")
}

/// Création d'une clé et vérification du secret retourné.
#[tokio::test]
async fn create_returns_valid_secret() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let material = store
        .create(
            "mcp-stub",
            vec!["vault_read".into()],
            "main".into(),
            Some("test key".into()),
        )
        .await
        .expect("create doit réussir");

    // Le secret doit commencer par le préfixe `ak_`.
    assert!(
        material.secret.starts_with("ak_"),
        "le secret doit commencer par 'ak_'"
    );
    // Le préfixe doit être cohérent avec le secret.
    assert_eq!(
        &material.secret[..material.prefix.len()],
        &material.prefix[..],
        "le préfixe doit être le début du secret"
    );
}

/// Vérification du secret après création — roundtrip create → verify.
#[tokio::test]
async fn create_verify_roundtrip() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let material = store
        .create("owner-1", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create");

    let key = store
        .verify(&material.secret)
        .await
        .expect("verify avec le bon secret doit réussir");

    assert_eq!(key.owner, "owner-1");
    assert_eq!(key.tenant_id, "main");
    assert!(!key.is_revoked());
}

/// Vérification avec mauvais secret → NotFound.
#[tokio::test]
async fn verify_wrong_secret_returns_not_found() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    store
        .create("owner-1", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create");

    let err = store
        .verify("ak_0000000000000000000000000000000000") // 32 hex = 34 chars avec préfixe
        .await
        .expect_err("verify avec mauvais secret doit échouer");

    assert!(
        matches!(err, gradatum_acl_auth::ApiKeyError::NotFound),
        "mauvais secret → NotFound, obtenu : {err}"
    );
}

/// Lister les clés actives.
#[tokio::test]
async fn list_active_keys() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    store
        .create("owner-a", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create a");
    store
        .create("owner-b", vec!["vault_write".into()], "main".into(), None)
        .await
        .expect("create b");

    let keys = store.list(false).await.expect("list");
    assert_eq!(keys.len(), 2, "2 clés actives attendues");
    assert!(keys.iter().any(|k| k.owner == "owner-a"));
    assert!(keys.iter().any(|k| k.owner == "owner-b"));
}

/// Lister toutes les clés (y compris révoquées).
#[tokio::test]
async fn list_all_includes_revoked() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let mat_a = store
        .create("owner-a", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create a");
    store
        .create("owner-b", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create b");

    // Révoquer owner-a.
    store.revoke(&mat_a.prefix).await.expect("revoke a");

    // list(false) = seulement actives.
    let active = store.list(false).await.expect("list active");
    assert_eq!(active.len(), 1, "1 clé active après révocation");

    // list(true) = toutes.
    let all = store.list(true).await.expect("list all");
    assert_eq!(all.len(), 2, "2 clés au total");
}

/// Révoquer une clé → AlreadyRevoked sur second appel.
#[tokio::test]
async fn revoke_twice_returns_already_revoked() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let mat = store
        .create("owner-x", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create");

    store
        .revoke(&mat.prefix)
        .await
        .expect("première révocation");

    let err = store
        .revoke(&mat.prefix)
        .await
        .expect_err("deuxième révocation doit échouer");

    assert!(
        matches!(err, gradatum_acl_auth::ApiKeyError::AlreadyRevoked),
        "deuxième révocation → AlreadyRevoked, obtenu : {err}"
    );
}

/// Révoquer une clé inexistante → NotFound.
#[tokio::test]
async fn revoke_nonexistent_returns_not_found() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let err = store
        .revoke("ak_inexistant")
        .await
        .expect_err("révocation clé inexistante doit échouer");

    assert!(
        matches!(err, gradatum_acl_auth::ApiKeyError::NotFound),
        "clé inexistante → NotFound, obtenu : {err}"
    );
}

/// Rotation : nouvelle clé valide, ancienne révoquée.
#[tokio::test]
async fn rotate_produces_new_valid_key() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let mat_old = store
        .create("owner-r", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create");

    let mat_new = store
        .rotate(&mat_old.prefix)
        .await
        .expect("rotation doit réussir");

    // Le nouveau secret doit être différent de l'ancien.
    assert_ne!(
        mat_old.secret, mat_new.secret,
        "le nouveau secret doit être différent de l'ancien"
    );

    // L'ancienne clé doit être révoquée.
    let err = store
        .verify(&mat_old.secret)
        .await
        .expect_err("l'ancienne clé doit être révoquée après rotation");
    assert!(
        matches!(
            err,
            gradatum_acl_auth::ApiKeyError::AlreadyRevoked
                | gradatum_acl_auth::ApiKeyError::NotFound
        ),
        "ancienne clé après rotation → AlreadyRevoked ou NotFound, obtenu : {err}"
    );

    // La nouvelle clé doit être vérifiable.
    let key = store
        .verify(&mat_new.secret)
        .await
        .expect("nouvelle clé après rotation doit être vérifiable");
    assert_eq!(key.owner, "owner-r");
}
