//! `gradatum-admin init` — bootstraps a Gradatum root directory.
//!
//! ## Effects
//! - Creates `{root}/{md,db,config}` with mode 0750
//! - Materializes the ACL preset TOML into `config/bearer.toml` 0640
//! - Initializes `db/queue.sqlite`, `db/revocation.sqlite` and `db/api_keys.sqlite` in WAL mode
//! - Mints the mandatory `main-agent` bootstrap key (R4) into the api-key store and
//!   records its secret in `config/main-agent.apikey.txt` 0600
//! - Generates an admin bearer (32 random bytes, hex-encoded) → `config/admin.bearer.txt` 0600
//! - Mints the server↔worker internal-API secrets: two independent 256-bit CSPRNG tokens
//!   (the worker `token` and the operator `admin_token`), written inline into the
//!   `[internal_api]` section of `config/server.toml`. Without that section
//!   `gradatum-server` never starts its loopback listener (`main.rs`), so the worker
//!   cannot reach it and all curation / embedding / distillation stays silently idle.
//!   The worker `token` is additionally copied to `config/internal-worker.token.txt` 0600
//!   so the operator can feed it to the worker via `GRADATUM_INTERNAL_TOKEN`.
//! - Writes `config/server.toml` with default values 0640
//!   (`jwt_ttl_human_secs=3600`, `jwt_ttl_service_secs=86400`, `revocation_store=sqlite`,
//!    `[internal_api]` with `bind=127.0.0.1:19092` + the two tokens)
//!
//! ## JWT signing key
//!
//! `init` does **not** create it. `gradatum-server` generates
//! `config/jwt-signing-key.secret` (mode 0600) at first boot and is its sole
//! owner; `gradatum-admin token issue` loads that same file. Until v1.0.0 `init`
//! scaffolded a PKCS#8 PEM pair no runtime component ever read.
//!
//! ## Security
//! - Refuses with an explicit error if `config/admin.bearer.txt` already exists; pass
//!   `--force` to re-initialize
//! - The admin bearer, the `main-agent` key secret and the internal worker token are each
//!   printed to stdout **exactly once**, only in interactive mode, and are **never logged**
//!   at any level (the internal tokens also stay redacted in `InternalApiConfig`'s `Debug`)
//! - The `main-agent` mint (R4) runs **before** the `admin.bearer.txt` marker is written:
//!   on failure no marker is left behind, so the root stays un-initialised and `init` is
//!   safe to retry without `--force`

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::Args;
use gradatum_acl_auth::{ApiKeyStore, SqliteApiKeyStore};
use gradatum_acl_policy::AclEngine;
use gradatum_core::paths::queue_db_path;
use gradatum_core::scope::AgentId;
use rand::RngCore;
use rand::rngs::OsRng;
use rusqlite::Connection;

/// Arguments for the `init` sub-command.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// ACL preset to materialize.
    ///
    /// Built-in short names: `hierarchical` (default), `flat`.
    /// For a custom preset, pass a path containing `/`
    /// (e.g. `--preset /etc/gradatum/my-preset.toml`).
    #[arg(long, default_value = "hierarchical")]
    pub preset: String,

    /// Gradatum root directory to initialize.
    #[arg(long)]
    pub root: PathBuf,

    /// Projects to substitute into `${PROJECT}` template placeholders (comma-separated).
    #[arg(long)]
    pub projects: Option<String>,

    /// Server listen address (written to `server.toml`).
    #[arg(long, default_value = "127.0.0.1:19090")]
    pub bind: String,

    /// Non-interactive mode: no prompt, bearer not printed to stdout.
    /// Useful for tests and CI pipelines.
    #[arg(long)]
    pub non_interactive: bool,

    /// Forces re-initialization even if `config/admin.bearer.txt` already exists.
    #[arg(long)]
    pub force: bool,
}

/// Entry point for the `init` sub-command.
///
/// # Errors
/// - Returns an error if `config/admin.bearer.txt` exists and `--force` is not set.
/// - Returns an error if the mandatory `main-agent` bootstrap key cannot be minted (R4):
///   an undeclared identity in the preset, an unreadable/unparseable preset, or a store
///   failure. In that case no `admin.bearer.txt` marker is written, so the root stays
///   un-initialised and the command is safe to retry once the preset is fixed.
/// - Propagates any I/O or cryptographic generation error.
pub async fn run(args: InitArgs) -> Result<()> {
    let bearer_marker = args.root.join("config/admin.bearer.txt");

    if bearer_marker.exists() && !args.force {
        return Err(anyhow!(
            "init already performed (admin.bearer.txt exists in {}); \
             pass --force to re-initialize",
            args.root.display()
        ));
    }

    create_layout(&args.root)?;

    // The preset and the SQLite stores are materialised BEFORE the bootstrap mint:
    // the R4 mint validates `main-agent` against `config/bearer.toml` and writes into
    // `db/api_keys.sqlite`, so both must exist first.
    materialize_preset(&args.root, &args.preset, args.projects.as_deref())?;
    init_sqlite_dbs(&args.root)?;

    // R4 — the `main-agent` bootstrap key is mandatory. Minted BEFORE the admin bearer
    // (the "initialised" marker) so that a failure leaves no marker behind: the root is
    // not reported as initialised and `init` can be retried without `--force`.
    mint_bootstrap_key(&args.root, args.non_interactive).await?;

    // Under --force: remove the existing admin bearer before regenerating it.
    // generate_admin_bearer uses create_new (O_EXCL), which fails if the file already
    // exists — the intended behaviour in normal mode.
    //
    // The JWT signing key is deliberately absent from this: it belongs to
    // `gradatum-server`, which creates it at first boot. Deleting it here would
    // invalidate every JWT in circulation as a side effect of `init --force`.
    // (API keys are verified against their own store and are unaffected.)
    if args.force && bearer_marker.exists() {
        fs::remove_file(&bearer_marker)
            .with_context(|| format!("suppression (--force) de {}", bearer_marker.display()))?;
    }
    let bearer = generate_admin_bearer(&args.root)?;

    // Server↔worker internal-API tokens (v2.0.0). Two independent 256-bit CSPRNG
    // secrets: the worker `token` and the operator `admin_token`. They are written
    // inline into `[internal_api]` so `gradatum-server` actually spawns its loopback
    // listener (main.rs: `internal_api_token.is_some() || admin_api_token.is_some()`).
    // Both satisfy `validate_internal_token` (64 hex chars ≥ 32 min).
    let internal_token = random_token_hex();
    let admin_token = random_token_hex();
    let effective_internal_token =
        write_or_merge_server_toml(&args.root, &args.bind, &internal_token, &admin_token)?;

    // Mirror the `.apikey.txt` gesture: the worker token is copied to a dedicated
    // 0600 file the operator reads back and passes via `GRADATUM_INTERNAL_TOKEN`.
    // On a re-init that PRESERVED an existing `[internal_api].token` (backup-authoritative
    // merge), the EFFECTIVE token actually written to server.toml is recorded — never the
    // freshly generated candidate — so the file and server.toml can never disagree.
    let internal_token_file = internal_token_path(&args.root);
    write_secret_file(&internal_token_file, &effective_internal_token)?;

    if !args.non_interactive {
        println!(
            "\nAdmin bearer (saved in {}, shown ONCE ONLY):\n  {}",
            bearer_marker.display(),
            bearer
        );
        println!(
            "\nInternal worker token (saved in {}, shown ONCE ONLY):\n  {}\n  \
             → set GRADATUM_INTERNAL_TOKEN to this value for gradatum-worker",
            internal_token_file.display(),
            effective_internal_token
        );
    }

    Ok(())
}

