# gradatum-engine

> On-device inference adapter: provides `Chat`, `Embedder`, and `Reranker` trait implementations backed by candle or llama.cpp. Optional via `engine-local` feature gate.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API (Phase 1+)

### Feature flags

| Feature | Description | Default |
|---|---|---|
| `engine-local` | Enables local inference backends (candle / llama.cpp) | disabled |

### Implementations (feature = `engine-local`)

```rust
/// Chat implementation backed by local llama.cpp model.
pub struct LocalChat { ... }

/// Embedder implementation backed by local candle model (bge-small-en-v1.5).
pub struct LocalEmbedder { ... }

/// Reranker implementation backed by local cross-encoder model.
pub struct LocalReranker { ... }
```

## Anti-cycle invariant

`gradatum-engine` MAY depend on `gradatum-chat` and `gradatum-embed`.
`gradatum-chat` and `gradatum-embed` MUST NOT depend on `gradatum-engine`.
Composition happens at binary level (`gradatum-server`, `gradatum-worker`).

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 1+ (candle CPU benchmarked at 17ms/embed on a modern CPU)
- License : Apache-2.0