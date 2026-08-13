//! End-to-end: creating a vault whose notes live on S3.
//!
//! This is the test that was missing when the `create_dir`-on-S3 defect slipped through:
//! it exercises `Vault::create` on an object backend, not just raw read/write/delete.
//! Object stores reject `create_dir` with `Unsupported`; before the fix, vault
//! bootstrap failed fatally on that call and no S3 vault could be created.
//!
//! `#[ignore]`d and self-skipping without a provisioned S3 environment — it never
//! breaks the suite. Run explicitly with:
//! `cargo test -p gradatum-vault --features s3 -- --ignored vault_create_on_s3`.
//!
//! Credentials are loaded by OpenDAL from the standard `AWS_ACCESS_KEY_ID` /
//! `AWS_SECRET_ACCESS_KEY` environment variables — this test binds no secret value
//! itself. Point it at an endpoint and bucket (region optional) before running:
//!
//! ```sh
//! export AWS_ACCESS_KEY_ID="<your-access-key-id>"
//! export AWS_SECRET_ACCESS_KEY="<your-secret-access-key>"
//! export GRADATUM_S3_TEST_ENDPOINT="<your-s3-endpoint-url>"
//! export GRADATUM_S3_TEST_BUCKET="<your-bucket>"
//! export GRADATUM_S3_TEST_REGION="<your-region>"   # optional
//! ```

#![cfg(feature = "s3")]

use gradatum_core::config::{StorageBackendConfig, VaultConfig};
use gradatum_core::scope::VaultId;
use gradatum_vault::Vault;

/// Returns `(endpoint, bucket)` if the S3 test environment is fully provisioned,
/// `None` otherwise (the test then skips cleanly). Never reads a secret value.
fn s3_env() -> Option<(String, String)> {
    let endpoint = std::env::var("GRADATUM_S3_TEST_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty())?;
    let bucket = std::env::var("GRADATUM_S3_TEST_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())?;
    // Presence only — the credential value is never bound, copied, or logged.
    // `.is_some().then_some(())?` rather than `var_os(...)?`: the latter would bind the
    // secret into a temporary before discarding it. Here nothing but a boolean is ever held.
    std::env::var_os("AWS_ACCESS_KEY_ID")
        .is_some()
        .then_some(())?;
    Some((endpoint, bucket))
}

#[tokio::test]
#[ignore = "requires a reachable S3 endpoint + AWS_* credentials in the environment"]
async fn vault_create_on_s3_round_trips() {
    let Some((endpoint, bucket)) = s3_env() else {
        eprintln!(
            "skip: environnement S3 non provisionné \
             (GRADATUM_S3_TEST_ENDPOINT / _BUCKET + AWS_ACCESS_KEY_ID requis)"
        );
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // Declare the S3 backend in the vault's local config. Creating the local
    // `.gradatum/` directory is intrinsic to selecting S3 (the config lives there),
    // and it is also where the always-local SQLite index will sit.
    std::fs::create_dir_all(root.join(".gradatum")).expect("mkdir .gradatum");
    let cfg = VaultConfig {
        storage: StorageBackendConfig {
            service: "s3".to_owned(),
            endpoint: Some(endpoint),
            bucket: Some(bucket),
            region: std::env::var("GRADATUM_S3_TEST_REGION")
                .ok()
                .filter(|s| !s.is_empty()),
            root: Some("gradatum-f86-vault-test/".to_owned()),
        },
        ..VaultConfig::default()
    };
    std::fs::write(
        root.join(".gradatum").join("config.toml"),
        toml::to_string(&cfg).expect("serialize config"),
    )
    .expect("write config.toml");

    // Même contrat qu'au démarrage du serveur : installe le transport HTTP (+ fournisseur
    // crypto) avant toute opération objet. Sans cet appel, `Vault::create` échoue sur
    // `ConfigInvalid: default HTTP transport is not installed` (OpenDAL 0.58).
    gradatum_storage::install_object_backend_defaults();

    // THE point: creating the vault on S3 must succeed. It calls `create_dir` on the
    // S3 backend three times during bootstrap — each now tolerated, not fatal.
    let vault = Vault::create(root, VaultId::new("main"))
        .await
        .expect("Vault::create doit réussir sur un backend objet S3");

    // A note round-trips through the vault's storage (notes on S3, index stays local).
    let key = "main/f86-e2e.md";
    let body = b"# F-86 vault-create-on-s3\n";
    vault
        .storage()
        .write(key, body)
        .await
        .expect("write note S3");
    assert!(vault.storage().exists(key).await.expect("exists note S3"));
    assert_eq!(vault.storage().read(key).await.expect("read note S3"), body);

    // Cleanup — the object store delete is idempotent.
    vault.storage().delete(key).await.expect("cleanup note S3");
}
