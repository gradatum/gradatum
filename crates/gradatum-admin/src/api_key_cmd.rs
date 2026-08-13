//! `gradatum-admin api-key {create,list,revoke,rotate}` — API key lifecycle management.
//!
//! ## Sub-commands
//!
//! ```text
//! gradatum-admin api-key create --root /var/lib/gradatum --owner mcp-stub --scopes write [--tenant main] [--description "desc"]
//! gradatum-admin api-key create --root /var/lib/gradatum --owner reader --scopes vault_read --read-only
//! gradatum-admin api-key list   --root /var/lib/gradatum [--all]
//! gradatum-admin api-key revoke --root /var/lib/gradatum --prefix ak_abcdef01
//! gradatum-admin api-key rotate --root /var/lib/gradatum --prefix ak_abcdef01
//! gradatum-admin api-key reset  --root /var/lib/gradatum                         # aperçu
//! gradatum-admin api-key reset  --root /var/lib/gradatum --execute --confirm-prefixes "ak_..,ak_.."
//! ```
//!
//! ## Security
//! - The secret is printed to stdout ONCE on `create` and `rotate`
//! - The argon2id hash is never displayed
//! - The SQLite path is derived from `--root` (`<root>/db/api_keys.sqlite`)
//! - `create` refuses a scope set that grants no write access unless `--read-only`
//!   is passed, so it never mints a key whose scopes promise a capability the key
//!   does not have. The check runs on `create` AND `rotate` (A1-bis, trou ferme le
//!   2026-07-27). Keys already in the store are not revalidated at rest
//! - `create` likewise refuses an `--owner` that no `[[consumer]]` of the ACL preset
//!   declares, unless `--allow-unknown-identity` is passed. Same shape as the scope
//!   guard, same restriction to `create` — the boot-time reconciliation of
//!   `gradatum-server` covers the keys already persisted
//! - `create` also enforces R1 — one identity, one active key: it refuses to mint a
//!   second active key for an `--owner` that already carries one, and points the operator
//!   at `rotate` (which revokes and replaces atomically). A revoked key does not count,
//!   so a replacement can always be minted once the previous key is retired
//! - `reset` implements R6/R7 — a registry-wide wipe by REVOCATION (never row deletion,
//!   so the audit trail survives). Its scope is the key registry ALONE: it opens only
//!   `<root>/db/api_keys.sqlite` and never touches the vault. Confirmation follows the
//!   `vault forget` idiom — a dry-run preview, then execution requires echoing the exact
//!   active prefixes back via `--confirm-prefixes`. There is deliberately no `--yes`/
//!   `--force` flag: a blind boolean an alias could carry is exactly what the echo guards
//!   against. After a reset the registry has no active key, so the server answers 503
//!   (R5 empty-registry) until it is re-provisioned

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use gradatum_acl_auth::{ApiKey, ApiKeyStore, SqliteApiKeyStore, WRITE_SCOPES, has_write_scope};
use gradatum_acl_policy::AclEngine;
use gradatum_core::scope::AgentId;

/// Sub-commands of `api-key`.
#[derive(Debug, Subcommand)]
pub enum ApiKeyCmd {
    /// Creates a new API key.
    Create(ApiKeyCreateArgs),
    /// Lists existing API keys.
    List(ApiKeyListArgs),
    /// Revokes an API key by its prefix.
    Revoke(ApiKeyRevokeArgs),
    /// Revokes the existing key and atomically generates a replacement.
    Rotate(ApiKeyRotateArgs),
    /// Resets the registry — revokes EVERY active key (R6). Never touches the vault (R7).
    Reset(ApiKeyResetArgs),
}

/// Arguments for `api-key create`.
#[derive(Debug, Args)]
pub struct ApiKeyCreateArgs {
    /// Gradatum root directory.
    #[arg(long)]
    pub root: PathBuf,

    /// Key owner — the agent identity the key will carry (e.g. `mcp-stub`, `engine`).
    ///
    /// Parsed as an [`AgentId`] (charset `[a-z0-9-]`, non-empty, ≤ 64 bytes, no leading or
    /// trailing hyphen) and checked against the identities declared in the ACL preset,
    /// unless `--allow-unknown-identity` is passed.
    #[arg(long)]
    pub owner: String,

