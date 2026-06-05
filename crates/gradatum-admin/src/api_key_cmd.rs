//! `gradatum-admin api-key {create,list,revoke,rotate}` — gestion du cycle de vie des API keys.
//!
//! ## Sous-commandes
//!
//! ```text
//! gradatum-admin api-key create --root /var/lib/gradatum --owner mcp-stub [--scopes vault_read] [--tenant main] [--description "desc"]
//! gradatum-admin api-key list   --root /var/lib/gradatum [--all]
//! gradatum-admin api-key revoke --root /var/lib/gradatum --prefix ak_abcdef01
//! gradatum-admin api-key rotate --root /var/lib/gradatum --prefix ak_abcdef01
//! ```
//!
//! ## Sécurité
//! - Le secret est affiché UNE SEULE FOIS sur stdout lors de `create` et `rotate`
//! - Le hash argon2id n'est jamais affiché
//! - Le fichier SQLite est lu depuis `config/server.toml` (`[auth].api_keys_db_path`)
//!   ou dérivé de `--root` si absent

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use gradatum_acl_auth::{ApiKeyStore, SqliteApiKeyStore};

/// Sous-commandes de `api-key`.
#[derive(Debug, Subcommand)]
pub enum ApiKeyCmd {
    /// Crée une nouvelle API key.
    Create(ApiKeyCreateArgs),
    /// Liste les API keys existantes.
    List(ApiKeyListArgs),
    /// Révoque une API key par son préfixe.
    Revoke(ApiKeyRevokeArgs),
    /// Révoque l'ancienne clé et en génère une nouvelle atomiquement.
    Rotate(ApiKeyRotateArgs),
}

/// Arguments de `api-key create`.
#[derive(Debug, Args)]
pub struct ApiKeyCreateArgs {
    /// Répertoire racine Gradatum.
    #[arg(long)]
    pub root: PathBuf,

    /// Propriétaire de la clé (ex. `mcp-stub`, `curator-worker`).
    #[arg(long)]
    pub owner: String,

    /// Scopes accordés, séparés par virgule (ex. `vault_read,vault_search`).
    #[arg(long, default_value = "vault_read")]
    pub scopes: String,

    /// Tenant cible (D3-complet, D10 multi-tenancy).
    #[arg(long, default_value = "main")]
    pub tenant: String,

    /// Description optionnelle de la clé.
    #[arg(long)]
    pub description: Option<String>,
}

/// Arguments de `api-key list`.
#[derive(Debug, Args)]
pub struct ApiKeyListArgs {
    /// Répertoire racine Gradatum.
    #[arg(long)]
    pub root: PathBuf,

    /// Inclure les clés révoquées dans la liste.
    #[arg(long)]
    pub all: bool,
}

/// Arguments de `api-key revoke`.
#[derive(Debug, Args)]
pub struct ApiKeyRevokeArgs {
    /// Répertoire racine Gradatum.
    #[arg(long)]
    pub root: PathBuf,

    /// Préfixe de la clé à révoquer (ex. `ak_abcdef01`).
    #[arg(long)]
    pub prefix: String,
}

/// Arguments de `api-key rotate`.
#[derive(Debug, Args)]
pub struct ApiKeyRotateArgs {
    /// Répertoire racine Gradatum.
    #[arg(long)]
    pub root: PathBuf,

    /// Préfixe de la clé à faire pivoter (ex. `ak_abcdef01`).
    #[arg(long)]
    pub prefix: String,
}

/// Point d'entrée du sous-commande `api-key`.
pub async fn run(cmd: ApiKeyCmd) -> Result<()> {
    match cmd {
        ApiKeyCmd::Create(args) => run_create(args).await,
        ApiKeyCmd::List(args) => run_list(args).await,
        ApiKeyCmd::Revoke(args) => run_revoke(args).await,
        ApiKeyCmd::Rotate(args) => run_rotate(args).await,
    }
}

/// Résout le chemin de la DB api_keys depuis `{root}/db/api_keys.sqlite`.
///
/// Tentative de lecture depuis `{root}/config/server.toml` reportée —
/// la dérivation depuis root est fiable et cohérente avec le layout `init`.
fn resolve_db_path(root: &std::path::Path) -> PathBuf {
    root.join("db/api_keys.sqlite")
}

/// Ouvre le store SQLite d'API keys.
async fn open_store(root: &std::path::Path) -> Result<SqliteApiKeyStore> {
    let db_path = resolve_db_path(root);
    SqliteApiKeyStore::init(&db_path)
        .await
        .with_context(|| format!("ouverture du store api_keys : {}", db_path.display()))
}

/// `api-key create` — génère une nouvelle clé et affiche le secret UNE SEULE FOIS.
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

    // Afficher le secret UNE SEULE FOIS sur stdout (D8 spec V2).
    // Format : deux lignes séparées pour pipe-friendly et lisibilité humaine.
    eprintln!("API key créée (secret affiché UNE SEULE FOIS) :");
    eprintln!("  préfixe : {}", material.prefix);
    println!("{}", material.secret);

    Ok(())
}

/// `api-key list` — affiche la liste des clés (sans secrets).
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

    // En-tête de tableau.
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

/// `api-key revoke` — révoque une clé par son préfixe.
async fn run_revoke(args: ApiKeyRevokeArgs) -> Result<()> {
    let store = open_store(&args.root).await?;

    store
        .revoke(&args.prefix)
        .await
        .map_err(|e| anyhow::anyhow!("révocation échouée pour '{}': {e}", args.prefix))?;

    eprintln!("Clé '{}' révoquée avec succès.", args.prefix);

    Ok(())
}

/// `api-key rotate` — révoque l'ancienne clé et en génère une nouvelle atomiquement.
async fn run_rotate(args: ApiKeyRotateArgs) -> Result<()> {
    let store = open_store(&args.root).await?;

    let material = store
        .rotate(&args.prefix)
        .await
        .map_err(|e| anyhow::anyhow!("rotation échouée pour '{}': {e}", args.prefix))?;

    // Nouveau secret affiché UNE SEULE FOIS sur stdout (D8 spec V2).
    eprintln!(
        "Rotation réussie (ancien préfixe révoqué : {}).",
        args.prefix
    );
    eprintln!("Nouveau secret (affiché UNE SEULE FOIS) :");
    eprintln!("  préfixe : {}", material.prefix);
    println!("{}", material.secret);

    Ok(())
}
