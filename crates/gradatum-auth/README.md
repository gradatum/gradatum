# gradatum-auth

> JWT verification (Ed25519, audience-scoped, mandatory `kid`), API key exchange, and token revocation.

**Status**: 0.x — API not yet stable. Apache-2.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-auth` handles external identity verification. It provides:

- **JWT verification** — Ed25519 signature validation, expiry, audience scope, and mandatory
  `kid` claim. JWTs are signed by `gradatum-admin` at init time with a persistent Ed25519 keypair.
- **API key exchange** — a consumer presents a `ak_xxx` API key to `POST /auth/exchange`;
  the server verifies it against the stored argon2id hash (via `gradatum-acl-auth`) and
  returns a short-lived JWT.
- **Revocation store** — in-memory + SQLite-backed `jti` revocation, used to invalidate
  tokens before expiry.

## Usage

```toml
[dependencies]
gradatum-auth = "0.7.6"
```

```rust
use ed25519_dalek::SigningKey;
use gradatum_auth::jwt::{JwtService, TokenScope, Claims};

// Build a JwtService with an Ed25519 signing key.
let signing = SigningKey::generate(&mut rand::rngs::OsRng);
let svc = JwtService::new(signing, "kid-2026".into(), "gradatum".into(), 3600, 86400);

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

## License

Apache-2.0
