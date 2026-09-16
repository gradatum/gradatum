//! `project-map scope --json` — the machine-readable contract of the audit report.
//!
//! These tests invoke the REAL binary (`CARGO_BIN_EXE_gradatum-admin`) and parse its
//! stdout, because the contract belongs to the command, not to the module: a schema
//! proven only at the level of `ProjectScopeJson` would say nothing about the bytes the
//! CLI actually prints. The consumer is the external audit consumer (block A7.2), which indexes
//! into the object by key.
//!
//! ## What this file pins, and why it is a contract rather than a convenience
//!
//! A renamed key, a re-nested counter or one stray prose line on stdout is a silent
//! breakage for that consumer: the parser fails, or — worse — reads a default and reports
//! a number it never measured. Hence:
//!
//! - the exact set of top-level keys is asserted, not only the ones a given test reads;
//! - `--json` stdout must parse as a single JSON document, with no preamble and no
//!   trailing commentary around it;
//! - the prose labels that the current consumer extracts with `sed` are frozen as well,
//!   so the human-readable report can only be re-shaped once that consumer has actually
//!   switched to the JSON form.
//!
//! ## Exit codes
//!
//! `--json` does not change the exit code: `0` when the reconciliation holds, `2` when it
//! does not (see `project_map_scope_exit_code.rs`). Note that `2` is also what clap
//! returns for an unknown argument, so a consumer must discriminate on the payload —
//! `jq -e has("reconciled")` — never on the code alone.

use gradatum_admin::project_map_scope::EXIT_SCOPE_RECONCILED;
use gradatum_core::paths::vault_index_path;
use rusqlite::Connection;
use tempfile::TempDir;

/// Body of a project-map card carrying the typed wikilinks read by the command.
fn card_body(project: &str, status: &str, version: &str) -> String {
    format!(
        "[[project:{project}]] [[status:{status}]] [[kind:FEATURE]] [[version:{project}/{version}]]\n\nItem de test."
    )
}

/// Materialises `<root>/vault/.gradatum/index.db` and inserts one project-map card per
/// `(id, status)` pair, every card carrying `[[project:gradatum]]`.
fn seed_index(root: &std::path::Path, cards: &[(&str, &str)]) {
    let db_path = vault_index_path(root);
    std::fs::create_dir_all(db_path.parent().expect("index.db has a parent")).expect("mkdir");
    let conn = Connection::open(&db_path).expect("open the test index.db");
    conn.execute_batch(
        "CREATE TABLE notes (
            id TEXT PRIMARY KEY, vault_id TEXT NOT NULL, section TEXT NOT NULL,
            body_text TEXT NOT NULL, title TEXT, status TEXT NOT NULL,
            created INTEGER NOT NULL DEFAULT 0)",
    )
    .expect("create the test notes table");
    for (i, (id, card_status)) in cards.iter().enumerate() {
        conn.execute(
            "INSERT INTO notes (id, vault_id, section, body_text, title, status, created)
             VALUES (?1, 'main', 'project-map', ?2, ?3, 'live', ?4)",
            rusqlite::params![
                id,
                card_body("gradatum", card_status, "2.0.6"),
                format!("Carte {id}"),
                i64::try_from(i).expect("test index fits in an i64"),
            ],
        )
        .expect("insert a test card");
    }
}

/// Runs `gradatum-admin project-map scope --root <root> --vault main gradatum`.
fn run_scope(root: &std::path::Path, extra: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .args(["project-map", "scope"])
        .arg("--root")
        .arg(root)
        .args(["--vault", "main"])
        .args(extra)
        .arg("gradatum")
        .output()
        .expect("run the gradatum-admin binary")
}

