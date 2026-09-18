//! Boot behaviour of the ACL preset gate: what happens when the configured
//! preset file is absent, unreadable, or unparseable.
//!
//! ## The defect measured
//!
//! The gate treated every failure the same way: log a warning, keep the engine's
//! deny-all default, and serve. For a *fresh install* that is the right call —
//! there is no preset yet, and refusing to boot would make the first run
//! impossible. For a preset that **is** there and cannot be turned into an
//! engine, it is the wrong one: the operator has expressed a policy, the server
//! silently discards it, and the resulting `403` on every request is
//! indistinguishable from a correct restrictive policy. The two states are
//! observationally identical from the outside, which is precisely what makes the
//! silent path dangerous.
//!
//! ## What these tests require
//!
//! The two states are separated by *why* the read failed, not by *whether* it
//! failed:
//!
//! - **absent** (`ErrorKind::NotFound`) or **not configured** (empty path) →
//!   fallback preserved, the server boots and denies everything;
//! - **present but unusable** (parse error, an uncompilable pattern, or any I/O
//!   error other than `NotFound`) → the boot is refused, and the error names the
//!   gate and the path so the operator can act without reading the source.
//!
//! An empty file sits on the *booting* side of that line, which is why it is
//! asserted explicitly rather than left to the reader: the gate is not "the file
//! has no content" but "the content cannot become an engine".
//!
//! The assertions are on the *typed outcome* of the boot call, never on a log
//! line: a `warn!` is not a contract a caller can branch on, and the defect was
//! exactly that nothing outside the log could tell the two states apart.

use std::path::Path;

use gradatum_acl_policy::{AclDecision, AclEngine, AclOp};
use gradatum_core::paths::config_dir;
use gradatum_core::scope::{AgentId, TenantId};
use gradatum_core::trust::TrustContext;
use gradatum_server::config::{ServerConfig, effective_acl_preset_path};
use gradatum_server::state::AppState;

/// A preset that grants `probe` read access to everything under `main/`.
///
/// Used to prove the *positive* case — that a loadable preset really is the one
/// the engine evaluates against — so that a deny-all result elsewhere cannot be
/// explained by the probe being wrong.
const PRESET_ALLOW: &str = r#"
[[consumer]]
identity = "probe"
read_patterns = ["main/**"]
write_patterns = []
"#;

/// A well-formed TOML document that is not a valid ACL preset: the `consumer`
/// entry is missing its required `read_patterns` key. The file parses as TOML
/// and fails as a preset — the exact shape of a preset edited by hand.
const PRESET_INVALID: &str = r#"
[[consumer]]
identity = "probe"
write_patterns = []
"#;

/// A structurally complete preset whose `read_patterns` entry is not a valid
/// glob. The TOML parses and the preset deserialises; it is the pattern
/// compilation that fails.
///
/// This is the sub-door a guard narrowed to "the TOML did not parse" would let
/// through — the failure is real, the file is present, and the operator's
/// intent is unambiguously expressed.
const PRESET_BAD_GLOB: &str = r#"
[[consumer]]
identity = "probe"
read_patterns = ["[unclosed"]
write_patterns = []
"#;

/// The identity every probe below is evaluated as.
const PROBE: &str = "probe";

fn probe_trust() -> TrustContext {
    TrustContext::BearerToken {
        kid: "k1".to_owned(),
        aud: "gradatum".to_owned(),
        sub: AgentId::new(PROBE),
        scopes: vec!["vault_read".to_owned()],
        tenant_id: TenantId::new("main"),
        jti: None,
    }
}

/// The decision the loaded engine returns for the probe on a `main/` locus.
fn probe_decision(state: &AppState) -> AclDecision {
    state
        .acl
        .evaluate(&probe_trust(), AclOp::Read, "main/notes")
}

/// Asserts the state denies the probe — the fallback posture.
///
/// Deny-all is the *absence* of a decision, so any denial arm counts; requiring
/// `DenyImplicit` specifically would over-specify the fallback's internal route
/// without adding a property the caller can rely on.
fn assert_denied(state: &AppState, site: &str) {
    let decision = probe_decision(state);
    assert!(
        matches!(
            decision,
            AclDecision::DenyImplicit | AclDecision::DenyExplicit
        ),
        "[{site}] le repli doit refuser la sonde — un preset abandonné en silence \
         laisse le serveur servir un DENY-ALL indiscernable d'une politique \
         restrictive voulue. Obtenu : {decision:?}"
    );
}

