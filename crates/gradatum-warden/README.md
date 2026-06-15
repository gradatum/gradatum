# gradatum-warden

> L0 network guard for gradatum: IP CIDR filter, per-IP rate limiting, and loopback bypass.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-warden` is a Tower middleware layer mounted on the Axum router in `gradatum-server`.
It enforces three controls before any handler runs:

1. **IP allowlist** — CIDR-based filter; requests from unlisted ranges are rejected with 403.
2. **Per-IP rate limiting** — configurable requests-per-minute with burst allowance.
3. **Loopback bypass** — requests from `127.0.0.1` / `::1` skip rate limiting entirely,
   letting internal health checks and metrics scrapers pass through without quota impact.

The warden always calls `inner.call(req)` for allowed/bypass requests, so the upstream
handler receives the real request body — not a synthetic empty response.

## Usage

```toml
[dependencies]
gradatum-warden = "0.4.0"
```

```rust
use gradatum_warden::{WardenConfig, WardenLayer};

let config = WardenConfig {
    enabled: true,
    rate_limit_per_minute: 60,
    rate_limit_burst: 10,
    bypass_loopback: true,
    ..WardenConfig::default()
};
let warden = WardenLayer::new(config).expect("invalid warden config");

let app = Router::new()
    .route("/api/v1/vault_write", post(write_handler))
    .layer(warden);
```

## License

Apache-2.0
