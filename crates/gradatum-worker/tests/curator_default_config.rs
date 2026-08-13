//! Producer → consumer contract test for the default curator configuration.
//!
//! Wires the REAL producer (`gradatum_admin::generate_server_toml_template`, what
//! `gradatum-admin init` writes) into the REAL consumer
//! (`gradatum_worker::curator_loader::build_curator_pipeline`, what the worker runs at boot).
//!
//! It proves the single property the whole change exists for: a freshly-`init`'d config
//! makes the worker's curator enter LLM mode (calls the model) instead of staying in
//! heuristic mode (never calls the model). A template-only assertion could not prove this —
//! only feeding the produced TOML through the loader that ships in the binary does.

use std::io::Write;

use gradatum_admin::generate_server_toml_template;
use gradatum_worker::config_health::ConfigHealth;
use gradatum_worker::curator_loader::build_curator_pipeline;

/// The default `init` template loads into the worker as an ACTIVE LLM curator, with no
/// configuration fallback recorded.
///
/// Two independent assertions guard against the two ways this could silently regress:
/// - `backend_name() == "openai_compat"` — the pipeline instantiated the LLM backend, not
///   the heuristic one. This is the difference between "calls the model" and the exact bug
///   this change fixes ("curates in heuristic, never calls the model").
/// - `!health.is_degraded()` — neither `[curator]` nor `[curator.llm]` fell back to defaults
///   (missing section, malformed section, missing LLM sub-section). A degraded load would
///   mean the produced config is written-then-ignored, i.e. the config-landmine class.
#[test]
fn init_default_config_yields_active_llm_curator() {
    let toml = generate_server_toml_template(
        std::path::Path::new("/var/lib/gradatum"),
        "127.0.0.1:19090",
        // Internal-API tokens: irrelevant to the curator section, any 64-hex value passes.
        "0000000000000000000000000000000000000000000000000000000000000000",
        "1111111111111111111111111111111111111111111111111111111111111111",
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("server.toml");
    std::fs::File::create(&path)
        .and_then(|mut f| f.write_all(toml.as_bytes()))
        .expect("write server.toml");

    let mut health = ConfigHealth::new();
    let pipeline = build_curator_pipeline(&path, &mut health);

    assert_eq!(
        pipeline.backend_name(),
        "openai_compat",
        "default init config must drive the LLM curator backend, not heuristic — \
         the produced [curator] backend was not consumed"
    );
    assert!(
        !health.is_degraded(),
        "default init config must load [curator]/[curator.llm] cleanly, no fallback; got: {}",
        health.degraded_summary()
    );
}
