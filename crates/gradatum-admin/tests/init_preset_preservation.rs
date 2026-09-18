//! What `init` does to an ACL preset that is already on disk.
//!
//! ## The defect measured
//!
//! `init` materialised the preset unconditionally. On a root that is already
//! initialised, a re-run therefore replaced `config/bearer.toml` with the
//! shipped template, keeping the previous content only in a `.bak.<timestamp>`
//! file. For a deployment whose access policy is hand-maintained, that silently
//! swaps the policy the server enforces for a factory default — and the
//! installer re-runs `init --force` on every pass over an existing root, so the
//! swap happens on an ordinary upgrade.
//!
//! ## The contract these tests require
//!
//! - the preset is written on a root that is not initialised, and kept on one
//!   that is;
//! - `--force` alone does **not** overwrite it: the file is left byte-identical,
//!   and stderr names the file kept and the flag that would replace it;
//! - an explicit `--overwrite-preset` does replace it, after backing it up, and
//!   stderr names both the file and the backup.
//!
//! (The initialised/un-initialised split is pinned by
//! `init_main_agent_key::init_fails_when_bootstrap_identity_absent_and_leaves_no_half_root`,
//! which requires a failed install to stay repairable without `--force`.)
//!
//! `--force` keeps its other meanings unchanged (re-minting `server.toml` and
//! the `[internal_api]` tokens); these tests pin only the preset half.

use std::fs;
use std::path::Path;
use std::process::Command;

/// The preset path relative to the deployment root.
const PRESET_RELATIVE_PATH: &str = "config/bearer.toml";

/// A hand-maintained preset: a real policy an operator wrote, still declaring
/// the mandatory bootstrap identity so a re-init's referential check can pass.
const HAND_MAINTAINED_PRESET: &str = r#"# Hand-maintained policy — NOT the shipped template.
[[consumer]]
identity = "admin"
read_patterns  = ["**"]
write_patterns = ["**"]

[[consumer]]
identity = "main-agent"
read_patterns  = ["team-a/**"]
write_patterns = ["team-a/**"]
"#;

/// Outcome of one `gradatum-admin init` invocation.
struct InitRun {
    success: bool,
    stderr: String,
}

/// Runs `gradatum-admin init <extra…> --root <root> --non-interactive`.
fn run_init(root: &Path, extra: &[&str]) -> InitRun {
    let output = Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .arg("init")
        .args(extra)
        .arg("--root")
        .arg(root)
        .arg("--non-interactive")
        .current_dir(std::env::temp_dir())
        .output()
        .expect("running gradatum-admin init");
    InitRun {
        success: output.status.success(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Names of the `.bak.<timestamp>` files sitting next to the preset.
fn preset_backups(root: &Path) -> Vec<String> {
    let config = root.join("config");
    let mut names: Vec<String> = fs::read_dir(&config)
        .expect("reading config/")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("bearer.toml.bak."))
        .collect();
    names.sort();
    names
}

/// (a) A fresh root gets the preset — the write still happens where it must.
#[test]
fn init_writes_the_preset_on_a_fresh_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let run = run_init(tmp.path(), &["--preset", "hierarchical"]);
    assert!(run.success, "init on a fresh root must succeed");

    let preset = tmp.path().join(PRESET_RELATIVE_PATH);
    let content = fs::read_to_string(&preset)
        .unwrap_or_else(|e| panic!("reading back {}: {e}", preset.display()));
    assert!(
        content.contains("identity = \"admin\""),
        "a fresh root must be materialised with the requested preset"
    );
    assert!(
        preset_backups(tmp.path()).is_empty(),
        "a fresh root has nothing to back up"
    );
}

/// (b) The point of the fix: `--force` alone does not touch a preset that is
/// already there.
#[test]
fn init_force_leaves_a_hand_maintained_preset_byte_identical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert!(
        run_init(tmp.path(), &["--preset", "hierarchical"]).success,
        "initial init must succeed"
    );

    let preset = tmp.path().join(PRESET_RELATIVE_PATH);
    fs::write(&preset, HAND_MAINTAINED_PRESET).expect("replacing the preset by hand");
    let before = fs::read(&preset).expect("reading the hand-maintained preset");

    let run = run_init(tmp.path(), &["--preset", "hierarchical", "--force"]);
    assert!(
        run.success,
        "--force on an existing root must still succeed. stderr: {}",
        run.stderr
    );

    let after = fs::read(&preset).expect("reading back the preset");
    assert_eq!(
        before, after,
        "`init --force` must leave a hand-maintained preset byte-identical: the \
         policy the server enforces is the one on disk, and replacing it with the \
         shipped template swaps the operator's access control for a factory default"
    );
    assert!(
        preset_backups(tmp.path()).is_empty(),
        "a preset that was not rewritten has nothing to back up: {:?}",
        preset_backups(tmp.path())
    );
    assert!(
        run.stderr.contains("bearer.toml"),
        "the operator must be told which file was kept. stderr: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("--overwrite-preset"),
        "the operator must be told which flag replaces it. stderr: {}",
        run.stderr
    );
}

/// (c) The explicit replacement: backed up, then rewritten.
#[test]
fn init_force_with_overwrite_preset_backs_up_and_rewrites() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert!(
        run_init(tmp.path(), &["--preset", "hierarchical"]).success,
        "initial init must succeed"
    );

    let preset = tmp.path().join(PRESET_RELATIVE_PATH);
    fs::write(&preset, HAND_MAINTAINED_PRESET).expect("replacing the preset by hand");

    let run = run_init(
        tmp.path(),
        &["--preset", "hierarchical", "--force", "--overwrite-preset"],
    );
    assert!(
        run.success,
        "an explicit --overwrite-preset must succeed. stderr: {}",
        run.stderr
    );

    let backups = preset_backups(tmp.path());
    assert_eq!(
        backups.len(),
        1,
        "an explicit overwrite must leave exactly one backup, got {backups:?}"
    );
    let saved = fs::read_to_string(tmp.path().join("config").join(&backups[0]))
        .expect("reading the backup");
    assert_eq!(
        saved, HAND_MAINTAINED_PRESET,
        "the backup is the only remaining copy of the replaced policy"
    );

    let written = fs::read_to_string(&preset).expect("reading the preset");
    assert!(
        written.contains("identity = \"admin\""),
        "an explicit overwrite writes the requested preset"
    );
    assert!(
        run.stderr.contains("bearer.toml") && run.stderr.contains(&backups[0]),
        "the message must name the file and the backup that holds the replaced \
         policy. stderr: {}",
        run.stderr
    );
}

/// `--overwrite-preset` is inert on a fresh root: there is nothing to replace,
/// and the flag must not turn into a reason to write twice.
#[test]
fn init_overwrite_preset_on_a_fresh_root_writes_once_and_backs_up_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let run = run_init(
        tmp.path(),
        &["--preset", "hierarchical", "--overwrite-preset"],
    );
    assert!(
        run.success,
        "init with the flag on a fresh root must succeed"
    );
    assert!(
        preset_backups(tmp.path()).is_empty(),
        "nothing existed to back up: {:?}",
        preset_backups(tmp.path())
    );
    assert!(
        fs::read_to_string(tmp.path().join(PRESET_RELATIVE_PATH))
            .expect("reading the preset")
            .contains("identity = \"admin\""),
        "the preset must still be written on a fresh root"
    );
}
