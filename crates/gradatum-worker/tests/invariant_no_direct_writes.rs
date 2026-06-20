//! Invariant test — worker-flip architectural guarantee.
//!
//! Verifies that `crates/gradatum-worker/src/` contains no direct vault/index
//! write calls. All mutations must go through `InternalClient`.
//!
//! The patterns checked mirror the grep invariant from the worker-flip spec:
//!
//! ```text
//! grep -rnE "\.write_note_with_id\(|\.insert_note_embedding\(|
//!            \.upsert_note_title\(|\.mark_forgotten\(|\.set_note_trust\(|
//!            \.delete_note\(|SqliteIndex::open|open_or_create_vault|
//!            Vault::open|with_vault|with_index"
//!   crates/gradatum-worker/src/ | grep -vE "///|//!|^\s*//"
//! ```
//!
//! The only allowed match for `delete_note` is via `client.delete_note(...)` in
//! `apalis_handlers.rs` (InternalClient call, not a direct write). This test
//! explicitly permits that case.

use std::path::Path;
use walkdir::WalkDir;

/// Patterns that indicate a direct vault/index write — forbidden in worker src.
///
/// `delete_note` is handled specially (allowed only when via `client.`).
const FORBIDDEN_PATTERNS: &[&str] = &[
    ".write_note_with_id(",
    ".insert_note_embedding(",
    ".upsert_note_title(",
    ".mark_forgotten(",
    ".set_note_trust(",
    "SqliteIndex::open",
    "open_or_create_vault",
    "Vault::open",
    ".with_vault(",
    ".with_index(",
];

/// `delete_note` is allowed only via `client.delete_note(` — direct calls are
/// forbidden.
const FORBIDDEN_DELETE_NOTE: &str = ".delete_note(";
const ALLOWED_DELETE_NOTE: &str = "client.delete_note(";

/// Checks whether a source line is a comment (doc or line comment).
fn is_comment(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("///") || trimmed.starts_with("//!") || trimmed.starts_with("//")
}

/// Scans all `.rs` files under `src/` for forbidden patterns.
///
/// Returns a `Vec` of violation strings (`"path:line: content"`) for human-readable
/// assertion output.
fn scan_for_violations(src_dir: &Path) -> Vec<String> {
    let mut violations = Vec::new();

    for entry in WalkDir::new(src_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
    {
        let path = entry.path();
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            // Skip binary or unreadable files.
            Err(_) => continue,
        };

        for (line_no, line) in contents.lines().enumerate() {
            if is_comment(line) {
                continue;
            }

            // Check forbidden patterns.
            for pattern in FORBIDDEN_PATTERNS {
                if line.contains(pattern) {
                    violations.push(format!(
                        "{}:{}: {}",
                        path.display(),
                        line_no + 1,
                        line.trim()
                    ));
                }
            }

            // `delete_note` allowed only via `client.delete_note(`.
            if line.contains(FORBIDDEN_DELETE_NOTE) && !line.contains(ALLOWED_DELETE_NOTE) {
                violations.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    line_no + 1,
                    line.trim()
                ));
            }
        }
    }

    violations
}

/// Invariant: no direct vault/index writes in `gradatum-worker/src/`.
///
/// The worker-flip refactor (v0.5.3) ensures the worker never touches
/// `SqliteIndex` or `Vault` directly. All mutations route through
/// `InternalClient`, which the server implements over HTTP.
///
/// This test fails if any future change re-introduces a direct write.
#[test]
fn invariant_no_direct_vault_index_writes_in_worker_src() {
    // Resolve `crates/gradatum-worker/src/` relative to this test file.
    // `CARGO_MANIFEST_DIR` is set by Cargo to the crate root.
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo");
    let src_dir = Path::new(&manifest_dir).join("src");

    assert!(
        src_dir.exists(),
        "worker src/ directory not found at {}",
        src_dir.display()
    );

    let violations = scan_for_violations(&src_dir);

    assert!(
        violations.is_empty(),
        "worker-flip invariant violated: direct vault/index writes found in src/.\n\
         All mutations must go through InternalClient.\n\
         Violations:\n{}",
        violations.join("\n")
    );
}