    /// Granted scopes, comma-separated. Required — a key's capabilities are always
    /// stated explicitly.
    ///
    /// Write access is granted only by one of `write`, `admin` or `service`. Any
    /// other value yields a key that cannot write, which is refused unless
    /// `--read-only` is also passed.
    ///
    /// This argument has no default on purpose: the only value a default could carry
    /// is a read scope, which `create` refuses on its own, so a default would advertise
    /// a value that cannot mint a key unless `--read-only` is also passed.
    #[arg(long)]
    pub scopes: String,

    /// Confirms that the key is meant to be read-only.
    ///
    /// Required whenever `--scopes` carries no write scope, so that a key without
    /// write access is always a deliberate choice rather than an oversight.
    #[arg(long, default_value_t = false)]
    pub read_only: bool,

    /// Target tenant.
    #[arg(long)]
    pub tenant: String,

    /// Optional description for the key.
    #[arg(long)]
    pub description: Option<String>,

    /// Allows a `--tenant` other than `main`, explicitly lifting the single-vault guard.
    ///
    /// The tenant must already be provisioned in the `tenant_vault_grants` allow-list
    /// for the key to be exchangeable for a JWT; otherwise `/auth/exchange` answers
    /// `403`.
    #[arg(long, default_value_t = false)]
    pub allow_non_main_tenant: bool,

    /// Mints the key even though `--owner` is not declared in the ACL preset.
    ///
    /// The escape hatch for the one legitimate ordering — provisioning a credential before
    /// its `[[consumer]]` block exists. It is deliberately explicit: without it, the guard
    /// would be bypassable only by disabling the check altogether, and an operator in a
    /// hurry would disable it permanently.
    ///
    /// A key created this way authenticates but is denied on every locus until the identity
    /// is declared. That is stated on stderr at creation time, and the boot reconciliation
    /// keeps reporting it until the preset catches up.
    #[arg(long, default_value_t = false)]
    pub allow_unknown_identity: bool,
}

/// Arguments for `api-key list`.
#[derive(Debug, Args)]
pub struct ApiKeyListArgs {
    /// Gradatum root directory.
    #[arg(long)]
    pub root: PathBuf,

    /// Includes revoked keys in the listing.
    #[arg(long)]
    pub all: bool,
}

/// Arguments for `api-key revoke`.
#[derive(Debug, Args)]
pub struct ApiKeyRevokeArgs {
    /// Gradatum root directory.
    #[arg(long)]
    pub root: PathBuf,

    /// Prefix of the key to revoke (e.g. `ak_abcdef01`).
    #[arg(long)]
    pub prefix: String,
}

/// Arguments for `api-key rotate`.
#[derive(Debug, Args)]
pub struct ApiKeyRotateArgs {
    /// Gradatum root directory.
    #[arg(long)]
    pub root: PathBuf,

    /// Prefix of the key to rotate (e.g. `ak_abcdef01`).
    #[arg(long)]
    pub prefix: String,
}

/// Arguments for `api-key reset`.
#[derive(Debug, Args)]
pub struct ApiKeyResetArgs {
    /// Gradatum root directory.
    #[arg(long)]
    pub root: PathBuf,

    /// Executes the reset. Without it, `reset` only previews (dry-run).
    #[arg(long, default_value_t = false)]
    pub execute: bool,

    /// Confirmation prefixes, comma-separated. Required with `--execute`.
    ///
    /// Must match EXACTLY the active-key prefixes returned by the preview — this
    /// echo-of-the-previewed-list is the ONLY confirmation. There is deliberately no
    /// `--yes`/`--force` flag: a blind boolean a script or alias could carry without a
    /// human ever reading the list is precisely what the echo guards against.
    #[arg(long, value_delimiter = ',')]
    pub confirm_prefixes: Vec<String>,
}

/// Entry point for the `api-key` sub-command.
pub async fn run(cmd: ApiKeyCmd) -> Result<()> {
    match cmd {
        ApiKeyCmd::Create(args) => run_create(args).await,
        ApiKeyCmd::List(args) => run_list(args).await,
        ApiKeyCmd::Revoke(args) => run_revoke(args).await,
        ApiKeyCmd::Rotate(args) => run_rotate(args).await,
        ApiKeyCmd::Reset(args) => run_reset(args).await,
    }
}