/// Asserts the boot call refused, and that the refusal names the gate and the
/// path — the two facts an operator needs before opening the source.
///
/// `door` is a word that must appear only on the failing arm exercised; it is
/// what keeps the three doors distinguishable from one another rather than
/// merely known to be non-empty.
fn assert_refused(result: Result<AppState, anyhow::Error>, path: &Path, door: &str, site: &str) {
    let err = match result {
        Ok(_) => panic!(
            "[{site}] démarrage attendu REFUSÉ : un preset configuré et illisible \
             ne doit pas être converti en serveur vivant à politique vidée"
        ),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("acl.preset_path"),
        "[{site}] le refus doit nommer la porte de configuration. Obtenu : {err}"
    );
    assert!(
        err.contains(&path.display().to_string()),
        "[{site}] le refus doit nommer le chemin fautif. Obtenu : {err}"
    );
    assert!(
        err.contains(door),
        "[{site}] le refus doit distinguer cette porte (`{door}`) des autres. \
         Obtenu : {err}"
    );
}

// ── The failing arm: present but unusable ────────────────────────────────────

#[test]
fn preset_present_but_invalid_preset_refuses_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bearer.toml");
    std::fs::write(&path, PRESET_INVALID).expect("write fixture");

    let result = AppState::new().with_acl_preset_path(&path);

    // The discriminator is deliberately the *outcome* word, not `invalid TOML`:
    // a preset can also fail to compile on a bad glob pattern or a removed field,
    // and the refusal must name that door without claiming a cause it did not
    // measure — the underlying error already carries the precise one.
    assert_refused(result, &path, "does not compile", "preset invalide");
}

#[test]
fn preset_present_but_unreadable_refuses_startup() {
    // A directory is present on the filesystem and cannot be read as a string.
    // This is the I/O door *other than* `NotFound` — the one a fail-fast that
    // only guards the parse would let through.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bearer.toml");
    std::fs::create_dir(&path).expect("create dir at preset path");

    let result = AppState::new().with_acl_preset_path(&path);

    assert_refused(result, &path, "unreadable", "preset illisible");
}

#[test]
fn preset_present_but_uncompilable_pattern_refuses_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bearer.toml");
    std::fs::write(&path, PRESET_BAD_GLOB).expect("write fixture");

    let result = AppState::new().with_acl_preset_path(&path);

    // Same door, different cause: the TOML parsed and the preset deserialised,
    // so a guard that only watched the TOML parser would have waved this
    // through and served a deny-all the operator never asked for.
    assert_refused(result, &path, "does not compile", "glob invalide");
}

// ── The preserved arm: absent, or not configured ─────────────────────────────

#[test]
fn absent_preset_falls_back_to_deny_all_and_boots() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bearer.toml");
    assert!(!path.exists(), "la fixture doit être absente");

    let state = AppState::new()
        .with_acl_preset_path(&path)
        .expect("un preset absent ne doit pas refuser le démarrage — installation neuve");

    assert_denied(&state, "preset absent");
}

#[test]
fn unconfigured_preset_falls_back_to_deny_all_and_boots() {
    let state = AppState::new()
        .with_acl_preset_path(Path::new(""))
        .expect("aucune porte configurée ne doit pas refuser le démarrage");

    assert_denied(&state, "preset non configuré");
}

/// An **empty** preset is not an unusable one.
///
/// The refusal gate is *"the file cannot be turned into an engine"*, never *"the
/// engine it yields is restrictive"*. A file of zero bytes parses to a valid
/// preset that declares no consumer: the server starts and denies everything.
/// That is a deliberately restrictive policy — served as such — and reading the
/// gate as "the file is empty, therefore it was never written" would refuse a
/// deployment whose deny-all posture is exactly what it intends.
#[test]
fn empty_preset_is_a_valid_deny_all_and_boots() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bearer.toml");
    std::fs::write(&path, "").expect("write fixture");

    let state = AppState::new()
        .with_acl_preset_path(&path)
        .expect("un preset de zéro octet est un preset valide, pas un preset inutilisable");

    assert_denied(&state, "preset vide");
}

