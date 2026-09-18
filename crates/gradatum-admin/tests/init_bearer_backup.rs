//! What happens to `config/bearer.toml` when it is already there.
//!
//! The preset is the access policy the server enforces, not a generated file. A
//! re-run of `init` over a live root must therefore leave a hand-maintained
//! policy exactly as it is, and replacing it has to be an explicit act:
//!
//! 1. `ExistingPreset::Keep` + file present → byte-identical, no backup;
//! 2. `ExistingPreset::Overwrite` + file present → backup `.bak.<TS>` holding the
//!    original bytes, then the requested preset is written;
//! 3. no file present → written, no backup (nothing to save).

use gradatum_admin::{ExistingPreset, materialize_preset};
use std::fs;
use tempfile::TempDir;

/// A policy an operator maintains by hand, distinct from every shipped preset.
const HAND_MAINTAINED: &str = r#"
# Hand-maintained — must survive every `init` that did not ask to replace it.
[[consumer]]
identity = "custom-user-bot"
read_patterns = ["custom/**"]
write_patterns = ["custom/**"]
"#;

/// Names of the backups sitting next to the preset.
fn backups(config_dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(config_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("bearer.toml.bak."))
        .collect();
    names.sort();
    names
}

/// Case 1 — the file survives a re-materialisation that did not ask for it.
#[test]
fn existing_preset_is_kept_byte_identical() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let bearer = config_dir.join("bearer.toml");
    fs::write(&bearer, HAND_MAINTAINED).unwrap();
    let before = fs::read(&bearer).unwrap();

    materialize_preset(tmp.path(), "hierarchical", None, ExistingPreset::Keep)
        .expect("keeping an existing preset is not an error");

    assert_eq!(
        fs::read(&bearer).unwrap(),
        before,
        "a kept preset must come back byte-for-byte"
    );
    assert!(
        backups(&config_dir).is_empty(),
        "nothing was rewritten, so nothing was backed up: {:?}",
        backups(&config_dir)
    );
}

/// Case 2 — the explicit replacement, which is the only path that writes over it.
#[test]
fn existing_preset_is_backed_up_then_replaced_when_overwrite_is_asked_for() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let bearer = config_dir.join("bearer.toml");
    fs::write(&bearer, HAND_MAINTAINED).unwrap();

    materialize_preset(tmp.path(), "hierarchical", None, ExistingPreset::Overwrite)
        .expect("an explicit overwrite must succeed");

    let names = backups(&config_dir);
    assert_eq!(names.len(), 1, "exactly one backup expected, got {names:?}");
    let saved = fs::read_to_string(config_dir.join(&names[0])).unwrap();
    assert_eq!(
        saved, HAND_MAINTAINED,
        "the backup is the only remaining copy of the replaced policy"
    );

    // The shipped `hierarchical` preset declares the `admin` identity (renamed
    // from the former `maintainer` by the v2.0.0 identity doctrine).
    let written = fs::read_to_string(&bearer).unwrap();
    assert!(
        written.contains("identity = \"admin\""),
        "the requested preset must be the one now on disk"
    );
    assert!(
        !written.contains("custom-user-bot"),
        "the replaced policy must not survive in the active file (a consumer-aware \
         merge is out of scope here)"
    );
}

/// Case 3 — a fresh install has nothing to save.
#[test]
fn fresh_install_writes_the_preset_without_a_backup() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();

    materialize_preset(tmp.path(), "hierarchical", None, ExistingPreset::Overwrite)
        .expect("materializing on a fresh root must succeed");

    assert!(
        backups(&config_dir).is_empty(),
        "no pre-existing file means no backup"
    );
    assert!(config_dir.join("bearer.toml").exists());
}
