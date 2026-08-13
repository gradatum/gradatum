# gradatum-auth

> JWT verification (Ed25519, audience-scoped, mandatory `kid`), signing-key store, and `jti` revocation.

**Status**: v2.0.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-auth` handles external identity verification. Used by `gradatum-server` to provide:

- **JWT verification** — Ed25519 signature validation, expiry, audience scope, and mandatory
  `kid` claim. The Ed25519 signing seed is created by `gradatum-server` at first boot
  (`config/jwt-signing-key.secret`); `gradatum-admin token issue` loads that same seed — it
  never creates one.
- **Revocation store** — in-memory + SQLite-backed `jti` revocation, used to invalidate
  tokens before expiry.

API key exchange (`POST /auth/exchange`) is **not** part of this crate: argon2id verification of
`ak_xxx` keys lives in `gradatum-acl-auth`, on which this crate does not depend. `gradatum-server`
combines the two.

## Usage

```toml
[dependencies]
gradatum-auth = "2.0.0"
```

```rust
use ed25519_dalek::SigningKey;
use gradatum_auth::jwt::{JwtService, TokenScope, Claims};

// Build a JwtService with an Ed25519 signing key.
let signing = SigningKey::generate(&mut rand::rngs::OsRng);
let svc = JwtService::new(
    signing,
    "kid-2026".into(),  // kid
    "gradatum".into(),  // issuer
    "gradatum".into(),  // audience
    3600,               // ttl_human_secs
    86400,              // ttl_service_secs
);

// Sign a token.
let token = svc.sign("consumer-id", &["read".into()], TokenScope::Service, "main")?;

// Verify — validates signature, kid, audience, and expiry.
let claims: Claims = svc.verify(&token)?;
println!("consumer: {}, scopes: {:?}", claims.sub, claims.scopes);
```

## Modules

| Module | Contents |
|---|---|
| `jwt` | JWT Ed25519 verification + `Claims` struct |
| `revocation` | `RevocationStore` — in-memory and SQLite-backed `jti` revocation |
| `key_store` | `load` / `load_or_generate` / `signing_key_path` — Ed25519 seed on disk (mode 0600) |

## License

Apache-2.0
