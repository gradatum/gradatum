# gradatum-heartbeat

> Fire-and-forget fleet heartbeat client for gradatum — a fleet member announces liveness to the registry ingress.

**Status**: v2.5.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-heartbeat` is the thin client through which a fleet member (server, worker,
gateway, engine) announces its liveness to the registry ingress with a periodic authenticated
POST. It is designed around one contract: **a heartbeat never degrades the service it reports
on.** The client:

- runs in a detached task — the caller never awaits it and errors are never propagated;
- is non-blocking — a slow or unreachable ingress cannot stall the caller; every request
  carries a short timeout and runs off the caller's path;
- is panic-free on the network path — no `unwrap`, `expect`, or `panic`;
- backs off — consecutive failures widen the interval up to a fixed cap, so a down ingress is
  never hammered;
- throttles its logs — a persistent outage logs once at onset and then only on state
  transitions (outage / recovery), not on every missed beat.

Heartbeats are **opt-in and configured by environment** — nothing is hardcoded. When no target
URL is set, no task is spawned and no error is raised. When a URL is set but no token, the
client refuses to emit an unauthenticated heartbeat (fail-safe): it logs a warning and emits
nothing.

## Usage

```toml
[dependencies]
gradatum-heartbeat = "2.5.0"
```

The common path spawns a producer from the environment at startup:

```rust
use gradatum_heartbeat::spawn_from_env;

// Reads GRADATUM_HEARTBEAT_URL and GRADATUM_HEARTBEAT_TOKEN.
// Returns None when the URL is absent (opt-in) or the token is missing (fail-safe);
// otherwise returns a JoinHandle for the detached beat loop.
let _handle = spawn_from_env(
    "worker",                                  // fleet service name (exact allowlist match)
    env!("CARGO_PKG_VERSION"),                 // binary version
    gradatum_heartbeat::sha_opt(env!("BUILD_SHA")), // build commit SHA, empty -> None
);
```

For an explicit configuration (custom cadence, non-environment token source), build a
`HeartbeatConfig` and call `spawn`:

```rust
use std::time::Duration;
use secrecy::SecretString;
use gradatum_heartbeat::{spawn, HeartbeatConfig, DEFAULT_INTERVAL};

let handle = spawn(HeartbeatConfig {
    url: "http://registry.internal:19095/internal/v1/registry/heartbeat".to_owned(),
    token: SecretString::from("<heartbeat-scope-token>".to_owned()),
    service: "worker".to_owned(),
    version: env!("CARGO_PKG_VERSION").to_owned(),
    build_sha: None,
    interval: DEFAULT_INTERVAL, // 15 s
});
```

## Public API

| Item | Description |
|---|---|
| `spawn_from_env(service, version, build_sha) -> Option<JoinHandle<()>>` | Spawns a producer from the environment, or `None` when disabled |
| `spawn(cfg: HeartbeatConfig) -> JoinHandle<()>` | Spawns the detached beat loop for an explicit configuration |
| `HeartbeatConfig` | Static parameters of one heartbeat producer (URL, token, service, version, build SHA, interval) |
| `sha_opt(sha: &str) -> Option<String>` | Maps a build-SHA string to `Option<String>` (empty ⇒ `None`) |
| `ENV_URL`, `ENV_TOKEN` | Environment variable names read by `spawn_from_env` |
| `DEFAULT_INTERVAL` | Default heartbeat cadence (15 s) |

The `service` value must match one of the registry's allowlisted names exactly; any other
value is rejected by the store.

## License

Apache-2.0
