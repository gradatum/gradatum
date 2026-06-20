//! `gradatum-admin token issue` — issues service JWT tokens.
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
//! ## Effects
//! - Loads the Ed25519 private key from `{root}/config/jwt.private.pem`
//! - Signs a JWT with `TokenScope::Service` (24 h TTL by default)
//! - Prints the token to stdout only (pipe-friendly, no decoration)
//!
//! ## Security
//! - The private key is never logged
//! - The token is printed without trailing CRLF (compatible with `export TOKEN=$(...)`)

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use gradatum_auth::jwt::{JwtService, TokenScope};

/// Sub-commands of `token`.
#[derive(Debug, Subcommand)]
pub enum TokenCmd {
    /// Issues a service JWT token.
    Issue(TokenIssueArgs),
}

/// Arguments for the `token issue` sub-command.
#[derive(Debug, Args)]
pub struct TokenIssueArgs {
    /// Gradatum root directory (must contain `config/jwt.private.pem`).
    #[arg(long)]
    pub root: PathBuf,

    /// Token subject (e.g. `mcp-stub`, `curator-worker`, `agent-xxx`).
    #[arg(long)]
    pub sub: String,

    /// Granted scopes, comma-separated (e.g. `vault_read,vault_search`).
    #[arg(long, default_value = "vault_read")]
    pub scopes: String,

    /// Target tenant.
    #[arg(long, default_value = "main")]
    pub tenant: String,

    /// TTL override in seconds (default: 86400 s for service tokens).
    /// When absent, the service TTL from the config is used (86400 s).
    #[arg(long)]
    pub ttl_secs: Option<u64>,
}

/// Entry point for the `token` sub-command.
pub fn run(cmd: TokenCmd) -> Result<()> {
    match cmd {
        TokenCmd::Issue(args) => run_issue(args),
    }
}

/// Issues a service JWT token and prints it to stdout.
///
/// Loads the PEM private key from `{root}/config/jwt.private.pem`,
/// signs with `TokenScope::Service`, and prints the raw token to stdout.
///
/// # Errors
/// - Returns an error if the PEM file is missing or malformed.
/// - Returns an error if JWT signing fails.
fn run_issue(args: TokenIssueArgs) -> Result<()> {
    let priv_path = args.root.join("config/jwt.private.pem");

    // Read the PEM private key.
    let pem = fs::read_to_string(&priv_path)
        .with_context(|| format!("lecture de la clé privée {}", priv_path.display()))?;

    // Decode PKCS8 PEM → Ed25519 SigningKey.
    let signing = SigningKey::from_pkcs8_pem(&pem)
        .map_err(|e| anyhow::anyhow!("décodage PKCS8 PEM échoué: {e}"))?;

    // Build JwtService with TTL override when provided.
    let ttl_service = args.ttl_secs.unwrap_or(86400);
    let jwt = JwtService::new(
        signing,
        "gradatum-admin-issued".to_string(),
        "gradatum".to_string(),
        3600, // ttl_human — non utilisé pour TokenScope::Service
        ttl_service,
    );

    // Parse scopes (comma-separated, whitespace trimmed).
    let scopes: Vec<String> = args
        .scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Sign the token with Service scope.
    let token = jwt
        .sign(&args.sub, &scopes, TokenScope::Service, &args.tenant)
        .map_err(|e| anyhow::anyhow!("signature JWT échouée: {e}"))?;

    // Stdout only — pipe-friendly, no decoration or trailing CRLF.
    println!("{token}");

    Ok(())
}
