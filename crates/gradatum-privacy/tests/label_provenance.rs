//! Where the label fixture comes from, checked against the model it names.
//!
//! The adaptation table is checked against a fixture of the model's entity groups
//! (`tests/fixtures/model-labels.txt`), and a fixture is worth exactly what its
//! provenance is worth: one nobody can reproduce drifts away from the model it
//! claims to describe, and the table is then checked, green, against a list that no
//! longer exists anywhere. This test runs the sidecar component's own dump script
//! against a real binary and a real model and requires its output to be the fixture,
//! line for line.
//!
//! It is `#[ignore]`d, and gated on two environment variables naming artefacts a
//! checkout does not carry: a built `pf-cli` and the weights it reads. When it is
//! asked to run and either is missing, it fails rather than skipping — a test that
//! reports success without having run anything is worse than no test.
//!
//! ```text
//! PF_CLI_BIN=/path/to/pf-cli \
//! PF_MODEL_PATH=/path/to/model.gguf \
//!   cargo test -p gradatum-privacy --test label_provenance -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The fixture the adaptation table is checked against.
const FIXTURE: &str = include_str!("fixtures/model-labels.txt");

/// Crate root, resolved at run time.
///
/// The value baked in at compile time goes stale as soon as a test binary is reused
/// from another checkout directory (shared build artefacts): the environment variable
/// wins, and the compile-time value only serves as a fallback outside cargo.
fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

/// The component's dump script: the one mechanism that derives the groups.
fn dump_script() -> PathBuf {
    manifest_dir().join("../../tools/privacy-sidecar/tests/dump-labels.sh")
}

/// How many entity groups the deployed model carries.
const GROUPS: usize = 54;

/// Names the child of [`the_dump_script_is_resolved_at_run_time`], which re-executes
/// this binary with a decoy crate root.
const PROBE: &str = "PRIVACY_MANIFEST_DIR_PROBE";

/// The lines that carry content, on both sides of the comparison.
///
/// Comments and blank lines are not part of the list, so a fixture that documents
/// itself is not a divergence from the script's output.
fn lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// Runs the dump script against `model`, with `binary` as the tool it cross-checks
/// with.
fn dump(binary: &str, model: &Path) -> Output {
    Command::new("bash")
        .arg(dump_script())
        .env("PF_CLI_BIN", binary)
        .env("PF_MODEL_PATH", model)
        .output()
        .expect("the dump script can be launched")
}

/// The two artefacts the environment names, or a failure saying which is missing.
///
/// The script itself checks both — this is so that the failure names the variable a
/// reader has to set, rather than surfacing as a script that refused to run.
fn artefacts() -> (String, PathBuf) {
    let binary = std::env::var("PF_CLI_BIN").unwrap_or_else(|_| {
        panic!(
            "PF_CLI_BIN must name a built pf-cli: this test is ignored by default and \
             must not be asked to run without its artefacts"
        )
    });
    let model = std::env::var("PF_MODEL_PATH")
        .unwrap_or_else(|_| panic!("PF_MODEL_PATH must name the model weights pf-cli reads"));
    let model = PathBuf::from(model);
    assert!(
        Path::new(&binary).is_file(),
        "PF_CLI_BIN must name an existing file"
    );
    assert!(model.is_file(), "PF_MODEL_PATH must name an existing file");
    (binary, model)
}

#[test]
fn the_dump_script_is_resolved_at_run_time() {
    // A test binary can be reused from another checkout directory, the build artefacts
    // being shared. A crate root baked in at compile time would then name a directory
    // that is no longer the crate's, and the script would silently be looked for where
    // it no longer is. The contract is therefore that the environment wins, and a
    // constant cannot honour one: the check is a re-execution of this binary with a
    // decoy crate root, which no compile-time value can follow.
    if std::env::var_os(PROBE).is_some() {
        let decoy = std::env::var("CARGO_MANIFEST_DIR").expect("the probe sets a decoy root");
        let script = dump_script();
        assert!(
            script.starts_with(&decoy),
            "the dump script must be resolved under the crate root the environment names \
             ({decoy}), got {}",
            script.display()
        );
        return;
    }
    let exe = std::env::current_exe().expect("the test binary's own path");
    let output = Command::new(exe)
        .args(["--exact", "the_dump_script_is_resolved_at_run_time"])
        .env("CARGO_MANIFEST_DIR", "/decoy-crate-root")
        .env(PROBE, "1")
        .output()
        .expect("the probe child can be launched");
    assert!(
        output.status.success(),
        "the child probe must resolve the crate root from its environment: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "needs a built pf-cli and the model weights, named by PF_CLI_BIN and PF_MODEL_PATH"]
fn the_label_fixture_is_the_vocabulary_of_the_model_it_names() {
    let (binary, model) = artefacts();

    let output = dump(&binary, &model);
    assert!(
        output.status.success(),
        "the dump script must succeed on the model it is given: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let produced = lines(&stdout);
    let fixture = lines(FIXTURE);
    // Stated before the comparison, so that an empty fixture compared against an
    // empty output cannot pass as agreement.
    assert_eq!(
        fixture.len(),
        GROUPS,
        "the fixture must carry the model's entity groups"
    );
    assert_eq!(
        produced, fixture,
        "the fixture must be what the script derives from the model, line for line"
    );
}

#[test]
#[ignore = "needs a built pf-cli, named by PF_CLI_BIN"]
fn the_groups_are_read_from_the_file_and_not_printed_from_a_constant() {
    // The equality above says the fixture and the script agree; on its own it would
    // also hold for a script that printed the fixture it was written next to. This
    // is the other half: the same script, handed a file that is not the model, must
    // fail and print no group — so what it prints depends on the file it is given.
    let (binary, _) = artefacts();
    let not_a_model = manifest_dir().join("tests/fixtures/model-labels.txt");

    let output = dump(&binary, &not_a_model);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "a file that is not the model must not yield a vocabulary, got a successful run \
         printing {} line(s)",
        lines(&stdout).len()
    );
    assert!(
        lines(&stdout).is_empty(),
        "a failed run must print no group on stdout"
    );
}