/// The bootstrap identity every Gradatum root must carry an active key for (R4).
///
/// Hard-coded on purpose: it is the orchestrator identity `gradatum-server` resolves
/// from the credential. An installation without an active key for it has no identity to
/// serve and is unusable — so `init` refuses to leave a root in that state.
const BOOTSTRAP_IDENTITY: &str = "main-agent";

/// Scopes minted for the bootstrap key — the orchestrator's working credential.
///
/// Mirrors the documented provisioning command (`docs/UPGRADING-1.0.0-to-2.0.0.md`):
/// read + search + write on the vault. Contains a write scope, so the key is a full
/// working credential rather than a read-only one.
const BOOTSTRAP_SCOPES: &[&str] = &["vault_read", "vault_search", "vault_write", "write"];

/// Path of the file recording the bootstrap key secret (mode 0600).
///
/// Same contract as `config/admin.bearer.txt`: a 0600 file, written once, that the
/// operator copies into the `Authorization` header of the `main-agent` MCP config.
fn bootstrap_secret_path(root: &Path) -> PathBuf {
    root.join("config/main-agent.apikey.txt")
}

/// Mints the mandatory `main-agent` API key (R4).
///
/// Three guarantees, in order:
/// 1. **Referential integrity** — the identity must be declared in the materialised
///    preset (`config/bearer.toml`), otherwise the key would authenticate and then be
///    denied on every locus, indistinguishable from an outage. An unreadable or
///    unparseable preset is a refusal, not a pass (the server itself falls back to
///    DENY-ALL in that case).
/// 2. **Idempotence / R1** — if an active `main-agent` key already exists (e.g.
///    `init --force` on a provisioned root), the mint is skipped: one identity, one
///    active key.
/// 3. **Secret hygiene** — on mint, the secret is written to a 0600 file and, in
///    interactive mode only, printed exactly once. It is NEVER logged, at any level.
///
/// # Errors
/// Returns an error if the preset does not declare `main-agent`, if it cannot be read or
/// parsed, or if the api-key store operation fails.
async fn mint_bootstrap_key(root: &Path, non_interactive: bool) -> Result<()> {
    // `main-agent` is a compile-time constant satisfying the AgentId charset
    // ([a-z0-9-], non-empty, ≤ 64 bytes, no leading/trailing hyphen); parsing it cannot
    // fail — the invariant is provable at the call site.
    let owner = AgentId::parse(BOOTSTRAP_IDENTITY)
        .expect("BOOTSTRAP_IDENTITY 'main-agent' is a valid AgentId by construction");

    // 1. Referential integrity against the freshly materialised preset.
    let preset_path = root.join("config/bearer.toml");
    let preset = fs::read_to_string(&preset_path).with_context(|| {
        format!(
            "cannot read the ACL preset {} to validate the '{BOOTSTRAP_IDENTITY}' identity",
            preset_path.display()
        )
    })?;
    let engine = AclEngine::from_preset_str(&preset).map_err(|e| {
        anyhow!(
            "the ACL preset {} does not parse: {e}",
            preset_path.display()
        )
    })?;
    if !engine.has_identity(&owner) {
        return Err(anyhow!(
            "the ACL preset {} does not declare the mandatory bootstrap identity \
             '{BOOTSTRAP_IDENTITY}'.\n\
             \n\
             v2.0.0 requires an active '{BOOTSTRAP_IDENTITY}' key at installation; without a \
             `[[consumer]] identity = \"{BOOTSTRAP_IDENTITY}\"` block the key would authenticate \
             and be denied on every locus.\n\
             Add that block to the preset (the default `hierarchical` preset already declares it) \
             and re-run `gradatum-admin init`.",
            preset_path.display()
        ));
    }

    // 2. Idempotence / R1 — skip if an active main-agent key already exists.
    let db_path = root.join("db/api_keys.sqlite");
    let store = SqliteApiKeyStore::init(&db_path)
        .await
        .map_err(|e| anyhow!("opening the api_keys store {}: {e}", db_path.display()))?;
    let existing = store
        .list(false, None)
        .await
        .map_err(|e| anyhow!("listing api keys before mint: {e}"))?;
    if existing
        .iter()
        .any(|k| k.owner.as_str() == BOOTSTRAP_IDENTITY)
    {
        tracing::info!(
            owner = BOOTSTRAP_IDENTITY,
            "active bootstrap key already present — mint skipped (R1)"
        );
        return Ok(());
    }

    // 3. Mint + record the secret (0600), never logged.
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| (*s).to_owned()).collect();
    let material = store
        .create(
            &owner,
            scopes,
            "main".to_owned(),
            Some("bootstrap identity (gradatum-admin init, R4)".to_owned()),
        )
        .await
        .map_err(|e| anyhow!("minting the '{BOOTSTRAP_IDENTITY}' bootstrap key: {e}"))?;

    let secret_path = write_bootstrap_secret(root, &material.secret)?;

    if !non_interactive {
        println!(
            "\nmain-agent API key (saved in {}, shown ONCE ONLY):\n  {}",
            secret_path.display(),
            material.secret
        );
    }

    Ok(())
}

