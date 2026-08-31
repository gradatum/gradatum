//! Regression guard for F-190: the environment is **not** a configuration source.
//!
//! `EngineConfig::load_local` used to advertise a `GRADATUM_ENGINE_` override layer that
//! reached no field at all — a promise failing in silence. The layer was removed rather
//! than wired, because the prefix already carries a credential
//! (`GRADATUM_ENGINE_API_KEY`, exported by every deployed unit through its
//! `EnvironmentFile=`) and pointing it at configuration fields would route a secret
//! through the figment parser, whose `Display` renders offending values.
//!
//! These tests pin the *absence*. Without them, re-adding a provider would silently
//! restore both the ambiguity and the secret path, and nothing would turn red.
//!
//! They live in their own test binary because they mutate the process environment.
//!
//! Compiler avec : `cargo test -p gradatum-engine --features serve`
#![cfg(feature = "serve")]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use gradatum_engine::config::{EngineConfig, RuntimeKind};

/// Serialises the tests of this binary.
///
/// The environment is process-global: under `cargo test` every test of a binary shares
/// it (`cargo nextest` isolates them per process, but the tests must be correct under
/// both harnesses).
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
        // SAFETY: `set_var` is unsound only when another thread reads the environment
        // concurrently. Every test of this binary holds `env_lock()` for its whole body,
        // and none of them spawns a thread that reads the environment, so no concurrent
        // `getenv` can be in flight here.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: same argument as `EnvVarGuard::set` — the lock guard held by the test
        // outlives this drop, so no concurrent reader exists.
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// Writes a minimal, TOML-valid `[engine]` config declaring `runtime = "llamaserver"`.
fn write_config(dir: &Path) -> PathBuf {
    let path = dir.join("70-engine-fixture.toml");
    std::fs::write(
        &path,
        "[engine]\n\
         model_path = \"/opt/gradatum/models/fixture.gguf\"\n\
         model_kind = \"chat\"\n\
         runtime = \"llamaserver\"\n\
         port = 11435\n\
         child_port = 11455\n",
    )
    .expect("writing the fixture config");
    path
}

/// The three spellings an operator would reasonably try must all stay inert.
///
/// `runtime` is the discriminating field on purpose: `onnx` is a *parsable* value that
/// the binary then refuses, so a variable that did reach the field would be visible
/// twice — as a changed `RuntimeKind`, and as a `--check` rejection. Asserting on a port
/// would not discriminate as sharply: a wrong port still yields a valid config.
#[test]
fn no_gradatum_engine_variable_reaches_a_field() {
    let _lock = env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path());

    let _flat = EnvVarGuard::set("GRADATUM_ENGINE_RUNTIME", "onnx");
    let _nested = EnvVarGuard::set("GRADATUM_ENGINE_ENGINE_RUNTIME", "onnx");
    let _split = EnvVarGuard::set("GRADATUM_ENGINE_ENGINE__RUNTIME", "onnx");

    let loaded = EngineConfig::load_local(&cfg).expect("the fixture config must load");

    assert_eq!(
        loaded.runtime,
        RuntimeKind::LlamaServer,
        "the TOML declares runtime = \"llamaserver\"; no GRADATUM_ENGINE_* spelling may \
         move it. A failure here means an environment layer came back — re-read F-190 \
         before wiring one: the prefix carries GRADATUM_ENGINE_API_KEY on every unit."
    );
}

/// The credential variable must not disturb the load either.
///
/// `GRADATUM_ENGINE_API_KEY` is present in the environment of all five deployed engines.
/// It is read by the binary through a direct `std::env::var`, never through the config
/// parser — and loading a config while it is set must stay a no-op.
#[test]
fn the_api_key_variable_does_not_disturb_the_load() {
    let _lock = env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path());

    let _key = EnvVarGuard::set("GRADATUM_ENGINE_API_KEY", "ak_fixture_not_a_real_key");

    let loaded = EngineConfig::load_local(&cfg).expect(
        "a config must load while GRADATUM_ENGINE_API_KEY is set — this is the state of \
         every deployed unit",
    );

    assert_eq!(loaded.port, 11435, "the TOML value must survive untouched");
    assert_eq!(loaded.runtime, RuntimeKind::LlamaServer);
}
