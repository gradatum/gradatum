# gradatum-chat

> `Chat` trait with heuristic, HTTP (OpenAI-compatible), and no-op backends plus circuit breaker decorator.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Trait

```rust
/// LLM chat backend for curator gating decisions.
#[async_trait]
pub trait Chat: Send + Sync {
    /// Classify a note in context — returns curator verdict + confidence.
    async fn classify_curator(
        &self,
        note: &Note,
        ctx: &CuratorContext,
    ) -> Result<(CuratorVerdict, f32), ChatError>;
}

pub trait ChatBackend: Chat {}
```

### Types

```rust
pub enum CuratorVerdict {
    Admit,
    Route { section: SectionId },
    Retire,
    Defer,
}

pub struct CuratorContext {
    pub vault_id: String,
    pub existing_sections: Vec<SectionId>,
}
```

### Implementations

```rust
/// Rule-based heuristic classifier — no network dependency (invariant #3 / R1).
pub struct Heuristic { ... }

/// OpenAI-compatible HTTP backend (local inference server / gateway-v2).
pub struct HttpChat { ... }

impl HttpChat {
    pub fn new(base_url: &str, model: &str, bearer: Option<&str>) -> Self
}

/// No-op backend — always returns Defer with confidence 0.0 (tests / disabled).
pub struct Noop;

/// Circuit breaker decorator: opens after N consecutive failures, resets after cooldown.
pub struct CircuitBreakerChat<C: Chat> { ... }

impl<C: Chat> CircuitBreakerChat<C> {
    pub fn new(inner: C, failure_threshold: u32, cooldown: Duration) -> Self
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    Http(reqwest::Error),
    Serialization(serde_json::Error),
    CircuitOpen,
    Backend(String),
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0