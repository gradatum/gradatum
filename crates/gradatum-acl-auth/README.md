# gradatum-acl-auth

> Argon2id bearer credential verification and scope enforcement per vault.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-acl-auth` handles the credential layer for API key consumers. When a consumer
presents an API key (`ak_xxx`), this crate verifies it against the argon2id hash stored
in the SQLite credential store and resolves the associated owner, scopes, and tenant ID.

Authentication flow (Path 2):

```text
gradatum-admin api-key create
  → generate 256-bit secret
  → argon2id hash (m=65536, t=3, p=4)
  → persist row in SqliteApiKeyStore
  → print ak_xxx to stdout (one time only)

Consumer: POST /auth/exchange
  Authorization: Bearer ak_xxx
  → verify(ak_xxx) → owner + scopes + tenant_id
  → JWT signed and returned
  → Consumer caches JWT for subsequent /api/v1/* calls
```

## Usage

```toml
[dependencies]
gradatum-acl-auth = "0.4.0"
```

```rust
use gradatum_acl_auth::{verify_bearer, enforce_scope};

let ok = verify_bearer(&presented_token, &stored_hash)?;
enforce_scope(&trust_context, Scope::Write)?;
```

## License

Apache-2.0