/// Writes the bootstrap key secret to a 0600 file and returns its path.
///
/// Mirrors [`generate_admin_bearer`]: atomic 0600 creation with no world-readable
/// window. A stale file from a previous failed attempt is removed first — the mint only
/// runs when no active key exists, so an existing file is never authoritative.
fn write_bootstrap_secret(root: &Path, secret: &str) -> Result<PathBuf> {
    let path = bootstrap_secret_path(root);
    write_secret_file(&path, secret)?;
    Ok(path)
}

/// Path recording the worker's internal-API token (mode 0600).
///
/// Same contract as [`bootstrap_secret_path`]: a 0600 file, written once, whose content
/// the operator copies into `GRADATUM_INTERNAL_TOKEN` for `gradatum-worker`.
fn internal_token_path(root: &Path) -> PathBuf {
    root.join("config/internal-worker.token.txt")
}

/// Writes `secret` to a fresh 0600 file at `path` (atomic `O_EXCL` create, no
/// world-readable window).
///
/// A stale file from a previous failed attempt is removed first — callers only invoke
/// this when no authoritative secret already exists at `path`. The secret is never
/// logged.
///
/// Shared primitive behind [`write_bootstrap_secret`] and the internal-token file so all
/// operator secret files carry identical permissions and write semantics.
fn write_secret_file(path: &Path, secret: &str) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("removing stale secret {}", path.display()))?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("exclusive creation of {}", path.display()))?
        .write_all(secret.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Generates a 256-bit CSPRNG secret, hex-encoded (64 chars).
///
/// 32 random bytes = 256 bits of entropy — well above `MIN_INTERNAL_TOKEN_LEN` (32
/// chars), so the output always satisfies `validate_internal_token`. Shared by the admin
/// bearer and both internal-API tokens so every minted secret has identical strength.
fn random_token_hex() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Creates the `{md,db,config}` subdirectories with mode 0750.
fn create_layout(root: &Path) -> Result<()> {
    for sub in ["md", "db", "config"] {
        let p = root.join(sub);
        fs::create_dir_all(&p).with_context(|| format!("creating directory {}", p.display()))?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o750))
            .with_context(|| format!("chmod 0750 on {}", p.display()))?;
    }
    Ok(())
}

// NOTE: `generate_jwt_keys` used to live here. It wrote a PKCS#8/SPKI PEM pair
// (`config/jwt.private.pem` 0600 + `config/jwt.public.pem` 0644) that no runtime
// component ever read: `gradatum-server` signs and verifies with the raw Ed25519
// seed `config/jwt-signing-key.secret` (`gradatum_auth::key_store`), which it
// creates itself at first boot. Scaffolding a key pair the runtime ignores made
// operators back up and rotate the wrong files. The server now owns the key's
// entire lifecycle; `init` no longer touches it.

/// Generates an admin bearer (32 CSPRNG bytes, hex-encoded) and writes it to
/// `config/admin.bearer.txt` with mode 0600.
///
/// Returns the cleartext bearer (to be printed once in interactive mode).
fn generate_admin_bearer(root: &Path) -> Result<String> {
    let bearer = random_token_hex();

    let path = root.join("config/admin.bearer.txt");
    // Atomic write: O_EXCL + mode 0o600 at creation — no world-readable window.
    // If the file already exists, `create_new` fails → already initialized.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| {
            format!(
                "exclusive-create open of {} (already initialized?)",
                path.display()
            )
        })?
        .write_all(bearer.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;

    Ok(bearer)
}

/// Presets embedded in the binary (compiled via `include_str!`).
/// Allows calling `gradatum-admin init --preset <name>` from any working directory.
const PRESET_HIERARCHICAL: &str = include_str!("../presets/hierarchical.toml");
const PRESET_FLAT: &str = include_str!("../presets/flat.toml");

/// Resolves a preset by its short name or by an absolute / relative filesystem path containing `/`.
///
/// Detection rule:
/// - Contains `/` → read from the filesystem (explicit absolute or relative path).
/// - Otherwise → lookup in the embedded map (`hierarchical`, `flat`).
///
/// Returns an error if the short name is unknown or if the file is unreadable.
fn resolve_preset(preset: &str) -> Result<String> {
    if preset.contains('/') {
        // Explicit filesystem path (absolute or relative with directory component).
        fs::read_to_string(preset)
            .with_context(|| format!("reading the preset from file '{preset}'"))
    } else {
        match preset {
            "hierarchical" => Ok(PRESET_HIERARCHICAL.to_owned()),
            "flat" => Ok(PRESET_FLAT.to_owned()),
            other => Err(anyhow!(
                "unknown preset: '{other}'. \
                 Available embedded presets: hierarchical, flat. \
                 For a custom preset, pass a path containing '/' \
                 (e.g. --preset /etc/gradatum/my-preset.toml)"
            )),
        }
    }
}

