//! Désambiguïsation `Vault.tenant_id` → `vault_id`.
//!
//! Le champ historiquement nommé `tenant_id` sur [`Vault`] désigne en réalité le
//! **namespace** physique du vault (le répertoire `<vault_id>/` sur disque), PAS le
//! **principal** authentifié (qui, lui, est porté par `TenantId` dans le `TrustContext`).
//!
//! Ce test verrouille le nom correct de l'accesseur : `Vault::vault_id()` doit exister
//! et retourner le namespace du vault servi par l'instance.

use gradatum_core::scope::VaultId;
use gradatum_vault::Vault;
use tempfile::TempDir;

/// L'accesseur `vault_id()` retourne le namespace du vault (« main » par défaut).
#[tokio::test]
async fn vault_id_accessor_returns_namespace() {
    let dir = TempDir::new().expect("tempdir");

    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create — vault_namespace_naming");

    // Le namespace (pas le principal) : reflète le VaultId passé à `create`.
    assert_eq!(vault.vault_id().as_str(), "main");
}
