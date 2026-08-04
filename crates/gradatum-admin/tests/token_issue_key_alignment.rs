//! P0 regression — `gradatum-admin token issue` must sign with the server's key.
//!
//! Until v1.0.0 this command signed with the PKCS#8 PEM pair produced by
//! `gradatum-admin init` (`config/jwt.private.pem`, `kid = "gradatum-admin-issued"`),
//! while `gradatum-server` signed and verified with the raw Ed25519 seed
//! `config/jwt-signing-key.secret` (`kid = "gradatum-v0"`). Every token emitted
//! through the documented operator path was rejected with
//! `HTTP 401 {"error":"authentication required"}`.
//!
//! These tests exercise the exact production sequence: server boot creates the
//! key, the CLI issues a token, the server verifies it.

use std::path::PathBuf;

use gradatum_admin::token::{TokenIssueArgs, issue_token};
use gradatum_auth::key_store;
use gradatum_core::paths::config_dir;

const TTL_HUMAN: u64 = 3600;
const TTL_SERVICE: u64 = 86400;

fn args(root: PathBuf) -> TokenIssueArgs {
    TokenIssueArgs {
        root,
        sub: "diag".to_string(),
        scopes: "vault_read".to_string(),
        tenant: "main".to_string(),
        ttl_secs: None,
    }
}

/// The token issued by the CLI must verify against the server's JWT service.
///
/// This is the assertion that failed before the fix (`InvalidKid`, then a
/// signature mismatch) and that produced the reproducible `401` on the LIVE
/// deployment.
#[test]
fn issued_token_verifies_against_the_server_jwt_service() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(config_dir(root.path())).expect("creating config/");

    // Server first boot — creates config/jwt-signing-key.secret.
    let server = key_store::load_or_generate(&config_dir(root.path()), TTL_HUMAN, TTL_SERVICE)
        .expect("server boot must generate the signing key");

    // Operator: gradatum-admin token issue --root <root> --sub diag --scopes vault_read
    let token = issue_token(&args(root.path().to_path_buf())).expect("token issue must succeed");

    let claims = server
        .verify(&token)
        .expect("the server must accept the token issued by gradatum-admin");

    assert_eq!(claims.sub, "diag");
    assert_eq!(claims.tenant_id, "main");
    assert!(
        claims.scopes.iter().any(|s| s == "vault_read"),
        "the requested scopes must be carried by the token; got {:?}",
        claims.scopes
    );
}

/// A root where the server has never booted yields an explicit error, not an orphan token.
///
/// Before the fix the command happily signed with a PEM key nobody would ever
/// verify against — the failure only surfaced later, as a `401` on an unrelated
/// request. Failing here, naming the missing file, is the point.
#[test]
fn token_issue_fails_explicitly_when_the_server_key_is_absent() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(config_dir(root.path())).expect("creating config/");

    let err = issue_token(&args(root.path().to_path_buf()))
        .expect_err("issuing must fail when no signing key exists");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("jwt-signing-key.secret"),
        "the error must name the missing file so the operator can act; got: {msg}"
    );
    assert!(
        !config_dir(root.path())
            .join("jwt-signing-key.secret")
            .exists(),
        "the CLI must never create the signing key — it belongs to the server"
    );
}

/// Two successive issuances share one key: no rotation as a side effect.
#[test]
fn repeated_issuance_keeps_using_the_same_server_key() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(config_dir(root.path())).expect("creating config/");

    let server = key_store::load_or_generate(&config_dir(root.path()), TTL_HUMAN, TTL_SERVICE)
        .expect("server boot");

    let first = issue_token(&args(root.path().to_path_buf())).expect("first issuance");
    let second = issue_token(&args(root.path().to_path_buf())).expect("second issuance");

    server.verify(&first).expect("first token must stay valid");
    server.verify(&second).expect("second token must be valid");
}

/// `--ttl-secs` is honoured: `exp - iat` equals the requested TTL.
///
/// Replaces a test that measured this on a throwaway `JwtService` built in the
/// test itself; it now measures it on the token the command actually emits.
#[test]
fn custom_ttl_is_applied_to_the_issued_token() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(config_dir(root.path())).expect("creating config/");
    let server = key_store::load_or_generate(&config_dir(root.path()), TTL_HUMAN, TTL_SERVICE)
        .expect("server boot");

    let ttl_secs = 7200;
    let mut a = args(root.path().to_path_buf());
    a.sub = "worker".to_string();
    a.scopes = "vault_write".to_string();
    a.ttl_secs = Some(ttl_secs);

    let token = issue_token(&a).expect("token issue must succeed");
    let claims = server.verify(&token).expect("the server must accept it");

    assert_eq!(
        claims.exp - claims.iat,
        ttl_secs,
        "the effective TTL must match --ttl-secs"
    );
}

/// A non-`main` tenant is carried into the claims and survives verification.
#[test]
fn non_main_tenant_is_preserved_in_the_issued_token() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(config_dir(root.path())).expect("creating config/");
    let server = key_store::load_or_generate(&config_dir(root.path()), TTL_HUMAN, TTL_SERVICE)
        .expect("server boot");

    let mut a = args(root.path().to_path_buf());
    a.sub = "agent-1".to_string();
    a.tenant = "staging".to_string();

    let token = issue_token(&a).expect("token issue must succeed");
    let claims = server.verify(&token).expect("the server must accept it");

    assert_eq!(claims.tenant_id, "staging");
}