/// Resolves the `api_keys` database path as `{root}/db/api_keys.sqlite`.
///
/// Derivation from `root` is reliable and consistent with the `init` layout.
fn resolve_db_path(root: &std::path::Path) -> PathBuf {
    root.join("db/api_keys.sqlite")
}

/// Resolves the ACL preset path as `{root}/config/bearer.toml`.
///
/// Same derivation contract as [`resolve_db_path`]: `init` materialises the preset at that
/// exact path and writes it back into the server config as `acl.preset_path`, so `--root`
/// alone identifies both halves of the relation this guard checks.
fn resolve_preset_path(root: &std::path::Path) -> PathBuf {
    root.join("config/bearer.toml")
}

/// Parses `--owner` and refuses an identity that the ACL preset does not declare.
///
/// Two barriers, in order:
///
/// 1. **Parse-don't-validate** — `--owner` is untrusted CLI input, so it crosses into
///    [`AgentId`] here or not at all. This is the call site [`AgentId::parse`] was typed
///    for; before it existed, any byte sequence reached the `api_keys.owner` column.
/// 2. **Referential integrity** — `api_keys.owner` and `consumer.identity` are joined by
///    nothing but string equality. A key minted for an undeclared identity authenticates
///    (200 on `/auth/exchange`) and is then denied on every locus, silently — the exact
///    shape of the `engine` incident, which cost a day of investigation because the refusal
///    was indistinguishable from an outage. Catching it here costs one lookup, at the only
///    moment where the operator can still fix the typo.
///
/// The check runs on `create` only, mirroring [`validate_create_scopes`]: `rotate` carries
/// the source key's owner over unchanged, and keys already in the store are not revalidated.
/// Those are covered by the boot-time reconciliation instead.
///
/// An unreadable or absent preset is a refusal, not a pass: the server falls back to
/// DENY-ALL in that situation, so a key minted against an unknown preset is a key that
/// cannot work. `--allow-unknown-identity` lifts both the lookup and this case.
///
/// # Errors
/// Returns an error when `--owner` is not a well-formed [`AgentId`], when the preset cannot
/// be read or parsed, or when the identity is absent from it.
fn validate_create_owner(
    root: &std::path::Path,
    raw_owner: &str,
    allow_unknown_identity: bool,
) -> Result<AgentId> {
    let owner = AgentId::parse(raw_owner).map_err(|e| {
        anyhow::anyhow!(
            "invalid --owner: {e}\n\
             \n\
             An agent identity is lowercase `[a-z0-9-]`, non-empty, at most 64 bytes, and \
             carries no leading or trailing hyphen. It must match a `[[consumer]] identity` \
             of the ACL preset byte for byte."
        )
    })?;

    if allow_unknown_identity {
        eprintln!(
            "WARNING: --allow-unknown-identity — key minted for '{owner}', an identity the \
             preset does not declare.\n\
             It will authenticate and then be denied on every locus until a `[[consumer]] \
             identity = \"{owner}\"` block exists."
        );
        return Ok(owner);
    }

    let preset_path = resolve_preset_path(root);
    let preset = std::fs::read_to_string(&preset_path).map_err(|e| {
        anyhow::anyhow!(
            "cannot read the ACL preset {}: {e}\n\
             \n\
             Without it the identity of '{owner}' cannot be checked, and the server itself \
             falls back to DENY-ALL — the key would authenticate and be refused everywhere.\n\
             Run `gradatum-admin init` to materialise the preset, or pass \
             `--allow-unknown-identity` to mint the key anyway.",
            preset_path.display()
        )
    })?;
    let engine = AclEngine::from_preset_str(&preset).map_err(|e| {
        anyhow::anyhow!(
            "the ACL preset {} does not parse: {e}\n\
             \n\
             Fix the preset, or pass `--allow-unknown-identity` to mint the key anyway.",
            preset_path.display()
        )
    })?;

    if !engine.has_identity(&owner) {
        bail!(
            "refusing to create a key for an undeclared identity: '{owner}' has no \
             `[[consumer]]` block in {}\n\
             \n\
             Such a key authenticates and is then denied on every locus, which is \
             indistinguishable from an outage.\n\
             Declare `identity = \"{owner}\"` in the preset first, or pass \
             `--allow-unknown-identity` if the credential is meant to precede its ACL entry.",
            preset_path.display()
        );
    }

    Ok(owner)
}

