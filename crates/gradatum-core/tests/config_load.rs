//! Tests de chargement VaultConfig depuis TOML.
//!
//! Tests chargement VaultConfig depuis TOML.

use gradatum_core::config::{ConfigError, VaultConfig};
use std::fs;
use tempfile::TempDir;

/// Helper : crée un vault root avec `.gradatum/config.toml` contenant `content`.
fn vault_with_config(content: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let gradatum_dir = dir.path().join(".gradatum");
    fs::create_dir_all(&gradatum_dir).unwrap();
    fs::write(gradatum_dir.join("config.toml"), content).unwrap();
    dir
}

/// Fichier absent → Default sans erreur, tous les champs Option sont None.
#[test]
fn missing_file_returns_default() {
    let dir = TempDir::new().unwrap();
    let cfg = VaultConfig::load_from_root(dir.path()).expect("missing file → default");
    assert!(cfg.vault.default_tenant_id.is_none());
    assert!(cfg.embed.embedder_id.is_none());
    assert!(cfg.embed.embeddable_status.is_none());
    assert!(cfg.curator.confidence_threshold.is_none());
    assert!(cfg.curator.llm_review_enabled.is_none());
    assert!(!cfg.audit.strict_mode);
}

/// Seule la section [vault] présente → embed/curator/… restent sur Default.
#[test]
fn minimal_config_only_vault_section() {
    let raw = include_str!("../fixtures/config-minimal.toml");
    let dir = vault_with_config(raw);
    let cfg = VaultConfig::load_from_root(dir.path()).unwrap();
    assert_eq!(cfg.vault.default_tenant_id.as_deref(), Some("main"));
    // Autres sections retombent sur Default
    assert!(cfg.embed.embedder_id.is_none());
    assert!(cfg.curator.confidence_threshold.is_none());
}

/// Round-trip complet : tous les champs D-perf-1/2/3 lus correctement.
#[test]
fn full_config_round_trip_all_sections() {
    let raw = include_str!("../fixtures/config-full.toml");
    let dir = vault_with_config(raw);
    let cfg = VaultConfig::load_from_root(dir.path()).expect("parse full config");

    // Vault
    assert_eq!(cfg.vault.default_tenant_id.as_deref(), Some("main"));
    assert_eq!(cfg.vault.schema_version, Some(1));

    // Embed (D-perf-1)
    assert_eq!(cfg.embed.embedder_id.as_deref(), Some("bge-m3"));
    assert_eq!(cfg.embed.dim, Some(1024));
    assert_eq!(cfg.embed.backend.as_deref(), Some("http"));
    assert_eq!(cfg.embed.fallback_backend.as_deref(), Some("fastembed"));
    assert_eq!(
        cfg.embed.http_url.as_deref(),
        Some("http://127.0.0.1:8432/embed")
    );
    assert_eq!(cfg.embed.http_timeout_ms, Some(5000));
    assert_eq!(cfg.embed.http_model.as_deref(), Some("bge-m3"));
    let allowed = cfg
        .embed
        .embeddable_status
        .as_ref()
        .expect("embeddable_status");
    assert_eq!(allowed.len(), 3);
    assert_eq!(allowed[0], "live");
    assert_eq!(allowed[1], "pending-review");
    assert_eq!(allowed[2], "staging");

    // Curator (D-perf-3)
    assert_eq!(cfg.curator.heuristic_admit_threshold, Some(0.6));
    assert_eq!(cfg.curator.llm_review_enabled, Some(true));
    assert_eq!(cfg.curator.confidence_threshold, Some(0.7));
    assert_eq!(
        cfg.curator.llm_review_endpoint.as_deref(),
        Some("http://127.0.0.1:8435/v1")
    );
    assert_eq!(
        cfg.curator.llm_review_model.as_deref(),
        Some("qwen3.6-35b-a3b-q4-k-xl")
    );
    assert_eq!(cfg.curator.llm_review_timeout_ms, Some(30_000));
    assert_eq!(cfg.curator.llm_review_max_tokens, Some(512));
    assert_eq!(
        cfg.curator.llm_review_fallback.as_deref(),
        Some("pending-review-fallback")
    );

    // Index
    assert_eq!(cfg.index.backend.as_deref(), Some("sqlite"));
    assert_eq!(cfg.index.fts_tokenizer.as_deref(), Some("unicode61"));

    // Drift
    assert_eq!(cfg.drift.scan_interval_seconds, Some(3600));

    // Audit (caveat C2)
    assert_eq!(cfg.audit.rotation.as_deref(), Some("daily"));
    assert_eq!(cfg.audit.retention_days, Some(0));
    assert!(!cfg.audit.strict_mode);
}

/// TOML malformé → ConfigError::Parse (pas de panique, pas de Default).
#[test]
fn malformed_toml_returns_parse_error() {
    let dir = vault_with_config("[vault\nbroken");
    let err = VaultConfig::load_from_root(dir.path()).expect_err("expected parse error");
    assert!(
        matches!(err, ConfigError::Parse(_)),
        "expected Parse variant, got: {:?}",
        err
    );
}
