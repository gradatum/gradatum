# gradatum-auth

> JWT verification (Ed25519, audience-scoped, mandatory `kid`), OIDC integration, and API-key support.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Modules

```rust
pub mod jwt;        // JWT Ed25519 verification + claims
pub mod revocation; // RevocationStore — in-memory + SQLite-backed jti revocation
```

### JWT

```rust
/// Verify a JWT bearer token against the Ed25519 public key.
/// Validates: signature, expiry, audience, mandatory `kid` claim.
pub fn verify_jwt(
    token: &str,
    public_key: &Ed25519PublicKey,
    expected_audience: &str,
) -> Result<Claims, AuthError>

pub struct Claims {
    pub sub: String,           // consumer_id
    pub aud: String,           // audience scope
    pub exp: u64,              // expiry (unix timestamp)
    pub jti: String,           // unique token ID (for revocation)
    pub kid: String,           // key ID (mandatory)
    pub scopes: Vec<Scope>,    // granted scopes
}
```

### Revocation

```rust
/// Token revocation store (blocks reuse of revoked JTIs).
pub trait RevocationStore: Send + Sync {
    async fn revoke(&self, jti: &str, expires_at: u64) -> Result<(), AuthError>;
    async fn is_revoked(&self, jti: &str) -> Result<bool, AuthError>;
    async fn purge_expired(&self) -> Result<u64, AuthError>;
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    InvalidSignature,
    Expired,
    InvalidAudience,
    MissingKid,
    Revoked(String),
    Decode(String),
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation → `v0.1.0` public
- License : Apache-2.0