/// Opens the SQLite API key store.
async fn open_store(root: &std::path::Path) -> Result<SqliteApiKeyStore> {
    let db_path = resolve_db_path(root);
    SqliteApiKeyStore::init(&db_path)
        .await
        .with_context(|| format!("opening the api_keys store: {}", db_path.display()))
}

/// Renders a scope set for display in an error message.
fn render_scopes(scopes: &[String]) -> String {
    if scopes.is_empty() {
        "(none)".to_owned()
    } else {
        scopes.join(", ")
    }
}

/// Rejects a scope set whose write capability does not match the caller's intent.
///
/// A key is refused when it carries no write scope and `--read-only` was not passed,
/// so that `create` never mints a key claiming a write capability it does not have.
/// The mirror case is refused too: `--read-only` combined with a write scope would
/// produce a writable key under a read-only label.
///
/// This guards creation AND rotation. [`run_rotate`] validates the source key's
/// scopes before the atomic rotate — same guard, same message, same fail-closed.
/// Keys already persisted are never revalidated at rest.
///
/// # Errors
/// Returns an error describing the mismatch and the command to run instead.
fn validate_create_scopes(scopes: &[String], read_only: bool) -> Result<()> {
    let writable = has_write_scope(scopes);
    let allowed = WRITE_SCOPES.join(", ");

    match (read_only, writable) {
        (false, false) => bail!(
            "refusing to create a key that cannot write: scopes [{}] grant no write access\n\
             \n\
             Write access is granted only by these exact scopes: {}.\n\
             Re-run with `--scopes write` for a writable key, or add `--read-only` to \
             confirm a read-only key is intended.",
            render_scopes(scopes),
            allowed
        ),
        (true, true) => bail!(
            "--read-only contradicts the requested scopes: [{}] grants write access\n\
             \n\
             Drop `--read-only` to create a writable key, or remove the write scopes \
             ({}) from `--scopes`.",
            render_scopes(scopes),
            allowed
        ),
        _ => Ok(()),
    }
}

/// `api-key create` — generates a new key and prints the secret exactly once.
async fn run_create(args: ApiKeyCreateArgs) -> Result<()> {
    let scopes: Vec<String> = args
        .scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Validated before the store is opened: a refused invocation must not create
    // the database file as a side effect.
    validate_create_scopes(&scopes, args.read_only)?;
    let owner = validate_create_owner(&args.root, &args.owner, args.allow_unknown_identity)?;

    let mut store = open_store(&args.root).await?;
    if args.allow_non_main_tenant {
        store = store.with_non_main_tenants();
    }

    // R1 — one identity, one active key. A second active key for the same owner would
    // map two secrets to a single `sub` (`api_keys.owner`), which is exactly the state the
    // v2.0.0 identity model forbids. Checked here, after the store is open: the guard only
    // fires when a key already exists, so the database is never created by a refused
    // invocation. The refusal is actionable (R3) — it names the existing key's prefix and
    // the rotation command, since `rotate` is how the operator replaces a credential
    // without ever leaving two active keys.
    let active = store.list(false, None).await.map_err(|e| {
        anyhow::anyhow!("listing active keys to enforce R1 (one key per identity): {e}")
    })?;
    if let Some(existing) = active.iter().find(|k| k.owner == owner) {
        bail!(
            "refusing to create a second active key for '{owner}': an active key already \
             exists (prefix {prefix})\n\
             \n\
             One identity carries exactly one active key (R1). To replace the credential \
             without leaving two active keys, rotate the existing one:\n\
             \n\
             gradatum-admin api-key rotate --root {root} --prefix {prefix}\n\
             \n\
             `rotate` revokes the old key and mints its replacement atomically. To retire \
             the identity instead, revoke the key with `api-key revoke`.",
            prefix = existing.prefix,
            root = args.root.display(),
        );
    }

    let material = store
        .create(&owner, scopes, args.tenant, args.description)
        .await
        .map_err(|e| anyhow::anyhow!("API key creation failed: {e}"))?;

    // Print the secret ONCE to stdout.
    // Two separate lines: pipe-friendly and human-readable.
    eprintln!("API key created (secret shown ONCE ONLY):");
    eprintln!("  prefix: {}", material.prefix);
    println!("{}", material.secret);

    Ok(())
}

