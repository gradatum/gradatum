//! Test d'intégration AUTH-T6 — câblage SqliteRevocationStore dans AppState.
//!
//! Vérifie que `AppState::with_revocation_path` injecte un store persistant :
//! - revoke(jti) → is_revoked(jti) == true dans la même instance
//! - Le fichier SQLite est bien créé sur disque

use std::time::{Duration, SystemTime};

use gradatum_server::state::AppState;
use tempfile::TempDir;

/// Vérifie que le store SQLite est fonctionnel après injection dans AppState.
#[tokio::test]
async fn appstate_sqlite_revocation_store_persists() {
    let dir = TempDir::new().expect("tempdir");
    let revocation_path = dir.path().join("revocation.sqlite");

    let state = AppState::new()
        .with_revocation_path(&revocation_path)
        .await
        .expect("revocation store init");

    // Le fichier SQLite doit exister après init.
    assert!(
        revocation_path.exists(),
        "le fichier SQLite de révocation doit être créé par with_revocation_path"
    );

    // Révoquer un jti.
    let jti = "test-jti-12345";
    let exp = SystemTime::now() + Duration::from_secs(3600);
    state
        .revocation
        .revoke(jti, "main", exp)
        .await
        .expect("revoke doit réussir");

    // Le jti révoqué doit être reconnu comme tel.
    let is_revoked = state
        .revocation
        .is_revoked(jti, "main")
        .await
        .expect("is_revoked doit réussir");
    assert!(
        is_revoked,
        "le jti révoqué doit être reconnu par le store SQLite"
    );

    // Un jti non révoqué ne doit pas être bloqué.
    let is_other_revoked = state
        .revocation
        .is_revoked("autre-jti-non-revoque", "main")
        .await
        .expect("is_revoked non-révoqué doit réussir");
    assert!(
        !is_other_revoked,
        "un jti non révoqué ne doit pas être bloqué"
    );
}

/// Vérifie que le store détecte correctement les tokens expirés (exp dans le passé).
#[tokio::test]
async fn appstate_revocation_expired_not_blocked() {
    let dir = TempDir::new().expect("tempdir");
    let revocation_path = dir.path().join("revocation_exp.sqlite");

    let state = AppState::new()
        .with_revocation_path(&revocation_path)
        .await
        .expect("revocation store init");

    // Révoquer avec exp dans le passé (token déjà expiré).
    let jti = "jti-expire";
    let exp_passe = SystemTime::now() - Duration::from_secs(1);
    state
        .revocation
        .revoke(jti, "main", exp_passe)
        .await
        .expect("revoke avec exp passé doit réussir");

    // Un token expiré ne doit pas être considéré comme révoqué (il n'est plus actif de toute façon).
    let is_revoked = state
        .revocation
        .is_revoked(jti, "main")
        .await
        .expect("is_revoked doit réussir");
    assert!(
        !is_revoked,
        "un token avec exp dans le passé ne doit pas être considéré comme révoqué actif"
    );
}
