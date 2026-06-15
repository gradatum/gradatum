//! Tests d'intégration — câblage config curator dans `build_curator_pipeline`.
//!
//! T6 P2.0c : vérifie que `build_curator_pipeline` construit le bon backend
//! selon la section `[curator]` du TOML (absent / heuristic / openai_compat).
//!

use gradatum_worker::build_curator_pipeline;
use std::io::Write as _;
use tempfile::NamedTempFile;

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Crée un fichier TOML temporaire avec le contenu fourni.
/// Le fichier est retourné pour maintenir le lifetime (supprimé à la fin du test).
fn write_toml(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("impossible de créer un fichier TOML temporaire");
    f.write_all(content.as_bytes())
        .expect("impossible d'écrire le TOML temporaire");
    f
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// Test 1 — config absente → pipeline heuristique offline, pas de panique.
///
/// Scénario : le chemin passé à `build_curator_pipeline` ne pointe vers aucun fichier.
/// Comportement attendu : retour `CuratorPipeline` en mode heuristique (backend_name = "heuristic").
#[test]
fn curator_config_absent_fallback_heuristic() {
    let inexistant =
        std::path::PathBuf::from("/tmp/gradatum-test-absent-config-xyz-nonexistent.toml");
    // S'assurer que le fichier n'existe vraiment pas.
    assert!(
        !inexistant.exists(),
        "le fichier de test ne doit pas exister pour ce scénario"
    );

    let pipeline = build_curator_pipeline(&inexistant);
    assert_eq!(
        pipeline.backend_name(),
        "heuristic",
        "config absente doit produire un pipeline heuristique"
    );
}

/// Test 2 — config présente avec `backend = "heuristic"` → pipeline heuristique offline.
///
/// Scénario : TOML minimal avec `[curator] backend = "heuristic"`.
/// Comportement attendu : backend_name = "heuristic", pas de LLM instancié.
#[test]
fn curator_config_heuristic_backend_explicit() {
    let toml_content = r#"
[server]
bind = "127.0.0.1:19090"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "/tmp/gradatum-test"
db_path = "/tmp/gradatum-test/db/index.sqlite"

[auth]
jwt_public_key_path = "/tmp/jwt.public.pem"
jwt_private_key_path = "/tmp/jwt.private.pem"
jwt_ttl_human_secs = 3600
jwt_ttl_service_secs = 86400
revocation_store = "memory"

[acl]
preset_path = "/tmp/bearer.toml"

[log]
format = "json"

[curator]
backend = "heuristic"
"#;
    let toml_file = write_toml(toml_content);

    let pipeline = build_curator_pipeline(toml_file.path());
    assert_eq!(
        pipeline.backend_name(),
        "heuristic",
        "backend=heuristic explicite doit produire un pipeline heuristique"
    );
}

/// Test 3 — config avec `backend = "openai_compat"` + `[curator.llm]` valide
/// → pipeline instancié en mode LLM (circuit breaker wrappé).
///
/// Scénario : TOML complet avec section `[curator.llm]` valide.
/// Comportement attendu : construction réussie sans panique. Le backend_name
/// retourné par `CuratorPipeline::backend_name()` sur un CircuitBreaker est
/// `"circuit_breaker"` — on vérifie que ce n'est PAS `"heuristic"` (wiring effectif).
///
/// Aucun appel réseau n'est effectué — le CircuitBreaker est juste instancié.
#[test]
fn curator_config_openai_compat_llm_section_wires_llm_backend() {
    let toml_content = r#"
[server]
bind = "127.0.0.1:19090"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "/tmp/gradatum-test"
db_path = "/tmp/gradatum-test/db/index.sqlite"

[auth]
jwt_public_key_path = "/tmp/jwt.public.pem"
jwt_private_key_path = "/tmp/jwt.private.pem"
jwt_ttl_human_secs = 3600
jwt_ttl_service_secs = 86400
revocation_store = "memory"

[acl]
preset_path = "/tmp/bearer.toml"

[log]
format = "json"

[curator]
backend = "openai_compat"

[curator.llm]
backend = "openai_compat"
base_url = "http://localhost:8080"
model = "test-model"
timeout_ms = 5000
"#;
    let toml_file = write_toml(toml_content);

    let pipeline = build_curator_pipeline(toml_file.path());

    // Le backend doit être "circuit_breaker" (CircuitBreaker<OpenAiCompatBackend>)
    // et NON "heuristic" — c'est la preuve que le wiring LLM a eu lieu.
    assert_ne!(
        pipeline.backend_name(),
        "heuristic",
        "backend=openai_compat avec [curator.llm] doit produire un pipeline non-heuristique"
    );
}

/// Test 4 — config avec `backend = "openai_compat"` MAIS `[curator.llm]` absent
/// → fallback heuristique (warn log) sans panique.
///
/// Scénario : le TOML indique openai_compat mais omet la section llm.
/// Comportement attendu : warn + fallback heuristic (pas de crash).
#[test]
fn curator_config_openai_compat_missing_llm_section_fallback_heuristic() {
    let toml_content = r#"
[server]
bind = "127.0.0.1:19090"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "/tmp/gradatum-test"
db_path = "/tmp/gradatum-test/db/index.sqlite"

[auth]
jwt_public_key_path = "/tmp/jwt.public.pem"
jwt_private_key_path = "/tmp/jwt.private.pem"
jwt_ttl_human_secs = 3600
jwt_ttl_service_secs = 86400
revocation_store = "memory"

[acl]
preset_path = "/tmp/bearer.toml"

[log]
format = "json"

[curator]
backend = "openai_compat"
# [curator.llm] intentionnellement absent
"#;
    let toml_file = write_toml(toml_content);

    let pipeline = build_curator_pipeline(toml_file.path());
    assert_eq!(
        pipeline.backend_name(),
        "heuristic",
        "backend=openai_compat sans [curator.llm] doit fallback sur heuristic"
    );
}

/// Test 5 — champs gating propagés : `llm_review_enabled = true` + `confidence_threshold = 1.0`
/// → `CuratorPipelineConfig` contient ces valeurs après conversion From<&WorkerCuratorConfig>.
///
/// Ce test vérifie le bug structurel corrigé : sans propagation explicite des champs gating,
/// `llm_review_enabled` restait à `false` (défaut) quelle que soit la config TOML.
/// Scénario : TOML avec `llm_review_enabled = true` + `confidence_threshold = 1.0`
/// (mode force-LLM : toutes les notes passent en revue LLM).
#[test]
fn curator_gating_fields_propagated_to_pipeline_config() {
    use gradatum_curator::CuratorPipelineConfig;
    use gradatum_worker::WorkerCuratorConfig;

    let toml_content = r#"
backend = "openai_compat"
llm_review_enabled = true
confidence_threshold = 1.0
heuristic_admit_threshold = 0.9
llm_review_fallback = "admit-pending-review"
llm_review_timeout_ms = 60000
llm_review_max_tokens = 512

[llm]
backend = "openai_compat"
base_url = "http://localhost:8080"
model = "default"
api_key_env = "GRADATUM_LLM_BEARER"
timeout_ms = 60000
"#;

    let worker_cfg: WorkerCuratorConfig =
        toml::from_str(toml_content).expect("parsing TOML WorkerCuratorConfig doit réussir");

    // Vérification directe des champs gating dans WorkerCuratorConfig
    assert_eq!(
        worker_cfg.llm_review_enabled,
        Some(true),
        "llm_review_enabled doit être Some(true) après parsing TOML"
    );
    assert_eq!(
        worker_cfg.confidence_threshold,
        Some(1.0),
        "confidence_threshold doit être Some(1.0)"
    );
    assert_eq!(
        worker_cfg.heuristic_admit_threshold,
        Some(0.9),
        "heuristic_admit_threshold doit être Some(0.9)"
    );
    assert_eq!(
        worker_cfg.llm_review_fallback.as_deref(),
        Some("admit-pending-review"),
        "llm_review_fallback doit être propagé"
    );
    assert_eq!(
        worker_cfg.llm_review_timeout_ms,
        Some(60_000),
        "llm_review_timeout_ms doit être Some(60000)"
    );
    assert_eq!(
        worker_cfg.llm_review_max_tokens,
        Some(512),
        "llm_review_max_tokens doit être Some(512)"
    );

    // Vérification propagation vers CuratorPipelineConfig via From
    let pipeline_cfg = CuratorPipelineConfig::from(&worker_cfg);

    assert_eq!(
        pipeline_cfg.llm_review_enabled,
        Some(true),
        "llm_review_enabled doit être propagé vers CuratorPipelineConfig"
    );
    assert_eq!(
        pipeline_cfg.confidence_threshold,
        Some(1.0),
        "confidence_threshold doit être propagé vers CuratorPipelineConfig"
    );
    assert_eq!(
        pipeline_cfg.heuristic_admit_threshold,
        Some(0.9),
        "heuristic_admit_threshold doit être propagé vers CuratorPipelineConfig"
    );
    assert_eq!(
        pipeline_cfg.llm_review_fallback.as_deref(),
        Some("admit-pending-review"),
        "llm_review_fallback doit être propagé vers CuratorPipelineConfig"
    );
    assert_eq!(
        pipeline_cfg.llm_review_timeout_ms,
        Some(60_000),
        "llm_review_timeout_ms doit être propagé vers CuratorPipelineConfig"
    );
    assert_eq!(
        pipeline_cfg.llm_review_max_tokens,
        Some(512),
        "llm_review_max_tokens doit être propagé vers CuratorPipelineConfig"
    );
    // LLM doit être câblé
    assert!(
        pipeline_cfg.llm.is_some(),
        "curator.llm doit être présent dans CuratorPipelineConfig"
    );
    assert_eq!(pipeline_cfg.backend, "openai_compat");
}

/// Test 6 — defaults gating : config sans champs gating → defaults attendus.
///
/// Scénario : `WorkerCuratorConfig::default()` → tous les champs gating sont `None`.
/// Comportement attendu dans le pipeline : llm_review_enabled = false (jamais LLM),
/// confidence_threshold = None (le workflow applique 0.7 comme défaut interne).
#[test]
fn curator_gating_defaults_when_absent() {
    use gradatum_curator::CuratorPipelineConfig;
    use gradatum_worker::WorkerCuratorConfig;

    let worker_cfg = WorkerCuratorConfig::default();

    // Tous les champs gating doivent être None par défaut
    assert_eq!(
        worker_cfg.llm_review_enabled, None,
        "llm_review_enabled doit être None par défaut (→ false au pipeline)"
    );
    assert_eq!(
        worker_cfg.confidence_threshold, None,
        "confidence_threshold doit être None par défaut"
    );
    assert_eq!(
        worker_cfg.heuristic_admit_threshold, None,
        "heuristic_admit_threshold doit être None par défaut"
    );
    assert_eq!(
        worker_cfg.llm_review_fallback, None,
        "llm_review_fallback doit être None par défaut"
    );
    assert_eq!(worker_cfg.backend, "heuristic");
    assert!(worker_cfg.llm.is_none());

    // Propagation vers CuratorPipelineConfig — tous None (defaults appliqués par le pipeline)
    let pipeline_cfg = CuratorPipelineConfig::from(&worker_cfg);

    assert_eq!(pipeline_cfg.llm_review_enabled, None);
    assert_eq!(pipeline_cfg.confidence_threshold, None);
    assert_eq!(pipeline_cfg.heuristic_admit_threshold, None);
    assert_eq!(pipeline_cfg.llm_review_fallback, None);
    assert!(pipeline_cfg.llm.is_none());
}