/// `api-key list` — prints the list of keys (secrets never displayed).
async fn run_list(args: ApiKeyListArgs) -> Result<()> {
    let store = open_store(&args.root).await?;

    let keys = store
        .list(args.all, None)
        .await
        .map_err(|e| anyhow::anyhow!("API keys listing failed: {e}"))?;

    if keys.is_empty() {
        eprintln!("No key{}.", if args.all { "" } else { " active" });
        return Ok(());
    }

    // Table header.
    println!(
        "{:<12}  {:<24}  {:<12}  {:<16}  state",
        "prefix", "owner", "tenant", "scopes"
    );
    println!("{}", "-".repeat(80));

    for key in &keys {
        let etat = if key.is_revoked() {
            "revoked"
        } else {
            "active"
        };
        let scopes = key.scopes.join(",");
        println!(
            "{:<12}  {:<24}  {:<12}  {:<16}  {}",
            key.prefix, key.owner, key.tenant_id, scopes, etat
        );
    }

    // A1-bis (trou 2 — detection cles LIVE menteuses, 2026-07-27).
    // Une cle est "menteuse" si ses scopes contiennent un nom qui evoque
    // l'ecriture (contient "write" ou "admin") mais has_write_scope
    // retourne false — la cle ne peut pas ecrire malgre ses scopes.
    let menteuses: Vec<&ApiKey> = keys
        .iter()
        .filter(|k| {
            !k.is_revoked()
                && k.scopes
                    .iter()
                    .any(|s| s.contains("write") || s.contains("admin"))
                && !has_write_scope(&k.scopes)
        })
        .collect();
    if !menteuses.is_empty() {
        eprintln!();
        for k in &menteuses {
            eprintln!(
                "⚠ INCONSISTENT — key {} (owner={}) carries scopes [{}] \
                 that suggest write intent but no effective write scope. \
                 Fix by key rotation or SQLite UPDATE.",
                k.prefix,
                k.owner,
                k.scopes.join(",")
            );
        }
        eprintln!(
            "  → {} inconsistent key(s) detected. See \
             memory/gradatum-modele-droits-2026-07-27.md §1 (gap 2).",
            menteuses.len()
        );
    }

    eprintln!("\n{} key(s) listed.", keys.len());

    Ok(())
}

/// `api-key revoke` — revokes a key by its prefix.
async fn run_revoke(args: ApiKeyRevokeArgs) -> Result<()> {
    let store = open_store(&args.root).await?;

    store
        .revoke(&args.prefix)
        .await
        .map_err(|e| anyhow::anyhow!("revocation failed for '{}': {e}", args.prefix))?;

    eprintln!("Key '{}' revoked successfully.", args.prefix);

    Ok(())
}

/// `api-key rotate` — revokes the existing key and atomically generates a replacement.
///
/// Validates the source key scopes before rotation (A1-bis — trou rotate ferme le
/// 2026-07-27). `rotate` copies the source key's scopes verbatim: if the source is
/// "menteuse" (pretend ecrire sans scope write effectif), the rotation perpetuates
/// the lie. The guard re-runs [`validate_create_scopes`] on the source scopes before
/// the atomic rotate — same check as `create`.
async fn run_rotate(args: ApiKeyRotateArgs) -> Result<()> {
    let store = open_store(&args.root).await?;

    // A1-bis : valider les scopes de la source avant rotation.
    // rotate copie les scopes tels quels — si la source est menteuse,
    // la rotation perpetue le mensonge. Ferme le trou signale le 2026-07-27.
    let keys = store
        .list(false, None)
        .await
        .map_err(|e| anyhow::anyhow!("failed to list keys for rotation validation: {e}"))?;
    if let Some(source) = keys.iter().find(|k| k.prefix == args.prefix) {
        // read_only = !has_write_scope(...) : si la cle source n'a pas de scope
        // write, on la valide en read-only. Si elle en a, en writable.
        let read_only = !has_write_scope(&source.scopes);
        validate_create_scopes(&source.scopes, read_only)?;
    }

    let material = store
        .rotate(&args.prefix)
        .await
        .map_err(|e| anyhow::anyhow!("rotation failed for '{}': {e}", args.prefix))?;

    // New secret printed ONCE to stdout.
    eprintln!("Rotation succeeded (old prefix revoked: {}).", args.prefix);
    eprintln!("New secret (shown ONCE ONLY):");
    eprintln!("  prefix: {}", material.prefix);
    println!("{}", material.secret);

    Ok(())
}

