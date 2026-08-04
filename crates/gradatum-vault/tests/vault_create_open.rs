//! Tests d'intégration T11a — `Vault::create` + `Vault::open` + layout init.

mod common;

use gradatum_core::scope::VaultId;
use gradatum_vault::Vault;
use tempfile::TempDir;

#[tokio::test]
async fn create_initializes_layout() {
    let dir = TempDir::new().unwrap();

    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Répertoire notes tenant
    assert!(
        dir.path().join("main").exists(),
        "<root>/main/ doit exister"
    );
    // Répertoire .gradatum
    assert!(
        dir.path().join(".gradatum").exists(),
        "<root>/.gradatum/ doit exister"
    );
    // Index SQLite
    assert!(
        dir.path().join(".gradatum").join("index.db").exists(),
        "<root>/.gradatum/index.db doit exister"
    );
    // Répertoire overrides/<tenant>
    assert!(
        dir.path()
            .join(".gradatum")
            .join("overrides")
            .join("main")
            .exists(),
        "<root>/.gradatum/overrides/main/ doit exister"
    );
    // tenant_id correct
    assert_eq!(vault.vault_id().as_str(), "main");
}

#[tokio::test]
async fn create_custom_tenant_id() {
    let dir = TempDir::new().unwrap();

    let vault = Vault::create(dir.path(), VaultId::new("work"))
        .await
        .unwrap();

    assert!(dir.path().join("work").exists());
    assert!(
        dir.path()
            .join(".gradatum")
            .join("overrides")
            .join("work")
            .exists()
    );
    assert_eq!(vault.vault_id().as_str(), "work");
}

#[tokio::test]
async fn open_existing_vault() {
    let dir = TempDir::new().unwrap();

    // Créer d'abord
    let _ = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Ouvrir ensuite
    let vault = Vault::open(dir.path()).await.unwrap();

    assert_eq!(vault.vault_id().as_str(), "main");
    assert_eq!(vault.root(), dir.path());
}

#[tokio::test]
async fn create_is_idempotent() {
    let dir = TempDir::new().unwrap();

    // Deux appels successifs ne doivent pas échouer
    Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Layout intact
    assert!(dir.path().join(".gradatum").join("index.db").exists());
}

#[tokio::test]
async fn open_defaults_tenant_to_main_when_no_config() {
    let dir = TempDir::new().unwrap();

    // Créer avec "main" — pas de config.toml custom
    Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Ouvrir : le default_tenant_id absent dans config.toml → "main"
    let vault = Vault::open(dir.path()).await.unwrap();
    assert_eq!(vault.vault_id().as_str(), "main");
}
