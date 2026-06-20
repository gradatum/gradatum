//! `gradatum-admin api-key {create,list,revoke,rotate}` — API key lifecycle management.
//!
//! ## Sub-commands
//!
//! ```text
//! gradatum-admin api-key create --root /var/lib/gradatum --owner mcp-stub [--scopes vault_read] [--tenant main] [--description "desc"]
//! gradatum-admin api-key list   --root /var/lib/gradatum [--all]
//! gradatum-admin api-key revoke --root /var/lib/gradatum --prefix ak_abcdef01
//! gradatum-admin api-key rotate --root /var/lib/gradatum --prefix ak_abcdef01
//! ```
//!
//! ## Security
//! - The secret is printed to stdout ONCE on `create` and `rotate`
//! - The argon2id hash is never displayed
//! - The SQLite path is derived from `--root` (`<root>/db/api_keys.sqlite`)

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use gradatum_acl_auth::{ApiKeyStore, SqliteApiKeyStore};

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
}

/// Arguments for `api-key create`.
#[derive(Debug, Args)]
pub struct ApiKeyCreateArgs {
    /// Gradatum root directory.
    #[arg(long)]
    pub root: PathBuf,

    /// Key owner (e.g. `mcp-stub`, `curator-worker`).
    #[arg(long)]
    pub owner: String,

    /// Granted scopes, comma-separated (e.g. `vault_read,vault_search`).
    #[arg(long, default_value = "vault_read")]
    pub scopes: String,

    /// Target tenant.
    #[arg(long, default_value = "main")]
    pub tenant: String,

    /// Optional description for the key.
    #[arg(long)]
    pub description: Option<String>,
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

/// Entry point for the `api-key` sub-command.
pub async fn run(cmd: ApiKeyCmd) -> Result<()> {
    match cmd {
        ApiKeyCmd::Create(args) => run_create(args).await,
        ApiKeyCmd::List(args) => run_list(args).await,
        ApiKeyCmd::Revoke(args) => run_revoke(args).await,
        ApiKeyCmd::Rotate(args) => run_rotate(args).await,
    }
}

/// Resolves the `api_keys` database path as `{root}/db/api_keys.sqlite`.
///
/// Derivation from `root` is reliable and consistent with the `init` layout.
fn resolve_db_path(root: &std::path::Path) -> PathBuf {
    root.join("db/api_keys.sqlite")
}

/// Opens the SQLite API key store.
async fn open_store(root: &std::path::Path) -> Result<SqliteApiKeyStore> {
    let db_path = resolve_db_path(root);
    SqliteApiKeyStore::init(&db_path)
        .await
        .with_context(|| format!("ouverture du store api_keys : {}", db_path.display()))
}

/// `api-key create` — generates a new key and prints the secret exactly once.
async fn run_create(args: ApiKeyCreateArgs) -> Result<()> {
    let store = open_store(&args.root).await?;

    let scopes: Vec<String> = args
        .scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let material = store
        .create(&args.owner, scopes, args.tenant, args.description)
        .await
        .map_err(|e| anyhow::anyhow!("création API key échouée: {e}"))?;

    // Print the secret ONCE to stdout.
    // Two separate lines: pipe-friendly and human-readable.
    eprintln!("API key créée (secret affiché UNE SEULE FOIS) :");
    eprintln!("  préfixe : {}", material.prefix);
    println!("{}", material.secret);

    Ok(())
}

/// `api-key list` — prints the list of keys (secrets never displayed).
async fn run_list(args: ApiKeyListArgs) -> Result<()> {
    let store = open_store(&args.root).await?;

    let keys = store
        .list(args.all)
        .await
        .map_err(|e| anyhow::anyhow!("listage API keys échoué: {e}"))?;

    if keys.is_empty() {
        eprintln!("Aucune clé{}.", if args.all { "" } else { " active" });
        return Ok(());
    }

    // Table header.
    println!(
        "{:<12}  {:<24}  {:<12}  {:<16}  état",
        "préfixe", "owner", "tenant", "scopes"
    );
    println!("{}", "-".repeat(80));

    for key in &keys {
        let etat = if key.is_revoked() {
            "révoquée"
        } else {
            "active"
        };
        let scopes = key.scopes.join(",");
        println!(
            "{:<12}  {:<24}  {:<12}  {:<16}  {}",
            key.prefix, key.owner, key.tenant_id, scopes, etat
        );
    }

    eprintln!("\n{} clé(s) listée(s).", keys.len());

    Ok(())
}

/// `api-key revoke` — revokes a key by its prefix.
async fn run_revoke(args: ApiKeyRevokeArgs) -> Result<()> {
    let store = open_store(&args.root).await?;

    store
        .revoke(&args.prefix)
        .await
        .map_err(|e| anyhow::anyhow!("révocation échouée pour '{}': {e}", args.prefix))?;

    eprintln!("Clé '{}' révoquée avec succès.", args.prefix);

    Ok(())
}

/// `api-key rotate` — revokes the existing key and atomically generates a replacement.
async fn run_rotate(args: ApiKeyRotateArgs) -> Result<()> {
    let store = open_store(&args.root).await?;

    let material = store
        .rotate(&args.prefix)
        .await
        .map_err(|e| anyhow::anyhow!("rotation échouée pour '{}': {e}", args.prefix))?;

    // New secret printed ONCE to stdout.
    eprintln!(
        "Rotation réussie (ancien préfixe révoqué : {}).",
        args.prefix
    );
    eprintln!("Nouveau secret (affiché UNE SEULE FOIS) :");
    eprintln!("  préfixe : {}", material.prefix);
    println!("{}", material.secret);

    Ok(())
}
