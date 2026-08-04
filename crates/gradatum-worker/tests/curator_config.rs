//! Tests d'intégration — câblage config curator dans `build_curator_pipeline`.
//!
//! T6 P2.0c : vérifie que `build_curator_pipeline` construit le bon backend
//! selon la section `[curator]` du TOML (absent / heuristic / openai_compat).
//!

use gradatum_worker::build_curator_pipeline;
use gradatum_worker::config_health::{ConfigHealth, FallbackCause};
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

    let mut health = ConfigHealth::new();
    let pipeline = build_curator_pipeline(&inexistant, &mut health);
    assert_eq!(
        pipeline.backend_name(),
        "heuristic",
        "config absente doit produire un pipeline heuristique"
    );
    assert_eq!(
        health.degraded().collect::<Vec<_>>(),
        vec![("curator", FallbackCause::FileMissing)],
        "le repli doit être enregistré avec sa cause, pas subi en silence"
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

    let mut health = ConfigHealth::new();
    let pipeline = build_curator_pipeline(toml_file.path(), &mut health);
    assert_eq!(
        pipeline.backend_name(),
        "heuristic",
        "backend=heuristic explicite doit produire un pipeline heuristique"
    );
    assert!(
        !health.is_degraded(),
        "un mode heuristique EXPLICITE n'est pas un repli : ne pas crier au loup"
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

    let mut health = ConfigHealth::new();
    let pipeline = build_curator_pipeline(toml_file.path(), &mut health);

    // Le backend doit être "circuit_breaker" (CircuitBreaker<OpenAiCompatBackend>)
    // et NON "heuristic" — c'est la preuve que le wiring LLM a eu lieu.
    assert_ne!(
        pipeline.backend_name(),
        "heuristic",
        "backend=openai_compat avec [curator.llm] doit produire un pipeline non-heuristique"
    );
    assert!(
        !health.is_degraded(),
        "une config LLM complète et valide ne doit signaler aucun repli"
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

    let mut health = ConfigHealth::new();
    let pipeline = build_curator_pipeline(toml_file.path(), &mut health);
    assert_eq!(
        pipeline.backend_name(),
        "heuristic",
        "backend=openai_compat sans [curator.llm] doit fallback sur heuristic"
    );
    // C'est LE cas que l'arbitrage désigne comme grave : une faute de frappe sur
    // `[curator.llm]` désactive le LLM. Le repli doit être imputé à la sous-section,
    // pas à `[curator]` qui, elle, a parfaitement été lue.
    assert_eq!(
        health.degraded().collect::<Vec<_>>(),
        vec![("curator.llm", FallbackCause::SectionMissing)],
        "le LLM désactivé doit être imputé à [curator.llm], la section réellement absente"
    );
}

/// Une section `[curator]` PRÉSENTE mais rejetée ne doit pas être confondue avec une
/// section absente.
///
/// C'est la discrimination qui manquait : les deux cas produisaient le même pipeline
/// heuristique, et le second — presque toujours une faute de saisie — était indiscernable
/// du premier, souvent légitime.
#[test]
fn curator_section_malformee_est_distinguee_dune_section_absente() {
    // `backend` attend une chaîne ; un entier fait échouer la désérialisation de la
    // section, sans la rendre absente.
    let toml_file = write_toml("[curator]\nbackend = 42\n");

    let mut health = ConfigHealth::new();
    let pipeline = build_curator_pipeline(toml_file.path(), &mut health);

    assert_eq!(
        pipeline.backend_name(),
        "heuristic",
        "une section malformée doit toujours replier sans bloquer le boot"
    );
    assert_eq!(
        health.degraded().collect::<Vec<_>>(),
        vec![("curator", FallbackCause::ParseFailed)],
        "une section rejetée doit porter la cause parse_failed, jamais section_missing"
    );
}

/// Une section `[curator]` absente d'un fichier existant porte `section_missing`.
///
/// Contre-épreuve du test précédent : même pipeline heuristique en sortie, cause
/// différente. C'est la paire qui prouve le pouvoir discriminant, pas chaque test isolé.
#[test]
fn curator_section_absente_est_distinguee_dune_section_malformee() {
    let toml_file = write_toml("[log]\nformat = \"json\"\n");

    let mut health = ConfigHealth::new();
    let pipeline = build_curator_pipeline(toml_file.path(), &mut health);

    assert_eq!(pipeline.backend_name(), "heuristic");
    assert_eq!(
        health.degraded().collect::<Vec<_>>(),
        vec![("curator", FallbackCause::SectionMissing)],
        "une section absente doit porter la cause section_missing, jamais parse_failed"
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
    // `llm_review_timeout_ms` n'est volontairement PAS propagé : le timeout
    // effectif est `[curator.llm] timeout_ms`. Cf. deprecated_review_keys_*.
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

/// Les clés `[curator] llm_review_{endpoint,model,timeout_ms}` sont rapportées
/// comme dépréciées et n'influencent JAMAIS les valeurs effectives.
///
/// Avant le correctif, ces trois clés étaient recopiées dans
/// `CuratorPipelineConfig` puis jamais lues : un opérateur pouvait croire que
/// `llm_review_endpoint` redirigeait les appels de revue, alors que l'URL
/// réellement utilisée est `[curator.llm] base_url`. Le test fige les deux
/// moitiés du contrat : la clé est *signalée*, et la valeur effective reste
/// celle de `[curator.llm]`.
#[test]
fn deprecated_review_keys_are_reported_and_never_override_llm_section() {
    use gradatum_curator::CuratorPipelineConfig;
    use gradatum_worker::{WorkerCuratorConfig, deprecated_review_override_keys};

    // Les trois clés dépréciées pointent délibérément AILLEURS que [curator.llm] :
    // si elles étaient câblées, le trafic LLM partirait vers :9999.
    let toml_content = r#"
backend = "openai_compat"
llm_review_endpoint = "http://decoy.invalid:9999"
llm_review_model = "decoy-model"
llm_review_timeout_ms = 60000

[llm]
backend = "openai_compat"
base_url = "http://localhost:8080"
model = "real-model"
timeout_ms = 5000
"#;

    let worker_cfg: WorkerCuratorConfig =
        toml::from_str(toml_content).expect("parsing TOML WorkerCuratorConfig doit réussir");

    assert_eq!(
        deprecated_review_override_keys(&worker_cfg),
        vec![
            "llm_review_endpoint",
            "llm_review_model",
            "llm_review_timeout_ms"
        ],
        "les trois clés dépréciées présentes doivent être rapportées à l'opérateur"
    );

    let pipeline_cfg = CuratorPipelineConfig::from(&worker_cfg);
    let llm = pipeline_cfg
        .llm
        .as_ref()
        .expect("[curator.llm] est présent dans le TOML");

    assert_eq!(
        llm.base_url, "http://localhost:8080",
        "l'endpoint effectif doit rester [curator.llm] base_url"
    );
    assert_eq!(
        llm.model, "real-model",
        "le modèle effectif doit rester [curator.llm] model"
    );
    assert_eq!(
        llm.timeout_ms, 5000,
        "le timeout effectif doit rester [curator.llm] timeout_ms"
    );
}

/// Aucune clé dépréciée → aucun signalement (cas nominal, pas de bruit au boot).
#[test]
fn deprecated_review_keys_absent_reports_nothing() {
    use gradatum_worker::{WorkerCuratorConfig, deprecated_review_override_keys};

    let worker_cfg: WorkerCuratorConfig =
        toml::from_str("backend = \"heuristic\"\n").expect("parsing TOML doit réussir");

    assert!(
        deprecated_review_override_keys(&worker_cfg).is_empty(),
        "un server.toml sain ne doit produire aucun WARN de dépréciation"
    );
}
