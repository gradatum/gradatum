//! Provisioning — `with_vault_path` accepte un `VaultId` explicite (GAP-1 amont).
//!
//! Objectif : la signature de provisioning porte un
//! `VaultId` typé au lieu du hardcode `"main"`, en préparation du registre de handles
//! `Map<VaultId, Vault>` (W3). Ici on vérifie **uniquement** que le `vault_id` fourni est
//! propagé jusqu'à `Vault::create` — aucun 2e vault n'est câblé (ça reste W3).
//!
//! Observabilité : `Vault::create` matérialise le namespace sur disque sous
//! `<root>/<vault_id>/`. Si le paramètre était ignoré (retour au hardcode `"main"`),
//! le répertoire attendu n'existerait pas → régression détectée.

use gradatum_core::scope::VaultId;
use gradatum_server::state::AppState;

/// `with_vault_path` provisionne le namespace correspondant au `VaultId` fourni
/// (≠ `"main"`), prouvant que l'identifiant traverse jusqu'à `Vault::create`.
#[tokio::test]
async fn provisioning_creates_namespace_dir_for_given_vault_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let vault_path = tmp.path().join("vault");
    std::fs::create_dir_all(&vault_path).expect("create vault root");

    let _state = AppState::new()
        .with_vault_path(&vault_path, VaultId::new("vault-b"))
        .await
        .expect("with_vault_path doit construire le state pour vault-b");

    // Le namespace physique doit porter le vault_id fourni, pas le défaut "main".
    assert!(
        vault_path.join("vault-b").is_dir(),
        "le répertoire namespace <vault_id>/ doit être créé sous le vault_id fourni"
    );
    assert!(
        !vault_path.join("main").exists(),
        "aucun namespace 'main' ne doit être créé quand un autre vault_id est fourni"
    );
}
