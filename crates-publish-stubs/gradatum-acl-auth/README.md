# gradatum-acl-auth

> Argon2id bearer credential verification and scope enforcement per vault.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Functions

```rust
/// Verify a bearer token against its argon2id hash.
/// Constant-time comparison to prevent timing attacks.
pub fn verify_bearer(
    token: &str,
    hash: &str,
) -> Result<bool, AclAuthError>

/// Hash a new bearer token with argon2id (m=65536, t=3, p=4).
pub fn hash_bearer(token: &str) -> Result<String, AclAuthError>

/// Enforce that a TrustContext carries the required scope.
pub fn enforce_scope(
    trust: &TrustContext,
    required: Scope,
) -> Result<(), AclAuthError>
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum AclAuthError {
    InsufficientScope { required: Scope, granted: Vec<Scope> },
    Unauthenticated,
    HashError(String),
    InvalidHash,
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation → `v0.1.0` public
- License : Apache-2.0