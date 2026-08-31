# gradatum-acl-auth

> Argon2id bearer credential verification and scope enforcement per vault.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-acl-auth` handles the credential layer for API key consumers. When a consumer
presents an API key (`ak_xxx`), this crate verifies it against the argon2id hash stored
in the SQLite credential store and resolves the associated owner, scopes, and tenant ID.

Authentication flow:

```text
gradatum-admin api-key create
  → generate 256-bit secret
  → argon2id hash (m=19456 KiB / t=2 / p=1)
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
gradatum-acl-auth = "2.1.0"
```

```rust
use std::path::Path;
use gradatum_acl_auth::{SqliteApiKeyStore, ApiKeyStore, ApiKeyMaterial};
use gradatum_core::scope::AgentId;

// Initialize the credential store (applies migrations, WAL mode).
let store = SqliteApiKeyStore::init(Path::new("/var/lib/gradatum/api_keys.sqlite")).await?;

// Create a new API key (secret displayed ONCE ONLY).
let material: ApiKeyMaterial = store
    .create(&AgentId::new("owner-name"), vec!["read".into()], "main".into(), None)
    .await?;
println!("key: {}", material.secret); // ak_<64 hex chars>

// Verify a presented key — returns metadata on success.
let key = store.verify(&presented_secret).await?;
println!("owner: {}", key.owner);
```

## License

Apache-2.0