/// Loads the preset (embedded or filesystem), substitutes `${PROJECTS}`, `${AGENT}`,
/// and `${THEME}` template variables, then writes the result to `config/bearer.toml` 0640.
///
/// The embedded preset is resolved independently of the working directory, so
/// `gradatum-admin init --preset hierarchical` works from any directory.
///
/// If `bearer.toml` already exists, an atomic backup `.bak.<ISO-TS>` is created
/// before writing. Manual customizations can be recovered from the backup file.
pub fn materialize_preset(root: &Path, preset: &str, projects: Option<&str>) -> Result<()> {
    let template = resolve_preset(preset)?;

    let projects_list = projects.unwrap_or("main");
    // Substitute template variables.
    // ${PROJECTS} → project list, ${AGENT}/${THEME} → wildcard defaults.
    let materialized = template
        .replace("${PROJECTS}", projects_list)
        .replace("${AGENT}", "*")
        .replace("${THEME}", "*");

    let bearer_toml = root.join("config/bearer.toml");

    // Atomic backup if the file exists — consistent pattern with server.toml handling.
    if bearer_toml.exists() {
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let backup = bearer_toml.with_extension(format!("toml.bak.{ts}"));
        fs::copy(&bearer_toml, &backup)
            .with_context(|| format!("backup {} → {}", bearer_toml.display(), backup.display()))?;
        tracing::info!(backup = %backup.display(), "bearer.toml backed up before overwrite");
    }

    fs::write(&bearer_toml, materialized.as_bytes())
        .with_context(|| format!("writing {}", bearer_toml.display()))?;
    fs::set_permissions(&bearer_toml, fs::Permissions::from_mode(0o640))
        .with_context(|| format!("chmod 0640 on {}", bearer_toml.display()))?;

    Ok(())
}

/// Generates the `server.toml` template content with default values.
///
/// Deterministic given its inputs (no side effects): the two internal-API tokens are
/// generated by the caller and threaded in, so the CSPRNG lives in `run` alone and this
/// function stays pure and testable.
///
/// Returns the raw `String` without writing — allows reuse in
/// `write_or_merge_server_toml` and in integration tests.
///
/// - `[storage].vault_index_path` — canonical path of the full-text SQLite index, honoured
///   by both the server and the worker through `gradatum_core::paths::vault_index_path`.
///   The deprecated alias `db_path` is still accepted when reading, for backward
///   compatibility.
/// - `jwt_ttl_human_secs = 3600`  (1 hour)
/// - `jwt_ttl_service_secs = 86400` (24 hours)
/// - `revocation_store = "sqlite"`
/// - `api_keys_db_path = "{root}/db/api_keys.sqlite"`
/// - `[embed]` — HTTP embedder enabled by default (endpoint `:8436`).
/// - `[curator]` + `[curator.llm]` — LLM curator enabled by default, pointing at the chat
///   endpoint (`base_url = http://localhost:8000`, model `Qwen3-4B-Instruct-2507`). Emitted
///   active symmetrically to `[embed]`: both are the operational minimum served by the
///   default Docker stack. `base_url` is the endpoint root (the client appends
///   `/v1/chat/completions`).
/// - `[internal_api]` — `bind = 127.0.0.1:19092` (matches `InternalApiConfig::default`),
///   `token` = `internal_token` (worker), `admin_token` = `admin_token` (operator).
///   Both MUST already satisfy `validate_internal_token` (≥ 32 chars); the caller
///   guarantees this via `random_token_hex`.
pub fn generate_server_toml_template(
    root: &Path,
    bind: &str,
    internal_token: &str,
    admin_token: &str,
) -> String {
    let root_str = root.display();
    format!(
        r#"# Generated by `gradatum-admin init` — edit with caution.

[server]
bind = "{bind}"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "{root_str}"
vault_index_path = "{root_str}/vault/.gradatum/index.db"

[auth]
# The JWT signing key is not configurable: gradatum-server creates and reads
# {root_str}/config/jwt-signing-key.secret (mode 0600). Back up THAT file —
# losing it invalidates every JWT in circulation (API keys are unaffected:
# revoke them through the api-key store).
jwt_ttl_human_secs = 3600
jwt_ttl_service_secs = 86400
revocation_store = "sqlite"
revocation_db_path = "{root_str}/db/revocation.sqlite"
api_keys_db_path = "{root_str}/db/api_keys.sqlite"

[acl]
preset_path = "{root_str}/config/bearer.toml"

[log]
format = "json"

[embed]
# Embedder HTTP.
# Enables async embedding generation via gradatum-worker → POST HTTP endpoint.
# Without this section, the worker starts with embedder=None and silently skips embed_note jobs.
# Default values — override in server.toml for your deployment.
enabled = true
endpoint = "http://localhost:8436/v1/embeddings"
model = "bge-m3-Q8_0"
dim = 1024
timeout_ms = 5000

[curator]
# Curator classification LLM. Emitted ACTIVE by default, symmetrically to [embed]:
# both are the operational minimum of a Gradatum install (embed AND curator), and the
# default Docker stack serves the chat model on :8000 (llama-chat, docker-compose.yml).
# Without this section the worker curates in pure heuristic mode and NEVER calls the model,
# so a chat server would run warm but unconsumed.
#
# On an install that serves no chat endpoint, bring the stack up with `--no-deps` and either
# point [curator.llm] base_url at your own endpoint or set `backend = "heuristic"` here.
# Thresholds below reproduce the values in production use.
backend = "openai_compat"
llm_review_enabled = true
heuristic_admit_threshold = 0.8
confidence_threshold = 0.7
llm_review_fallback = "pending-review-fallback"
# Arbitrated value (L-01): 128 tokens balances a long duplicate_hint against waste
# (64 too short, 256 wasteful). See examples/configs/curator.toml.
llm_review_max_tokens = 128

[curator.llm]
# `base_url` is the endpoint ROOT — the OpenAI-compatible client appends
# `/v1/chat/completions` itself (gradatum_chat::OpenAiCompatBackend). Do NOT append `/v1`
# here, or the request path doubles into `/v1/v1/chat/completions` (404).
# `model` MUST match the served alias: llama-chat is started with
# `--alias Qwen3-4B-Instruct-2507` (the Instruct variant is required — a base model does
# not follow the classification instruction). No auth on the loopback endpoint.
backend = "openai_compat"
base_url = "http://localhost:8000"
model = "Qwen3-4B-Instruct-2507"
timeout_ms = 60000

[apalis.workers.curate]
# Curator worker sizing — read by gradatum-worker from this shared server.toml.
# These are the *compiled defaults* made explicit: without an [apalis.workers.*] table
# they stay invisible to whoever reads their configuration (verified on our own deploy).
# - concurrency  : the curate worker is LLM-bound — 1 slot, serial. The default Docker
#                  chat server (llama-chat) runs with `--parallel 1`; extra slots would
#                  only queue on that single slot with no throughput gain.
# - timeout_secs : per-job ceiling. A classification call can take tens of seconds under
#                  load; 300s leaves margin above the 60s client timeout in [curator.llm].
# The other worker kinds (embed, reindex, purge, forget, distill, validate) keep their
# compiled defaults; override any of them with a matching [apalis.workers.<kind>] table.
concurrency = 1
timeout_secs = 300

[internal_api]
# Server↔worker loopback API (port 19092). WITHOUT this section gradatum-server never
# starts its internal listener, so gradatum-worker cannot reach it and all curation /
# embedding / distillation jobs stay silently idle (the public /health stays green).
# - `token`       : worker credential. Copy it into GRADATUM_INTERNAL_TOKEN for the worker
#                   (also saved to config/internal-worker.token.txt, mode 0600).
# - `admin_token` : operator credential for /internal/v1/admin/* (delete/restore/purge).
# Both are 256-bit CSPRNG secrets minted by `gradatum-admin init`. Rotate with
# `openssl rand -hex 32`; never commit or log them.
bind = "127.0.0.1:19092"
token = "{internal_token}"
admin_token = "{admin_token}"
"#
    )
}

