# gradatum-embed

> `Embedder` trait with HTTP and CPU backends, fallback decorator. Local inference via [`gradatum-engine`](https://crates.io/crates/gradatum-engine).

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Trait

```rust
/// Embedding backend — produces fixed-dimension float vectors.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Unique identifier for this backend (e.g. "bge-small-en-v1.5-cpu").
    fn embedder_id(&self) -> &str;

    /// Output vector dimension. Must be consistent across all calls.
    fn dim(&self) -> usize;

    /// Embed a batch of texts. Returns one vector per input.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

pub trait EmbedBackend: Embedder {}
```

### Implementations

```rust
/// HTTP OpenAI-compatible /v1/embeddings backend (any embedding server, e.g. bge-m3, dim=1024).
pub struct HttpEmbedder { ... }

impl HttpEmbedder {
    pub fn new(base_url: &str, model: &str, bearer: Option<&str>) -> Self
}

/// Local CPU inference via fastembed (ONNX). Feature-gated.
/// feature = "fastembed-cpu" (disabled by default).
#[cfg(feature = "fastembed-cpu")]
pub struct FastEmbedCpu { ... }

/// No-op embedder — returns zero vectors (tests / disabled state).
pub struct Noop { dim: usize }

/// Decorator: tries primary, falls back to secondary on error.
/// Implements circuit-breaker pattern.
pub struct FallbackEmbedder<P: Embedder, F: Embedder> { ... }
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    Http(reqwest::Error),
    DimMismatch { expected: usize, got: usize },
    EmptyBatch,
    Backend(String),
}
```

## Feature flags

| Feature | Description | Default |
|---|---|---|
| `fastembed-cpu` | ONNX local inference via fastembed | disabled |

## Anti-cycle invariant

`gradatum-embed` MUST NOT depend on `gradatum-engine`.
`gradatum-engine` MAY depend on `gradatum-embed`.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0