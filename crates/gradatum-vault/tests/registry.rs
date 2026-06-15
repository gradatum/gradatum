//! Tests du trait `Registry` — T2 P2.0c.
//!
//! Vérifie que `Vault` implémente le trait `Registry` avec des comptages réels
//! depuis l'index SQLite (pas les valeurs stub 0/0 de `VaultRegistryStub`).

use gradatum_core::scope::VaultId;
use gradatum_vault::{Registry, Vault};
use tempfile::TempDir;

/// Vérifie que `tenant_count` et `locus_count` retournent des valeurs réelles.
///
/// Après `Vault::create` sur un vault vide, les deux compteurs sont 0.
/// Après `ensure_tenant("main")`, `tenant_count` passe à 1.
#[tokio::test]
async fn vault_registry_returns_real_counts() {
    let dir = TempDir::new().unwrap();
    // create initialise le layout (.gradatum/index.db) — open échoue sans layout.
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("vault create");

    assert_eq!(vault.tenant_count().await.unwrap(), 0);
    assert_eq!(vault.locus_count().await.unwrap(), 0);

    // Après ensure_tenant, le compteur tenant doit passer à 1.
    vault.ensure_tenant("main").await.unwrap();
    assert_eq!(vault.tenant_count().await.unwrap(), 1);
}