/// The same property for a file that holds nothing but whitespace.
///
/// TOML treats it as an empty document, so it lands on the booting side of the
/// gate for the same reason the zero-byte file does — and a hand-edited preset
/// that was emptied out rather than deleted is a realistic shape of it. The two
/// cases are asserted apart because only one of them is a file whose size is
/// zero: a gate that tested `content.is_empty()` would pass the first and fail
/// this one.
#[test]
fn whitespace_only_preset_is_a_valid_deny_all_and_boots() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bearer.toml");
    std::fs::write(&path, "  \n\t\n").expect("write fixture");

    let state = AppState::new()
        .with_acl_preset_path(&path)
        .expect("un preset d'espaces seuls est un preset valide, pas un preset inutilisable");

    assert_denied(&state, "preset espaces seuls");
}

// ── The happy path, so the fallback assertions are load-bearing ──────────────

#[test]
fn valid_preset_is_the_engine_that_evaluates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bearer.toml");
    std::fs::write(&path, PRESET_ALLOW).expect("write fixture");

    let state = AppState::new()
        .with_acl_preset_path(&path)
        .expect("un preset valide doit charger");

    assert_eq!(
        probe_decision(&state),
        AclDecision::Allow,
        "le preset valide doit être celui que le moteur évalue — sans quoi les \
         cas de repli ci-dessus ne prouveraient rien de la porte, seulement que \
         la sonde est refusée"
    );
    assert!(
        AclEngine::from_preset_str(PRESET_ALLOW)
            .expect("fixture valide")
            .has_identity(&AgentId::new(PROBE)),
        "la fixture doit déclarer l'identité sondée"
    );
}

// ── The defaulted path, composed from a real configuration ───────────────────

/// Writes a `server.toml` whose only storage declaration is `root`, and loads it
/// through the real entry-point loader.
fn load_config_for_root(root: &Path) -> ServerConfig {
    use std::io::Write as _;

    let body = format!("[storage]\nroot = \"{}\"\n", root.display());
    let mut file = tempfile::NamedTempFile::new().expect("fichier de config temporaire");
    file.write_all(body.as_bytes()).expect("écriture TOML");
    ServerConfig::load(Some(file.path())).expect("une config sans [acl] doit charger")
}

/// The path the boot gate is handed for a deployment that configured none.
///
/// This is the regression the CI caught, composed end to end: a config whose
/// `storage.root` is a fresh directory, no `[acl]` section, and a boot that must
/// proceed. A host-global file at the old default location used to be read here,
/// and — being present but unreadable — refused the startup of a server that had
/// named no preset.
#[test]
fn defaulted_preset_path_follows_the_root_and_boots() {
    let root = tempfile::tempdir().expect("tempdir");
    let cfg = load_config_for_root(root.path());
    let effective = effective_acl_preset_path(cfg.acl.preset_path.as_deref(), &cfg.storage.root);

    assert!(
        effective.starts_with(root.path()),
        "le chemin par défaut doit appartenir au déploiement : {}",
        effective.display()
    );
    assert!(
        !effective.exists(),
        "la fixture doit être absente (racine neuve) : {}",
        effective.display()
    );

    let state = AppState::new()
        .with_acl_preset_path(&effective)
        .expect("un preset par défaut absent ne doit pas refuser le démarrage");

    assert_denied(&state, "preset par défaut absent");
}

/// The correction must not have loosened the gate: when the deployment's *own*
/// preset is there and cannot be read, the boot is still refused — the fix
/// reclassifies the path, it does not weaken the door.
#[test]
fn defaulted_preset_path_present_but_unreadable_still_refuses_boot() {
    let root = tempfile::tempdir().expect("tempdir");
    let preset = config_dir(root.path()).join("bearer.toml");
    std::fs::create_dir_all(config_dir(root.path())).expect("créer config/");
    // A directory where a file is expected: present on the filesystem, and not
    // readable as a string — an I/O failure other than `NotFound`.
    std::fs::create_dir(&preset).expect("répertoire au chemin du preset");

    let cfg = load_config_for_root(root.path());
    let effective = effective_acl_preset_path(cfg.acl.preset_path.as_deref(), &cfg.storage.root);
    assert_eq!(
        effective, preset,
        "le chemin par défaut doit être celui du déploiement"
    );

    assert_refused(
        AppState::new().with_acl_preset_path(&effective),
        &effective,
        "unreadable",
        "preset par défaut illisible",
    );
}