/// Rename migration table: `(old_path, new_path)`.
///
/// Used in `merge_user_config`: if a key from the new template is absent from
/// the backup, the table is checked for a matching old name present in the backup.
/// New entries are added per release; order is irrelevant (entries are independent).
const KEY_MIGRATIONS: &[(&str, &str)] = &[
    // `storage.db_path` → `storage.vault_index_path`
    ("storage.db_path", "storage.vault_index_path"),
];

/// If `config/server.toml` exists, creates an atomic backup `.bak.<ISO-TS>` and
/// merges user values according to the new template schema.
/// Otherwise, writes the template as-is.
///
/// Merge semantics:
/// - Key in backup **and** in new template → backup value is preserved.
/// - Key only in new template → template default value is used.
/// - Key only in backup → discarded (intentionally removed from the new schema).
/// - Renames via `KEY_MIGRATIONS`: old value is injected into the new path.
///
/// `internal_token` / `admin_token` are the freshly minted internal-API secrets threaded
/// into the template. On a **fresh** install they land verbatim in `server.toml`. On a
/// **re-init that already had** an `[internal_api].token`, the backup-authoritative merge
/// KEEPS the existing one — so the candidate is discarded. Either way the function returns
/// the **effective** worker token actually written, which the caller records in the 0600
/// side-file (guaranteeing file ↔ server.toml agreement).
///
/// `server.toml` is written 0640, not 0600: `gradatum-worker` also reads it (`[apalis]` /
/// `[embed]` / `[curator]`), so a deployment where the worker runs under a different user
/// than the server needs group read. 0640 already denies world access; the raw worker
/// secret additionally lives in the dedicated 0600 side-file.
///
/// # Errors
/// Propagates I/O errors, merge failures, and returns an error if the written content
/// unexpectedly lacks `[internal_api].token`.
fn write_or_merge_server_toml(
    root: &Path,
    bind: &str,
    internal_token: &str,
    admin_token: &str,
) -> Result<String> {
    let p = root.join("config/server.toml");
    let new_content = generate_server_toml_template(root, bind, internal_token, admin_token);

    let final_content = if p.exists() {
        // Atomic backup
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let backup = p.with_extension(format!("toml.bak.{ts}"));
        fs::copy(&p, &backup)
            .with_context(|| format!("backup {} → {}", p.display(), backup.display()))?;
        tracing::info!(backup = %backup.display(), "server.toml backed up before re-init");

        // Merge user values
        let existing =
            fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        merge_user_config(&existing, &new_content)?
    } else {
        new_content
    };

    fs::write(&p, final_content.as_bytes()).with_context(|| format!("writing {}", p.display()))?;
    fs::set_permissions(&p, fs::Permissions::from_mode(0o640))
        .with_context(|| format!("chmod 0640 on {}", p.display()))?;

    extract_internal_token(&final_content)
}

/// Extracts `[internal_api].token` from a rendered `server.toml`.
///
/// The single source of truth for the effective worker token: after the
/// backup-authoritative merge, the value in the written file may differ from the freshly
/// minted candidate, so it is read back rather than assumed.
///
/// # Errors
/// Returns an error if the content does not parse or has no `[internal_api].token` string.
fn extract_internal_token(content: &str) -> Result<String> {
    let doc: toml_edit::DocumentMut = content
        .parse()
        .context("re-parsing generated server.toml to read [internal_api].token")?;
    doc.get("internal_api")
        .and_then(|it| it.get("token"))
        .and_then(toml_edit::Item::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("generated server.toml has no [internal_api].token string"))
}

