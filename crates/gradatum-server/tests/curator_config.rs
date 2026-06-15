//! Vérifie le parsing TOML de la configuration curator :
//! - défaut heuristic (aucun LLM requis)
//! - tier LLM openai_compat complet
//! - chaîne fallback LLM → heuristic

use gradatum_server::config::ServerConfig;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Écrit un fragment de config minimal (server + storage obligatoires) dans un
/// fichier temporaire, charge la config et retourne-la.
fn load_config_from_str(toml_str: &str) -> ServerConfig {
    let tmp = tempfile::TempDir::new().expect("répertoire temporaire");
    let cfg_path = tmp.path().join("server.toml");
    std::fs::write(&cfg_path, toml_str).expect("écriture config temporaire");
    ServerConfig::load(Some(&cfg_path)).expect("ServerConfig::load doit réussir")
}

/// Fragment TOML minimal valide (bind loopback, storage sous `tmp`).
fn minimal_toml(tmp_path: &std::path::Path, extra: &str) -> String {
    format!(
        r#"
[server]
bind = "127.0.0.1:0"
metrics_bind = "127.0.0.1:0"
[storage]
root = "{root}"
db_path = "{root}/index.sqlite"
{extra}
"#,
        root = tmp_path.display(),
        extra = extra
    )
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// La config par défaut (aucun TOML curator) utilise heuristic, aucun LLM.
#[test]
fn default_config_uses_heuristic_no_llm() {
    let cfg = ServerConfig::default();
    assert_eq!(
        cfg.curator.backend, "heuristic",
        "backend curator par défaut doit être 'heuristic'"
    );
    assert!(
        cfg.curator.llm.is_none(),
        "aucun LLM ne doit être configuré dans l'install par défaut"
    );
}

/// Un TOML avec `[curator]` backend = "openai_compat" + `[curator.llm]` complet
/// est parsé correctement.
#[test]
fn loads_curator_with_openai_compat_llm() {
    let tmp = tempfile::TempDir::new().expect("répertoire temporaire");
    let extra = r#"
[curator]
backend = "openai_compat"

[curator.llm]
backend = "openai_compat"
base_url = "http://localhost:11434/v1"
model = "Qwen3-4B-Instruct-2507"
api_key_env = "GRADATUM_LLM_API_KEY"
timeout_ms = 5000
"#;
    let toml_str = minimal_toml(tmp.path(), extra);
    let cfg = load_config_from_str(&toml_str);

    assert_eq!(cfg.curator.backend, "openai_compat");
    let llm = cfg
        .curator
        .llm
        .expect("curator.llm doit être présent pour openai_compat");
    assert_eq!(llm.backend, "openai_compat");
    assert_eq!(llm.base_url, "http://localhost:11434/v1");
    assert_eq!(llm.model, "Qwen3-4B-Instruct-2507");
    assert_eq!(llm.api_key_env.as_deref(), Some("GRADATUM_LLM_API_KEY"));
    assert_eq!(llm.timeout_ms, 5000);
    assert!(llm.fallback.is_none(), "pas de fallback configuré ici");
}

/// La chaîne de fallback LLM → heuristic est parsée correctement.
#[test]
fn loads_curator_with_fallback_chain() {
    let tmp = tempfile::TempDir::new().expect("répertoire temporaire");
    let extra = r#"
[curator]
backend = "openai_compat"

[curator.llm]
backend = "openai_compat"
base_url = "http://localhost:11434/v1"
model = "Qwen3-4B-Instruct-2507"
timeout_ms = 3000

[curator.llm.fallback]
backend = "heuristic"
base_url = ""
model = ""
"#;
    let toml_str = minimal_toml(tmp.path(), extra);
    let cfg = load_config_from_str(&toml_str);

    let llm = cfg.curator.llm.expect("curator.llm doit être présent");
    assert_eq!(llm.backend, "openai_compat");
    assert_eq!(llm.timeout_ms, 3000);

    let fallback = llm
        .fallback
        .expect("curator.llm.fallback doit être présent");
    assert_eq!(
        fallback.backend, "heuristic",
        "fallback doit pointer vers heuristic"
    );
    // Le fallback heuristic n'a pas besoin de timeout explicite — valeur par défaut
    assert_eq!(
        fallback.timeout_ms,
        5000, // default_timeout_ms()
        "timeout par défaut du fallback doit être 5000 ms"
    );
}

/// Un TOML sans section `[curator]` ne doit PAS casser le chargement —
/// backward compat avec les configs T8/P2.0a qui ignorent curator.
#[test]
fn missing_curator_section_uses_default() {
    let tmp = tempfile::TempDir::new().expect("répertoire temporaire");
    let toml_str = minimal_toml(tmp.path(), "");
    let cfg = load_config_from_str(&toml_str);
    assert_eq!(cfg.curator.backend, "heuristic");
    assert!(cfg.curator.llm.is_none());
}
