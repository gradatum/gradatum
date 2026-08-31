# gradatum-embed

> `Embedder` trait with HTTP (OpenAI-compatible) and ONNX CPU backends, plus a fallback decorator.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-embed` defines the `Embedder` trait (plus the `EmbedBackend` backend-kind enum) and provides three concrete backends:

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
gradatum-embed = "2.1.0"

# For local ONNX CPU inference:
gradatum-embed = { version = "2.1.0", features = ["fastembed-cpu"] }
```

```rust
use gradatum_embed::{Embedder, HttpEmbedder};

// The endpoint is the full URL, and the third argument is the vector dimension.
let embedder = HttpEmbedder::new("http://127.0.0.1:8436/v1/embeddings", "bge-m3", 1024);
let vectors = embedder.embed_batch(&["hello world", "foo bar"]).await?;
assert_eq!(vectors[0].len(), 1024); // bge-m3 dimension
```

## Feature Flags

| Feature | Description |
|---|---|
| `fastembed-cpu` (off by default) | Enables `FastEmbedCpu` — local ONNX inference via fastembed |
| `windows-native-tls` (off by default) | Falls back to the Windows native certificate store via `native-tls` |

## Anti-cycle invariant

`gradatum-embed` must not depend on `gradatum-engine`.
`gradatum-engine` may depend on `gradatum-embed`.

## License

Apache-2.0