/// Structural merge: the BACKUP is authoritative for all user content.
///
/// Semantics:
/// - The BACKUP is authoritative for all its keys (custom sections, extension sections,
///   customized keys). Each backup key is applied onto the result.
/// - The NEW template contributes keys/sections absent from the backup
///   (these keys keep their template default values).
/// - `KEY_MIGRATIONS` are applied as a pre-processing step on a copy of the backup
///   (renaming old keys before the walk, to avoid inserting them verbatim).
///
/// Returns the `String` content ready to write, preserving TOML comments
/// via `toml_edit::DocumentMut`.
pub fn merge_user_config(existing: &str, new_template: &str) -> Result<String> {
    use toml_edit::DocumentMut;

    let existing_doc: DocumentMut = existing
        .parse()
        .context("parse server.toml existant (backup)")?;
    let mut result: DocumentMut = new_template
        .parse()
        .context("parse nouveau template server.toml")?;

    // 1. Apply rename migrations on a copy of the backup (pre-walk).
    //    Rename old_path → new_path in the backup so the walk sees
    //    the canonical key directly and does not reinsert the old one.
    let mut backup_migrated = existing_doc.clone();
    for (old_path, new_path) in KEY_MIGRATIONS {
        if let Some(old_item) = lookup_item(backup_migrated.as_table(), old_path) {
            let val = old_item.clone();
            // Inject into new_path (always present in the migrated backup if the section exists,
            // or will be inserted by the walk into the result table).
            // Inserted directly into backup_migrated so the walk processes it.
            set_item_or_insert(backup_migrated.as_table_mut(), new_path, val);
            remove_path(backup_migrated.as_table_mut(), old_path);
            tracing::info!(
                old = %old_path,
                new = %new_path,
                "merge server.toml — rename migration applied pre-walk"
            );
        }
    }

    let mut preserved = 0usize;
    let mut new_keys = 0usize;
    let mut user_added = 0usize;

    // 2. Walk with migrated backup: BACKUP is authoritative.
    walk_and_merge(
        result.as_table_mut(),
        backup_migrated.as_table(),
        "",
        &mut preserved,
        &mut new_keys,
        &mut user_added,
    );

    tracing::info!(
        preserved,
        new_keys,
        user_added,
        "merge server.toml — values preserved + new keys with defaults + user extensions"
    );

    Ok(result.to_string())
}

/// Recursively walks the keys of `source` (BACKUP) and applies them onto `target` (NEW).
///
/// For each backup key:
/// - Present in target AND both are Tables → recurse
/// - Present in target (scalar or array) → overwrite target with backup value
/// - Absent from target → INSERT (preserves sections/keys present only in the backup)
///
/// Keys in target absent from the backup retain their NEW default values.
fn walk_and_merge(
    target: &mut toml_edit::Table,
    source: &toml_edit::Table,
    path_prefix: &str,
    preserved: &mut usize,
    new_keys: &mut usize,
    user_added: &mut usize,
) {
    // Iterate over BACKUP (source) keys — BACKUP-authoritative semantics
    let source_keys: Vec<String> = source.iter().map(|(k, _)| k.to_string()).collect();

    for key in &source_keys {
        let full_path = if path_prefix.is_empty() {
            key.clone()
        } else {
            format!("{path_prefix}.{key}")
        };

        let source_item = match source.get(key.as_str()) {
            Some(it) => it.clone(),
            None => continue,
        };

        match target.get_mut(key.as_str()) {
            Some(target_item)
                if matches!(target_item, toml_edit::Item::Table(_))
                    && matches!(source_item, toml_edit::Item::Table(_)) =>
            {
                // Les deux sont des Tables → récursion
                if let (toml_edit::Item::Table(t_target), toml_edit::Item::Table(t_source)) =
                    (target_item, &source_item)
                {
                    walk_and_merge(
                        t_target, t_source, &full_path, preserved, new_keys, user_added,
                    );
                }
            }
            Some(target_item) => {
                // Scalar or array in target → overwrite with backup value
                *target_item = source_item;
                *preserved += 1;
            }
            None => {
                // Key absent from the NEW template → insert from backup
                // (user extension section/key)
                target.insert(key.as_str(), source_item);
                *user_added += 1;
                tracing::info!(path = %full_path, "merge: section/key preserved (user extension)");
            }
        }
    }

    // Count NEW keys absent from the backup (already in target with default value)
    for (key, _) in target.iter() {
        if !source.contains_key(key) {
            *new_keys += 1;
        }
    }
}

/// Immutable lookup of an item by path `"section.key.subkey"`.
fn lookup_item<'a>(table: &'a toml_edit::Table, path: &str) -> Option<&'a toml_edit::Item> {
    let mut parts = path.splitn(2, '.');
    let head = parts.next()?;
    let rest = parts.next();

    match (table.get(head), rest) {
        (Some(item), None) => Some(item),
        (Some(toml_edit::Item::Table(sub)), Some(tail)) => lookup_item(sub, tail),
        _ => None,
    }
}

