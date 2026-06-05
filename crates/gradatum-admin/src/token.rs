//! `gradatum-admin token issue` — émission de tokens JWT service (Path 3 bootstrap).
//!
//! ## Usage
//! ```text
//! gradatum-admin token issue \
//!     --root /var/lib/gradatum \
//!     --sub mcp-stub \
//!     --scopes vault_read,vault_search \
//!     --tenant main
//! ```
//!
//! ## Effets
//! - Charge la clé privée Ed25519 depuis `{root}/config/jwt.private.pem`
//! - Signe un token JWT avec `TokenScope::Service` (TTL 24h par défaut)
//! - Affiche le token sur stdout UNIQUEMENT (pipe-friendly, pas de décoration)
//!
//! ## Sécurité
//! - La clé privée n'est jamais loggée
//! - Le token est affiché sur stdout sans CRLF superflu (compatible `export TOKEN=$(...)`)

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::SigningKey;
use gradatum_auth::jwt::{JwtService, TokenScope};

/// Sous-commandes de `token`.
#[derive(Debug, Subcommand)]
pub enum TokenCmd {
    /// Émet un token JWT service (Path 3 bootstrap).
    Issue(TokenIssueArgs),
}

/// Arguments du sous-commande `token issue`.
#[derive(Debug, Args)]
pub struct TokenIssueArgs {
    /// Répertoire racine Gradatum (contient `config/jwt.private.pem`).
    #[arg(long)]
    pub root: PathBuf,

    /// Subject du token (ex. `mcp-stub`, `curator-worker`, `agent-xxx`).
    #[arg(long)]
    pub sub: String,

    /// Scopes accordés, séparés par virgule (ex. `vault_read,vault_search`).
    #[arg(long, default_value = "vault_read")]
    pub scopes: String,

    /// Tenant cible (D3-complet, D10 multi-tenancy).
    #[arg(long, default_value = "main")]
    pub tenant: String,

    /// TTL override en secondes (défaut : valeur R-A1 service = 86400s).
    /// Si absent, utilise le TTL service de la config (86400s).
    #[arg(long)]
    pub ttl_secs: Option<u64>,
}

/// Point d'entrée du sous-commande `token`.
pub fn run(cmd: TokenCmd) -> Result<()> {
    match cmd {
        TokenCmd::Issue(args) => run_issue(args),
    }
}

/// Émet un token JWT service et l'affiche sur stdout.
///
/// Charge la clé privée PEM depuis `{root}/config/jwt.private.pem`,
/// signe avec `TokenScope::Service`, affiche le token brut sur stdout.
///
/// # Erreurs
/// - Retourne une erreur si le fichier PEM est absent ou malformé.
/// - Retourne une erreur si la signature JWT échoue (impossible en pratique).
fn run_issue(args: TokenIssueArgs) -> Result<()> {
    let priv_path = args.root.join("config/jwt.private.pem");

    // Lecture de la clé privée PEM.
    let pem = fs::read_to_string(&priv_path)
        .with_context(|| format!("lecture de la clé privée {}", priv_path.display()))?;

    // Décodage PKCS8 PEM → SigningKey Ed25519.
    let signing = SigningKey::from_pkcs8_pem(&pem)
        .map_err(|e| anyhow::anyhow!("décodage PKCS8 PEM échoué: {e}"))?;

    // Construction du JwtService avec TTL override si fourni.
    let ttl_service = args.ttl_secs.unwrap_or(86400);
    let jwt = JwtService::new(
        signing,
        "gradatum-admin-issued".to_string(),
        "gradatum".to_string(),
        3600, // ttl_human — non utilisé pour TokenScope::Service
        ttl_service,
    );

    // Parsing des scopes (séparés par virgule, trim whitespace).
    let scopes: Vec<String> = args
        .scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Signature du token avec scope Service.
    let token = jwt
        .sign(&args.sub, &scopes, TokenScope::Service, &args.tenant)
        .map_err(|e| anyhow::anyhow!("signature JWT échouée: {e}"))?;

    // Stdout uniquement — pipe-friendly, pas de décoration ni CRLF superflu.
    println!("{token}");

    Ok(())
}
