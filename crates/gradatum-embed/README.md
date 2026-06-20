# gradatum-embed

> `Embedder` trait with HTTP (OpenAI-compatible) and ONNX CPU backends, plus a fallback decorator.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-embed` defines the `Embedder` trait and provides three concrete backends:

| Backend | Description |
|---|---|
| `HttpEmbedder` | Calls any OpenAI-compatible `/v1/embeddings` endpoint (e.g. a remote bge-m3 server) |
| `FastEmbedCpu` | Local ONNX inference via fastembed (feature-gated, no GPU required) |
| `Noop` | Returns zero vectors — for tests and disabled-embed configurations |

`FallbackEmbedder<P, F>` wraps any primary + fallback pair: if the primary backend fails,
the fallback is called transparently.

## Usage

```toml
[dependencies]
gradatum-embed = "0.4.0"

# For local ONNX CPU inference:
gradatum-embed = { version = "0.4.0", features = ["fastembed-cpu"] }
```

```rust
use gradatum_embed::{Embedder, HttpEmbedder};

let embedder = HttpEmbedder::new("http://127.0.0.1:8432", "bge-m3", Some("token"));
let vectors = embedder.embed(&["hello world", "foo bar"]).await?;
assert_eq!(vectors[0].len(), 1024); // bge-m3 dimension
```

## Feature Flags

| Feature | Description |
|---|---|
| `fastembed-cpu` (off by default) | Enables `FastEmbedCpu` — local ONNX inference via fastembed |

## Anti-cycle invariant

`gradatum-embed` must not depend on `gradatum-engine`.
`gradatum-engine` may depend on `gradatum-embed`.

## License

Apache-2.0
