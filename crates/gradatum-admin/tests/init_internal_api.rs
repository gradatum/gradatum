//! E2E: `gradatum-admin init` must generate a working `[internal_api]` section.
//!
//! Regression guard for the v2.0.0 P0: a fresh install produced a `server.toml` with no
//! `[internal_api]`, so `gradatum-server` never started its loopback listener and the
//! worker could not reach it — a silent outage (curation/embedding/distillation idle
//! while `/health` stayed green). These tests drive the real binary end to end.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Runs `gradatum-admin init` into `root` (embedded `hierarchical` preset, non-interactive).
///
/// CWD is an arbitrary temp dir to prove the preset is embedded (not read from CWD).
fn run_init(root: &Path) {
    let status = Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .args(["init", "--preset", "hierarchical", "--root"])
        .arg(root)
        .arg("--non-interactive")
        .current_dir(std::env::temp_dir())
        .status()
        .expect("spawn gradatum-admin");
    assert!(status.success(), "gradatum-admin init returned non-zero");
}

/// Extracts a `key = "value"` string from a named TOML section, without a TOML parser
/// (kept out of dev-deps). Matches only inside the target section; `token` never collides
/// with `admin_token` because `strip_prefix` requires the full `key = "` prefix.
fn extract_quoted(content: &str, section: &str, key: &str) -> Option<String> {
    let header = format!("[{section}]");
    let needle = format!("{key} = \"");
    let mut in_section = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t == header;
            continue;
        }
        if in_section && let Some(rest) = t.strip_prefix(&needle) {
            return rest.strip_suffix('"').map(str::to_owned);
        }
    }
    None
}

/// The generated `server.toml` carries `[internal_api]` with two DISTINCT tokens, each
/// long enough to pass `validate_internal_token` (≥ 32 chars), and the default loopback
/// bind — i.e. the server will actually spawn its internal listener.
#[test]
fn init_writes_internal_api_with_two_distinct_valid_tokens() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    run_init(root);

    let server_toml = std::fs::read_to_string(root.join("config/server.toml"))
        .expect("server.toml must exist after init");
    assert!(
        server_toml.contains("[internal_api]"),
        "server.toml missing [internal_api] section:\n{server_toml}"
    );

    let worker = extract_quoted(&server_toml, "internal_api", "token")
        .expect("[internal_api].token must be present");
    let admin = extract_quoted(&server_toml, "internal_api", "admin_token")
        .expect("[internal_api].admin_token must be present");
    let bind = extract_quoted(&server_toml, "internal_api", "bind")
        .expect("[internal_api].bind must be present");

    assert_ne!(worker, admin, "worker and admin tokens must be distinct");
    // validate_internal_token's only constraint is len >= 32 (MIN_INTERNAL_TOKEN_LEN).
    assert!(
        worker.len() >= 32,
        "worker token too short ({}) — would fail validate_internal_token",
        worker.len()
    );
    assert!(
        admin.len() >= 32,
        "admin token too short ({}) — would fail validate_internal_token",
        admin.len()
    );
    assert_eq!(
        bind, "127.0.0.1:19092",
        "bind must match InternalApiConfig::default"
    );
}

/// The worker token is also written to a dedicated 0600 side-file, and it matches the
/// value inlined in `server.toml` (so the operator can feed it to the worker verbatim).
#[test]
fn init_writes_worker_token_sidefile_0600_matching_server_toml() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    run_init(root);

    let side = root.join("config/internal-worker.token.txt");
    assert!(side.is_file(), "config/internal-worker.token.txt missing");
    let mode = side.metadata().unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "internal-worker.token.txt chmod = {mode:o}, expected 0600"
    );

    let side_token = std::fs::read_to_string(&side).unwrap();
    let server_toml = std::fs::read_to_string(root.join("config/server.toml")).unwrap();
    let worker = extract_quoted(&server_toml, "internal_api", "token")
        .expect("[internal_api].token must be present");
    assert_eq!(
        side_token.trim(),
        worker,
        "side-file token must equal server.toml [internal_api].token"
    );
    assert!(
        side_token.trim().len() >= 32,
        "side-file token too short to pass validate_internal_token"
    );
}

/// Two independent installs mint different worker tokens (CSPRNG per run) — no shared or
/// hard-coded default secret.
#[test]
fn two_successive_inits_mint_distinct_worker_tokens() {
    let tmp1 = TempDir::new().unwrap();
    let tmp2 = TempDir::new().unwrap();
    run_init(tmp1.path());
    run_init(tmp2.path());

    let t1 = std::fs::read_to_string(tmp1.path().join("config/internal-worker.token.txt")).unwrap();
    let t2 = std::fs::read_to_string(tmp2.path().join("config/internal-worker.token.txt")).unwrap();
    assert_ne!(
        t1.trim(),
        t2.trim(),
        "two successive inits must mint different worker tokens"
    );
}
