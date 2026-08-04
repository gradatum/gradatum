//! SSOT regression tests for the queue path resolution (`--db`).
//!
//! Before the fix, `gradatum-worker` hard-coded `/var/lib/gradatum/db/queue.sqlite`
//! as the `--db` default and never read `[storage] root`. With a custom
//! `storage.root` it therefore opened — and created — a second, empty queue
//! database while the server wrote to another one. No error, no log, jobs stuck
//! forever.
//!
//! Each test below fails against that behaviour.

use std::path::{Path, PathBuf};

use gradatum_worker::queue_path::resolve_queue_db_path;

/// Writes a `server.toml` containing only `[storage] root` and returns its path.
fn write_config(dir: &Path, root: &str) -> PathBuf {
    let p = dir.join("server.toml");
    std::fs::write(&p, format!("[storage]\nroot = \"{root}\"\n")).expect("writing server.toml");
    p
}

/// Without `--db`, the path must be derived from `[storage] root`.
///
/// Proves the SSOT: the worker resolves the same path as
/// `gradatum-server` (`queue_db_path(&cfg.storage.root)`). The old code
/// returned the hard-coded `/var/lib/gradatum/db/queue.sqlite` here.
#[test]
fn derives_queue_path_from_storage_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path(), "/srv/gradatum-alt");

    let resolved = resolve_queue_db_path(None, &cfg).expect("derivation must succeed");

    assert_eq!(
        resolved,
        PathBuf::from("/srv/gradatum-alt/db/queue.sqlite"),
        "the queue path must follow [storage] root, never a hard-coded literal"
    );
}

/// A `--db` override that diverges from `[storage] root` must abort the boot.
///
/// This is the symmetric counterpart of the `vault_index_path` fail-fast in
/// `gradatum-server`. The old code accepted the divergence silently.
#[test]
fn rejects_db_override_diverging_from_storage_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path(), "/srv/gradatum-alt");

    let err = resolve_queue_db_path(Some(Path::new("/var/lib/gradatum/db/queue.sqlite")), &cfg)
        .expect_err("a divergent --db must be rejected");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("/srv/gradatum-alt/db/queue.sqlite"),
        "the error must name the canonical path so the operator can fix the unit; got: {msg}"
    );
}

/// An explicit `--db` equal to the canonical path must keep working.
///
/// Non-regression guard for every installation deployed with the historical unit,
/// which carried `--db /var/lib/gradatum/db/queue.sqlite` alongside the default
/// `storage.root = /var/lib/gradatum`. The shipped unit no longer passes `--db`,
/// but an already-installed one still does and must keep booting — as must any
/// operator who passes it by hand.
#[test]
fn accepts_db_override_matching_storage_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path(), "/var/lib/gradatum");

    let resolved =
        resolve_queue_db_path(Some(Path::new("/var/lib/gradatum/db/queue.sqlite")), &cfg)
            .expect("an override equal to the canonical path is legitimate");

    assert_eq!(resolved, PathBuf::from("/var/lib/gradatum/db/queue.sqlite"));
}

/// A trailing separator in `[storage] root` is not a divergence.
///
/// `Path` equality compares components, so `/var/lib/gradatum/` and
/// `/var/lib/gradatum` derive the same queue path.
#[test]
fn trailing_separator_in_storage_root_is_not_a_divergence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path(), "/var/lib/gradatum/");

    resolve_queue_db_path(Some(Path::new("/var/lib/gradatum/db/queue.sqlite")), &cfg)
        .expect("a trailing slash must not be reported as a divergence");
}

/// Missing config file → historical default, unchanged.
#[test]
fn falls_back_to_default_root_when_config_is_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("absent.toml");

    let resolved = resolve_queue_db_path(None, &missing).expect("the fallback must not fail");

    assert_eq!(
        resolved,
        PathBuf::from("/var/lib/gradatum/db/queue.sqlite"),
        "without a config file the worker must keep its historical target"
    );
}

/// A config file without a `[storage]` section → historical default, unchanged.
#[test]
fn falls_back_to_default_root_when_storage_section_is_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = dir.path().join("server.toml");
    std::fs::write(&cfg, "[apalis]\n").expect("writing server.toml");

    let resolved = resolve_queue_db_path(None, &cfg).expect("the fallback must not fail");

    assert_eq!(resolved, PathBuf::from("/var/lib/gradatum/db/queue.sqlite"));
}
