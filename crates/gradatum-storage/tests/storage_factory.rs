//! Tests for the config-driven storage factory ([`build_storage`]).
//!
//! Two layers:
//! - Deterministic unit-style tests that need no network: the `fs` default round-trips,
//!   and misconfigurations fail loudly with a clear message.
//! - One opt-in integration test against a real S3 endpoint, `#[ignore]`d and
//!   self-skipping when the environment is not provisioned — it never breaks the suite.

use gradatum_core::config::StorageBackendConfig;
use gradatum_storage::{StorageError, build_storage};

/// An absent `[storage]` section (i.e. `StorageBackendConfig::default()`) selects the
/// local filesystem backend, and notes round-trip through the abstraction. This is the
/// "configuration absente = local, inchangé" guarantee.
#[tokio::test]
async fn default_config_selects_fs_and_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = StorageBackendConfig::default();
    assert_eq!(cfg.service, "fs", "le défaut doit être le backend fichier");

    let storage = build_storage(&cfg, dir.path()).expect("build_storage(fs) doit réussir en local");

    let key = "notes/hello.md";
    let body = b"# F-86\ncontenu\n";
    storage.write(key, body).await.expect("write");
    assert!(storage.exists(key).await.expect("exists"));
    assert_eq!(storage.read(key).await.expect("read"), body);
    storage.delete(key).await.expect("delete");
    assert!(!storage.exists(key).await.expect("exists after delete"));
}

/// An unknown (or feature-disabled) service name fails at construction with a clear
/// message that names the offending service — never a silent no-op.
#[tokio::test]
async fn unknown_service_fails_loudly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = StorageBackendConfig {
        service: "frobnicate".to_owned(),
        ..StorageBackendConfig::default()
    };

    let err = build_storage(&cfg, dir.path())
        .err()
        .expect("un service inconnu doit échouer");
    match err {
        StorageError::ConfigInvalid(msg) => {
            assert!(
                msg.contains("frobnicate"),
                "le message doit nommer le service fautif, obtenu : {msg}"
            );
        }
        other => panic!("attendu ConfigInvalid, obtenu : {other:?}"),
    }
}

/// `s3` without a `bucket` is rejected at construction, with a message that names the
/// missing parameter and contains no secret.
#[cfg(feature = "s3")]
#[tokio::test]
async fn s3_without_bucket_fails_loudly() {
    let cfg = StorageBackendConfig {
        service: "s3".to_owned(),
        endpoint: Some("https://example.invalid".to_owned()),
        bucket: None,
        ..StorageBackendConfig::default()
    };

    let err = build_storage(&cfg, std::path::Path::new("/ignored"))
        .err()
        .expect("s3 sans bucket doit échouer");
    match err {
        StorageError::ConfigInvalid(msg) => {
            assert!(
                msg.contains("bucket"),
                "le message doit nommer 'bucket', obtenu : {msg}"
            );
        }
        other => panic!("attendu ConfigInvalid, obtenu : {other:?}"),
    }
}

/// End-to-end round-trip against a real S3 endpoint.
///
/// `#[ignore]`d: it never runs in the normal suite. Run explicitly with
/// `cargo test -p gradatum-storage --features s3 -- --ignored s3_round_trip`.
///
/// Even when run, it self-skips unless the environment is fully provisioned. Credentials
/// are loaded by OpenDAL from the standard `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
/// environment variables — this test binds no secret value itself. Point it at an
/// endpoint and bucket (region optional) before running:
///
/// ```sh
/// export AWS_ACCESS_KEY_ID="<your-access-key-id>"
/// export AWS_SECRET_ACCESS_KEY="<your-secret-access-key>"
/// export GRADATUM_S3_TEST_ENDPOINT="<your-s3-endpoint-url>"
/// export GRADATUM_S3_TEST_BUCKET="<your-bucket>"
/// export GRADATUM_S3_TEST_REGION="<your-region>"   # optional
/// ```
#[cfg(feature = "s3")]
#[tokio::test]
#[ignore = "requires a reachable S3 endpoint + AWS_* credentials in the environment"]
async fn s3_round_trip_real() {
    let endpoint = match std::env::var("GRADATUM_S3_TEST_ENDPOINT") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("skip: GRADATUM_S3_TEST_ENDPOINT absent");
            return;
        }
    };
    let bucket = match std::env::var("GRADATUM_S3_TEST_BUCKET") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("skip: GRADATUM_S3_TEST_BUCKET absent (fourni au moment de l'épreuve)");
            return;
        }
    };
    // Presence check only — the value is never bound, copied, or logged.
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_none() {
        eprintln!(
            "skip: AWS_ACCESS_KEY_ID absent — définir AWS_ACCESS_KEY_ID \
             (et AWS_SECRET_ACCESS_KEY) dans l'environnement avant de lancer"
        );
        return;
    }

    let cfg = StorageBackendConfig {
        service: "s3".to_owned(),
        endpoint: Some(endpoint),
        bucket: Some(bucket),
        region: std::env::var("GRADATUM_S3_TEST_REGION")
            .ok()
            .filter(|s| !s.is_empty()),
        // Isolate this run under a dedicated prefix; cleaned up at the end.
        root: Some("gradatum-f86-selftest/".to_owned()),
    };

    // Même contrat qu'au démarrage du serveur : installe le transport HTTP (+ fournisseur
    // crypto) avant toute opération objet. Sans cet appel, `write` échoue sur
    // `ConfigInvalid: default HTTP transport is not installed` (OpenDAL 0.58).
    gradatum_storage::install_object_backend_defaults();

    // `local_root` is irrelevant for S3 — the location comes entirely from `cfg`.
    let storage =
        build_storage(&cfg, std::path::Path::new("/dev/null")).expect("build_storage(s3)");

    let key = "roundtrip.md";
    let body = b"# F-86 s3 roundtrip\n";
    storage.write(key, body).await.expect("write S3");
    assert!(storage.exists(key).await.expect("exists S3"));
    assert_eq!(
        storage.read(key).await.expect("read S3"),
        body,
        "le contenu relu doit être identique"
    );

    let listed = storage.list("").await.expect("list S3");
    assert!(
        listed.iter().any(|e| e.path.ends_with("roundtrip.md")),
        "l'objet écrit doit apparaître dans la liste"
    );

    storage.delete(key).await.expect("delete S3");
    assert!(!storage.exists(key).await.expect("exists after delete S3"));
}