/// Injects `value` at `path` into `table` (path format: `"section.key"`).
///
/// Unlike `set_item`, inserts the intermediate node (section table) if absent.
/// Used in `KEY_MIGRATIONS` pre-walk to inject the migrated value even when
/// the `[storage]` section does not yet contain the new key name.
///
/// Note: supports only two-level paths (`section.key`) — sufficient for
/// current entries in `KEY_MIGRATIONS`.
fn set_item_or_insert(table: &mut toml_edit::Table, path: &str, value: toml_edit::Item) {
    let mut parts = path.splitn(2, '.');
    let head = match parts.next() {
        Some(h) => h,
        None => return,
    };
    let tail = parts.next();

    match tail {
        None => {
            // Top-level key
            table.insert(head, value);
        }
        Some(key) => {
            // Ensure the sub-table exists
            if table.get(head).is_none() {
                table.insert(head, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            if let Some(toml_edit::Item::Table(sub)) = table.get_mut(head) {
                sub.insert(key, value);
            }
        }
    }
}

/// Removes the key at `path` (`"section.key"`) from `table`.
///
/// Silent if the path does not exist.
/// Note: supports only two-level paths — sufficient for `KEY_MIGRATIONS`.
fn remove_path(table: &mut toml_edit::Table, path: &str) {
    let mut parts = path.splitn(2, '.');
    let head = match parts.next() {
        Some(h) => h,
        None => return,
    };
    let tail = parts.next();

    match tail {
        None => {
            table.remove(head);
        }
        Some(key) => {
            if let Some(toml_edit::Item::Table(sub)) = table.get_mut(head) {
                sub.remove(key);
            }
        }
    }
}

/// Validates that `server.toml` content is parseable as valid TOML.
///
/// Exposed as `pub(crate)` for smoke tests only.
#[allow(dead_code)]
pub(crate) fn validate_server_toml(content: &str) -> Result<()> {
    content
        .parse::<toml_edit::DocumentMut>()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("invalid server.toml: {e}"))
}

/// Initializes `db/queue.sqlite`, `db/revocation.sqlite`, and `db/api_keys.sqlite` in WAL mode.
///
/// - `queue.sqlite`      : `jobs` table + index (schema `gradatum-queue`)
/// - `revocation.sqlite` : `revoked` table (schema `gradatum-auth::SqliteRevocationStore`)
/// - `api_keys.sqlite`   : `api_keys` table + index (schema `gradatum-acl-auth`)
///
/// All operations are idempotent (`CREATE TABLE IF NOT EXISTS`).
fn init_sqlite_dbs(root: &Path) -> Result<()> {
    // --- queue.sqlite ---
    // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
    let queue_path = queue_db_path(root);
    let conn = Connection::open(&queue_path)
        .with_context(|| format!("ouverture de {}", queue_path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS jobs (
             id           TEXT    PRIMARY KEY,
             kind         TEXT    NOT NULL,
             payload_json TEXT    NOT NULL,
             status       TEXT    NOT NULL,
             lease_until  INTEGER,
             created_at   INTEGER NOT NULL,
             updated_at   INTEGER NOT NULL,
             attempts     INTEGER NOT NULL DEFAULT 0,
             last_error   TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_jobs_status_lease ON jobs(status, lease_until);",
    )
    .with_context(|| format!("initialisation de {}", queue_path.display()))?;

    // --- revocation.sqlite ---
    let revoc_path = root.join("db/revocation.sqlite");
    let conn = Connection::open(&revoc_path)
        .with_context(|| format!("ouverture de {}", revoc_path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS revoked (
             jti        TEXT    PRIMARY KEY,
             exp        INTEGER NOT NULL,
             revoked_at INTEGER NOT NULL
         );",
    )
    .with_context(|| format!("initialisation de {}", revoc_path.display()))?;

    // --- api_keys.sqlite ---
    //
    // Initialized via rusqlite directly (consistent with the other DBs initialized in this
    // sync function). The DB will be reopened by SqliteApiKeyStore via rusqlite at runtime,
    // which then records the migration in `_sqlx_migrations` (honored, never replayed).
    // Schema identical to `gradatum-acl-auth/migrations/V0001__create_api_keys.sql`.
    let api_keys_path = root.join("db/api_keys.sqlite");
    let conn = Connection::open(&api_keys_path)
        .with_context(|| format!("ouverture de {}", api_keys_path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS api_keys (
             id              TEXT    PRIMARY KEY,
             prefix          TEXT    NOT NULL UNIQUE,
             hash            TEXT    NOT NULL,
             owner           TEXT    NOT NULL,
             scopes_json     TEXT    NOT NULL,
             tenant_id       TEXT    NOT NULL,
             created_at      INTEGER NOT NULL,
             last_used_at    INTEGER,
             revoked_at      INTEGER,
             description     TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_api_keys_owner ON api_keys(owner) WHERE revoked_at IS NULL;
         CREATE INDEX IF NOT EXISTS idx_api_keys_prefix ON api_keys(prefix);",
    )
    .with_context(|| format!("initialisation de {}", api_keys_path.display()))?;

    // Warn if the table already has rows (re-init is non-destructive).
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))
        .unwrap_or(0);
    if count > 0 {
        tracing::warn!(
            rows = count,
            "api_keys table exists with {} rows — non-destructive re-init",
            count
        );
    }

    Ok(())
}

#[cfg(test)]
mod internal_api_tests {
    use super::*;

    /// Minimum length enforced by `gradatum_server::config::validate_internal_token`.
    ///
    /// Duplicated as a literal on purpose: `gradatum-server` is not a dependency of
    /// `gradatum-admin` and pulling it in just for a constant is not worth the coupling.
    /// That validator's ONLY constraint is `token.len() >= 32`, so asserting `>= MIN`
    /// here is equivalent to "this token would pass `validate_internal_token`".
    const MIN_INTERNAL_TOKEN_LEN: usize = 32;

    /// Reads a string value from the `[internal_api]` section of rendered TOML.
    fn internal_field(content: &str, key: &str) -> String {
        let doc: toml_edit::DocumentMut = content.parse().expect("template parses as TOML");
        doc.get("internal_api")
            .and_then(|it| it.get(key))
            .and_then(toml_edit::Item::as_str)
            .unwrap_or_else(|| panic!("[internal_api].{key} missing or not a string"))
            .to_owned()
    }

    /// The generated template carries an `[internal_api]` section whose two tokens are
    /// distinct, threaded verbatim, and long enough to pass `validate_internal_token`.
    #[test]
    fn template_embeds_two_distinct_valid_internal_tokens() {
        let worker = random_token_hex();
        let admin = random_token_hex();
        let content = generate_server_toml_template(
            Path::new("/var/lib/gradatum"),
            "127.0.0.1:19090",
            &worker,
            &admin,
        );

        assert!(
            content.contains("[internal_api]"),
            "template must contain an [internal_api] section: {content}"
        );

        let got_worker = internal_field(&content, "token");
        let got_admin = internal_field(&content, "admin_token");

        assert_eq!(got_worker, worker, "worker token must be threaded verbatim");
        assert_eq!(got_admin, admin, "admin token must be threaded verbatim");
        assert_ne!(
            got_worker, got_admin,
            "worker and admin tokens must be distinct"
        );
        assert!(
            got_worker.len() >= MIN_INTERNAL_TOKEN_LEN,
            "worker token too short ({}) to pass validate_internal_token",
            got_worker.len()
        );
        assert!(
            got_admin.len() >= MIN_INTERNAL_TOKEN_LEN,
            "admin token too short ({}) to pass validate_internal_token",
            got_admin.len()
        );
    }

    /// The template emits an ACTIVE `[curator]` + `[curator.llm]` (symmetric to `[embed]`),
    /// with `backend = "openai_compat"`, the mandated Instruct model, and a `base_url` that is
    /// the endpoint ROOT — no `/v1` (the client appends `/v1/chat/completions`, so a trailing
    /// `/v1` here would double the path into a 404). Guards the regression where curation
    /// silently stayed heuristic because no `[curator]` section was written.
    #[test]
    fn template_emits_active_curator_llm_at_chat_endpoint() {
        let content = generate_server_toml_template(
            Path::new("/var/lib/gradatum"),
            "127.0.0.1:19090",
            &random_token_hex(),
            &random_token_hex(),
        );
        let doc: toml_edit::DocumentMut = content.parse().expect("template parses as TOML");

        let curator = doc.get("curator").expect("[curator] section present");
        assert_eq!(
            curator.get("backend").and_then(toml_edit::Item::as_str),
            Some("openai_compat"),
            "curator backend must be openai_compat (LLM mode), not heuristic"
        );
        assert_eq!(
            curator
                .get("llm_review_enabled")
                .and_then(toml_edit::Item::as_bool),
            Some(true)
        );
        assert_eq!(
            curator
                .get("heuristic_admit_threshold")
                .and_then(toml_edit::Item::as_float),
            Some(0.8)
        );
        assert_eq!(
            curator
                .get("confidence_threshold")
                .and_then(toml_edit::Item::as_float),
            Some(0.7)
        );
        assert_eq!(
            curator
                .get("llm_review_max_tokens")
                .and_then(toml_edit::Item::as_integer),
            Some(128),
            "llm_review_max_tokens must be the arbitrated value 128 (L-01), not 1024"
        );

        let llm = curator
            .get("llm")
            .expect("[curator.llm] sub-section present");
        let base_url = llm
            .get("base_url")
            .and_then(toml_edit::Item::as_str)
            .expect("[curator.llm].base_url present");
        assert_eq!(base_url, "http://localhost:8000");
        assert!(
            !base_url.contains("/v1"),
            "base_url must be the endpoint ROOT: the client appends /v1/chat/completions ({base_url})"
        );
        assert_eq!(
            llm.get("model").and_then(toml_edit::Item::as_str),
            Some("Qwen3-4B-Instruct-2507"),
            "the Instruct variant is mandated"
        );
    }

    /// The template emits an explicit `[apalis.workers.curate]` table carrying the curator
    /// worker's compiled defaults (concurrency 1, timeout_secs 300), so these settings are
    /// visible to whoever reads their configuration instead of being invisible compiled
    /// defaults. Guards the regression where no `[apalis.workers]` section was written.
    #[test]
    fn template_emits_explicit_curate_worker_sizing() {
        let content = generate_server_toml_template(
            Path::new("/var/lib/gradatum"),
            "127.0.0.1:19090",
            &random_token_hex(),
            &random_token_hex(),
        );
        let doc: toml_edit::DocumentMut = content.parse().expect("template parses as TOML");

        let curate = doc
            .get("apalis")
            .and_then(|a| a.get("workers"))
            .and_then(|w| w.get("curate"))
            .expect("[apalis.workers.curate] section present");
        assert_eq!(
            curate
                .get("concurrency")
                .and_then(toml_edit::Item::as_integer),
            Some(1),
            "curate worker must be serial (concurrency 1) to match the single-slot chat endpoint"
        );
        assert_eq!(
            curate
                .get("timeout_secs")
                .and_then(toml_edit::Item::as_integer),
            Some(300),
            "curate per-job timeout must leave margin above the 60s [curator.llm] client timeout"
        );
    }

    /// `[internal_api].bind` matches `InternalApiConfig::default` (127.0.0.1:19092) so the
    /// generated file is coherent with the server's default internal listener address.
    #[test]
    fn template_internal_api_bind_is_default_loopback() {
        let content = generate_server_toml_template(
            Path::new("/r"),
            "127.0.0.1:19090",
            &random_token_hex(),
            &random_token_hex(),
        );
        assert_eq!(internal_field(&content, "bind"), "127.0.0.1:19092");
    }

    /// `random_token_hex` yields fresh, 64-char, hex-only secrets — the property that makes
    /// two successive `init` runs mint different tokens.
    #[test]
    fn random_token_hex_is_fresh_and_well_formed() {
        let a = random_token_hex();
        let b = random_token_hex();
        assert_ne!(a, b, "two draws must differ (256-bit entropy)");
        assert_eq!(a.len(), 64, "32 bytes hex-encoded = 64 chars");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "token must be hex only: {a}"
        );
    }

    /// `extract_internal_token` reads back exactly what the template embedded, and errors
    /// (never panics) when the section is absent.
    #[test]
    fn extract_internal_token_roundtrips_and_errors_when_absent() {
        let worker = random_token_hex();
        let content = generate_server_toml_template(
            Path::new("/r"),
            "127.0.0.1:19090",
            &worker,
            &random_token_hex(),
        );
        assert_eq!(
            extract_internal_token(&content).expect("token present"),
            worker
        );

        let no_section = "[server]\nbind = \"127.0.0.1:19090\"\n";
        assert!(
            extract_internal_token(no_section).is_err(),
            "must return Err (not panic) when [internal_api].token is absent"
        );
    }
}
