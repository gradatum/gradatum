//! Resolution of the queue SQLite path — SSOT with `gradatum-server`.
//!
//! ## Why this module exists
//!
//! `gradatum-server` derives the queue path with the canonical helper
//! [`gradatum_core::paths::queue_db_path`], applied to `[storage] root` of
//! `server.toml`. Before this module, `gradatum-worker` hard-coded the full
//! literal `/var/lib/gradatum/db/queue.sqlite` as the `--db` default and never
//! consulted `[storage] root` — even though it opens the very same
//! `server.toml` through `--config`.
//!
//! With `storage.root` left at its default the two derivations coincide, which
//! masked the divergence. With a custom `storage.root` the worker opened (and,
//! through `create_if_missing`, **created**) a second, empty queue database:
//! leadership was acquired, no error was logged, and the worker polled an empty
//! queue forever.
//!
//! [`resolve_queue_db_path`] makes the worker derive the path through the same
//! helper as the server, and turns the silent divergence into a boot failure —
//! symmetrical with the `vault_index_path` fail-fast in `gradatum-server`.
//!
//! The resolution also mirrors the server's **layering**, not just its helper:
//! the server merges `Env::prefixed("GRADATUM_").split("__")` on top of the TOML,
//! so `GRADATUM_STORAGE__ROOT` moves its queue database. A worker that read the
//! TOML alone would reopen the same divergence through the environment — and,
//! worse, would validate `--db` against a canonical path the server no longer
//! uses, letting the fail-fast wave the split-brain through.

use std::path::{Path, PathBuf};

use anyhow::bail;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use gradatum_core::paths::queue_db_path;
use serde::Deserialize;

/// Default `[storage] root` when the config file or the section is absent.
///
/// Matches `gradatum_server::config::ServerConfig::default()`; keeping the two
/// aligned is what guarantees that a worker started without a config file still
/// targets the same queue as the server.
const DEFAULT_STORAGE_ROOT: &str = "/var/lib/gradatum";

/// Minimal projection of the `[storage]` section of `server.toml`.
///
/// Only `root` is needed here: every other storage path is derived from it by
/// [`gradatum_core::paths`]. Declared locally to avoid a
/// `gradatum-worker → gradatum-server` dependency (same local-DTO pattern as
/// [`crate::curator_loader`]).
#[derive(Debug, Clone, Deserialize)]
struct WorkerStorageConfig {
    /// Storage root — parent of `db/`, `vault/` and `config/`.
    root: PathBuf,
}

/// Reads `[storage] root` the way `gradatum-server` does: env over TOML over default.
///
/// The layering mirrors `gradatum_server::config::ServerConfig::load` (named
/// rather than linked: depending on the server crate is what this module avoids)
/// — TOML first, then `Env::prefixed("GRADATUM_").split("__")`, so
/// `GRADATUM_STORAGE__ROOT` moves the worker's queue exactly as it moves the
/// server's. Reading the TOML alone would leave the divergence reachable through
/// the environment: `--db` would be validated against a canonical path the
/// server no longer uses, and the fail-fast would wave the split-brain through.
///
/// Only `[storage]` is extracted, so the other `GRADATUM_*` variables of the
/// deployed unit (`GRADATUM_INTERNAL_URL`, `GRADATUM_INTERNAL_TOKEN`) are inert
/// here: without a `__` separator they land as unrelated root keys.
///
/// A missing file, a missing section or a malformed section all yield
/// [`DEFAULT_STORAGE_ROOT`]: the worker must not refuse to boot because of an
/// unrelated TOML section, and the fallback reproduces the historical behaviour
/// exactly.
fn storage_root(config_path: &Path) -> PathBuf {
    let mut fig = Figment::new();
    // A missing file is not an error — but it must not short-circuit the env
    // layer either, since the server needs no TOML for the override to apply.
    if config_path.exists() {
        fig = fig.merge(Toml::file(config_path));
    }
    fig.merge(Env::prefixed("GRADATUM_").split("__"))
        .extract_inner::<WorkerStorageConfig>("storage")
        .map_or_else(|_| PathBuf::from(DEFAULT_STORAGE_ROOT), |cfg| cfg.root)
}

/// Resolves the queue database path the worker must open.
///
/// `cli_db` is the optional `--db` override. When absent the path is derived
/// from the effective `storage.root` — `GRADATUM_STORAGE__ROOT` over
/// `[storage] root` of `config_path` over the built-in default —
/// through [`gradatum_core::paths::queue_db_path`], the same helper the server
/// uses. When present it is *validated* against that canonical path rather than
/// trusted: an override that diverges is rejected.
///
/// # Errors
///
/// Returns an error when `cli_db` diverges from the canonical path derived from
/// the effective `storage.root`. Accepting the override would let the worker
/// create and poll a queue database the server never writes to — a silent,
/// permanent stall.
///
/// # Examples
///
/// ```
/// use std::path::{Path, PathBuf};
/// use gradatum_worker::queue_path::resolve_queue_db_path;
///
/// // No config file, no override, no GRADATUM_STORAGE__ROOT → historical default.
/// let p = resolve_queue_db_path(None, Path::new("/nonexistent/server.toml"))
///     .expect("the default derivation never fails");
/// assert_eq!(p, PathBuf::from("/var/lib/gradatum/db/queue.sqlite"));
/// ```
pub fn resolve_queue_db_path(cli_db: Option<&Path>, config_path: &Path) -> anyhow::Result<PathBuf> {
    let root = storage_root(config_path);
    let canonical = queue_db_path(&root);

    let Some(requested) = cli_db else {
        return Ok(canonical);
    };

    // `Path` compares by components: a trailing slash or a doubled separator in
    // `storage.root` does not produce a false divergence.
    if requested != canonical {
        bail!(
            "--db diverges from the queue path derived from storage.root — refusing to start.\n\
             \t--db      : {}\n\
             \tCanonical : {} (from storage.root = {})\n\
             \tConfig    : {}\n\
             \tstorage.root source: GRADATUM_STORAGE__ROOT if set, otherwise [storage] root \
             of the config above, otherwise the built-in default.\n\
             Drop --db (it is derived from the config) or align it with the canonical path. \
             Starting with a divergent path would create a second, empty queue database and \
             stall every job silently.",
            requested.display(),
            canonical.display(),
            root.display(),
            config_path.display(),
        );
    }

    Ok(canonical)
}