/// `api-key reset` — revokes EVERY active key in the registry (R6/R7).
///
/// **R7 — scope is the key registry ALONE.** This function opens only the api_keys store
/// (`<root>/db/api_keys.sqlite`) and holds no reference to the vault (`index.db`) or any
/// note: it cannot touch the vault, by construction. The wipe is by **revocation**, never
/// by row deletion, so the audit trail survives — `api-key list --all` keeps showing the
/// retired keys. After a reset the registry has no active key, so the server falls into the
/// R5 empty-registry state (503, "run api-key create") until it is re-provisioned; the
/// full default access of `main-agent`/`admin` is then restored by the ACL preset on the
/// next `init`, not by this command.
///
/// Confirmation follows the `vault forget` idiom: a dry-run preview lists the active
/// prefixes, and execution requires echoing that exact list back via `--confirm-prefixes`.
/// There is no `--yes`/`--force` flag on purpose — a blind boolean an alias could carry is
/// exactly what the echo-of-list guards against.
///
/// # Errors
/// Returns an error when the store cannot be opened or listed, when `--confirm-prefixes`
/// does not match the previewed active set, or when a revocation fails.
async fn run_reset(args: ApiKeyResetArgs) -> Result<()> {
    let store = open_store(&args.root).await?;

    let active = store
        .list(false, None)
        .await
        .map_err(|e| anyhow::anyhow!("listing active keys for reset: {e}"))?;

    // Preview — printed in both dry-run and execute, so the operator always sees the exact
    // set the confirmation must echo back.
    println!(
        "=== api-key reset preview ({} active key(s) to revoke) ===",
        active.len()
    );
    if active.is_empty() {
        println!("No active key.");
    } else {
        println!("{:<12}  owner", "prefix");
        for key in &active {
            println!("{:<12}  {}", key.prefix, key.owner);
        }
    }

    let expected: Vec<String> = active.iter().map(|k| k.prefix.clone()).collect();

    // Dry-run: stop here, echoing the exact command to run next.
    if !args.execute {
        println!(
            "\n[DRY-RUN] To execute, re-run with --execute --confirm-prefixes \"{}\"",
            expected.join(",")
        );
        return Ok(());
    }

    // Execute: the echoed list must match the previewed active prefixes exactly. Order is
    // irrelevant, membership is not — same shape as the `vault forget` confirmation.
    let mut expected_sorted = expected.clone();
    expected_sorted.sort();
    let mut confirmed_sorted = args.confirm_prefixes.clone();
    confirmed_sorted.sort();
    if expected_sorted != confirmed_sorted {
        bail!(
            "confirm-prefixes mismatch: {} active key(s) to confirm, {} provided — \
             re-run without --execute to get the exact list to echo back",
            expected_sorted.len(),
            confirmed_sorted.len()
        );
    }

    if expected.is_empty() {
        println!("No active key to revoke — operation cancelled.");
        return Ok(());
    }

    // Alert-level log: wiping the whole registry is a high-consequence, rarely legitimate
    // operation — it must leave a trace even when stdout is discarded.
    tracing::warn!(
        active_keys = expected.len(),
        "api-key reset: revoking EVERY active key in the registry (R6)"
    );

    for key in &active {
        store.revoke(&key.prefix).await.map_err(|e| {
            anyhow::anyhow!(
                "reset: revoking '{}' (owner {}): {e}",
                key.prefix,
                key.owner
            )
        })?;
    }

    eprintln!(
        "api-key reset: {} key(s) revoked. The registry now has no active key; the server \
         will answer 503 (empty registry) until you re-provision it with \
         `gradatum-admin api-key create` (or `gradatum-admin init`).",
        expected.len()
    );

    Ok(())
}
