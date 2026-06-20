# gradatum-auth

> JWT verification (Ed25519, audience-scoped, mandatory `kid`), API key exchange, and token revocation.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
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
gradatum-auth = "0.4.0"
```

```rust
use gradatum_auth::jwt::{verify_jwt, Claims};

let claims: Claims = verify_jwt(&token, &public_key, "gradatum-api")?;
println!("consumer: {}, scopes: {:?}", claims.sub, claims.scopes);
```

## Modules

| Module | Contents |
|---|---|
| `jwt` | JWT Ed25519 verification + `Claims` struct |
| `revocation` | `RevocationStore` — in-memory and SQLite-backed `jti` revocation |

## License

Apache-2.0