/// Sorted key names of a JSON object, so a schema comparison is order-independent while
/// still catching both a missing key and an added one.
fn sorted_keys(value: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("the payload must be a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// The JSON schema is stable: every documented key is present, with the expected value.
#[test]
fn scope_json_has_stable_schema() {
    let dir = TempDir::new().expect("tempdir");
    seed_index(
        dir.path(),
        &[("n1", "OPEN"), ("n2", "OPEN"), ("n3", "DONE")],
    );

    let out = run_scope(dir.path(), &["--json"]);

    assert_eq!(
        out.status.code(),
        Some(EXIT_SCOPE_RECONCILED),
        "a reconciled scope must still exit {EXIT_SCOPE_RECONCILED} under --json — \
         got {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout is one JSON document");

    assert_eq!(
        sorted_keys(&payload),
        [
            "by_status",
            "current_version",
            "gap",
            "project",
            "reconciled",
            "status_sum",
            "total",
            "versions",
        ],
        "the top-level key set is the contract — got:\n{stdout}"
    );
    assert_eq!(
        sorted_keys(&payload["by_status"]),
        [
            "blocked",
            "brainstorming",
            "done",
            "in_progress",
            "obsolete",
            "open",
            "unaccounted",
        ],
        "the per-status key set is the contract — got:\n{stdout}"
    );

    assert_eq!(payload["project"], "gradatum");
    assert_eq!(payload["current_version"], "2.0.6");
    assert_eq!(payload["total"], 3);
    assert_eq!(payload["by_status"]["open"], 2);
    assert_eq!(payload["by_status"]["in_progress"], 0);
    assert_eq!(payload["by_status"]["blocked"], 0);
    assert_eq!(payload["by_status"]["done"], 1);
    assert_eq!(payload["by_status"]["obsolete"], 0);
    assert_eq!(payload["by_status"]["brainstorming"], 0);
    assert_eq!(payload["by_status"]["unaccounted"], 0);
    assert_eq!(payload["status_sum"], 3);
    assert_eq!(payload["gap"], 0);
    assert_eq!(payload["reconciled"], true);
    assert_eq!(payload["versions"], serde_json::json!(["2.0.6"]));
}

/// `--json` prints the object and NOTHING else: the whole stdout must parse.
///
/// A leading banner or a trailing summary line would be read by a consumer that pipes
/// stdout straight into a JSON parser as a syntax error — the failure mode this test
/// exists to prevent.
#[test]
fn scope_json_prints_no_prose() {
    let dir = TempDir::new().expect("tempdir");
    seed_index(dir.path(), &[("n1", "DONE")]);

    let stdout = String::from_utf8(run_scope(dir.path(), &["--json"]).stdout).expect("UTF-8");

    assert!(
        serde_json::from_str::<serde_json::Value>(&stdout).is_ok(),
        "the entire stdout must be one JSON document, with no surrounding prose — got:\n{stdout}"
    );
}

/// The four extractions of the audit consumer are frozen **as that consumer reads them**.
///
/// The external audit consumer (A7.2) does not look for the labels as substrings: it reads the
/// report line by line, matches `^ *<label> *: *<number>` and captures the number. So
/// freezing the prose means replaying that extraction, never `stdout.contains(label)`:
/// a rewording that keeps the words and changes the FORM — `cartes total (3 cartes)`,
/// `ecart (gap total−sum) = 0` — satisfies `contains` while A7.2 captures nothing, and
/// the audit falls to a degraded branch that is silent by construction. A guarantee
/// announced and not held is worse than no guarantee: it authorises the rewording.
///
/// Each captured number is compared to the JSON payload rendered from the same seeded
/// index, so the two surfaces must also agree on the value — not merely on the shape.
#[test]
fn scope_prose_labels_are_frozen() {
    let dir = TempDir::new().expect("tempdir");
    // A card with a status outside the typed buckets falls into `unaccounted`, so the
    // four numbers are not all equal: a swapped label is caught, not just a dropped one.
    seed_index(
        dir.path(),
        &[("n1", "OPEN"), ("n2", "DONE"), ("n3", "WEIRD")],
    );

    let json = String::from_utf8(run_scope(dir.path(), &["--json"]).stdout).expect("UTF-8");
    let payload: serde_json::Value =
        serde_json::from_str(&json).expect("the --json payload must parse");
    let prose = String::from_utf8(run_scope(dir.path(), &[]).stdout).expect("UTF-8");

    // (label, extraction, expected value from the JSON of the same index).
    for (label, pattern, expected) in [
        (
            "cartes total",
            r"(?m)^ *cartes total *: *(-?[0-9]+)",
            payload["total"].as_i64().expect("total is a number"),
        ),
        (
            "somme statuts",
            r"(?m)^ *somme statuts *: *(-?[0-9]+)",
            payload["status_sum"]
                .as_i64()
                .expect("status_sum is a number"),
        ),
        (
            "gap total−sum",
            r"(?m)^ *gap total−sum *: *(-?[0-9]+)",
            payload["gap"].as_i64().expect("gap is a number"),
        ),
        (
            "unclassified",
            r"(?m)^ *unclassified *: *(-?[0-9]+)",
            payload["by_status"]["unaccounted"]
                .as_i64()
                .expect("unaccounted is a number"),
        ),
    ] {
        let re = regex::Regex::new(pattern).expect("the test pattern must compile");
        let capture = re.captures(&prose).unwrap_or_else(|| {
            panic!(
                "A7.2 no longer extracts `{label}` — the prose changed FORM, not just wording:\n{prose}"
            )
        });
        let extracted: i64 = capture[1]
            .parse()
            .expect("the capture must be a number, as A7.2 reads it");
        assert_eq!(
            extracted, expected,
            "the prose line `{label}` must carry the JSON value — prose:\n{prose}\njson:\n{json}"
        );
    }
}
