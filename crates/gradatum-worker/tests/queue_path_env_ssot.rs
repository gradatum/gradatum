//! SSOT regression tests for the **environment** door of the queue path.
//!
//! `gradatum-server` builds its configuration with
//! `Figment::from(Serialized::defaults(..)).merge(Toml::file(..)).merge(Env::prefixed("GRADATUM_").split("__"))`,
//! so `GRADATUM_STORAGE__ROOT` moves `storage.root` — and therefore the queue
//! database — for the server. A worker that only reads the TOML would keep
//! targeting the TOML-derived (or default) path: the very divergence
//! `resolve_queue_db_path` exists to prevent, reintroduced through the
//! environment and invisible to the `--db` fail-fast.
//!
//! These tests pin the worker's resolution to the server's resolution order
//! (env > TOML > default). They live in their own test binary because they
//! mutate the process environment.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use gradatum_worker::queue_path::resolve_queue_db_path;

/// Serialises the tests of this binary.
///
/// The environment is process-global: under `cargo test` every test of a binary
/// shares it (`cargo nextest` isolates them per process, but the tests must be
/// correct under both harnesses).
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    // A panicking test must not poison the environment for the others: the guard
    // restores the previous value on unwind anyway.
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sets an environment variable for the lifetime of the guard, then restores it.
struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        // SAFETY: `set_var` is unsound only when another thread reads the
        // environment concurrently. Every test of this binary holds `env_lock()`
        // for its whole body, and no test spawns a thread that reads the
        // environment, so no concurrent `getenv` can be in flight here.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: same argument as `EnvVarGuard::set` — the lock guard held by
        // the test outlives this drop, so no concurrent reader exists.
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// Writes a `server.toml` containing only `[storage] root` and returns its path.
fn write_config(dir: &Path, root: &str) -> PathBuf {
    let p = dir.join("server.toml");
    std::fs::write(&p, format!("[storage]\nroot = \"{root}\"\n")).expect("writing server.toml");
    p
}

/// `GRADATUM_STORAGE__ROOT` must win over the TOML, exactly as on the server.
///
/// Before the fix the worker returned the TOML-derived path while the server
/// opened the env-derived one — a silent split-brain the `--db` check could not
/// see.
#[test]
fn env_storage_root_overrides_toml() {
    let _lock = env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path(), "/srv/from-toml");
    let _env = EnvVarGuard::set("GRADATUM_STORAGE__ROOT", "/srv/from-env");

    let resolved = resolve_queue_db_path(None, &cfg).expect("derivation must succeed");

    assert_eq!(
        resolved,
        PathBuf::from("/srv/from-env/db/queue.sqlite"),
        "the worker must honour GRADATUM_STORAGE__ROOT like gradatum-server does"
    );
}

/// The env override applies even when no config file exists.
///
/// The server needs no TOML for `GRADATUM_STORAGE__ROOT` to take effect (its
/// base layer is `ServerConfig::default()`); an early `return` on a missing file
/// would leave the worker on the historical default.
#[test]
fn env_storage_root_applies_without_config_file() {
    let _lock = env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("absent.toml");
    let _env = EnvVarGuard::set("GRADATUM_STORAGE__ROOT", "/srv/from-env");

    let resolved = resolve_queue_db_path(None, &missing).expect("derivation must succeed");

    assert_eq!(
        resolved,
        PathBuf::from("/srv/from-env/db/queue.sqlite"),
        "a missing config file must not short-circuit the environment layer"
    );
}

/// A `--db` that matches the TOML but not the env-moved root is a divergence.
///
/// This is the case the fail-fast used to miss entirely: `--db` equal to the
/// TOML-derived path looked canonical to the worker while the server had already
/// moved to the env-derived one.
#[test]
fn db_override_diverging_from_env_storage_root_is_rejected() {
    let _lock = env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path(), "/srv/from-toml");
    let _env = EnvVarGuard::set("GRADATUM_STORAGE__ROOT", "/srv/from-env");

    let err = resolve_queue_db_path(Some(Path::new("/srv/from-toml/db/queue.sqlite")), &cfg)
        .expect_err("a --db aligned on the TOML but not on the env root must be rejected");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("/srv/from-env/db/queue.sqlite"),
        "the error must name the env-derived canonical path; got: {msg}"
    );
}

/// Non-regression: the variables of the deployed unit must not disturb anything.
///
/// `packaging/systemd/gradatum-worker.service` loads `/etc/gradatum/env`, which
/// carries `GRADATUM_INTERNAL_URL` and `GRADATUM_INTERNAL_TOKEN`. They share the
/// `GRADATUM_` prefix consumed by the env layer but carry no `__`, so they land
/// as unrelated root keys and must never reach `[storage]`.
#[test]
fn unrelated_gradatum_env_vars_do_not_disturb_resolution() {
    let _lock = env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path(), "/var/lib/gradatum");
    let _url = EnvVarGuard::set("GRADATUM_INTERNAL_URL", "http://127.0.0.1:19090");
    let _token = EnvVarGuard::set("GRADATUM_INTERNAL_TOKEN", "dummy-token");

    let resolved =
        resolve_queue_db_path(Some(Path::new("/var/lib/gradatum/db/queue.sqlite")), &cfg)
            .expect("the deployed unit must keep booting");

    assert_eq!(resolved, PathBuf::from("/var/lib/gradatum/db/queue.sqlite"));
}
