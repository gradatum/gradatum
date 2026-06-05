# gradatum-curator

> LLM-powered note curation: heuristic-first gating with optional LLM review for low-confidence notes.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Structs

```rust
/// Main curator workflow — generic over Chat backend.
pub struct Curator<C: Chat> { ... }

impl<C: Chat> Curator<C> {
    pub fn new(chat: C, config: CuratorConfig) -> Self

    /// Evaluate a note: heuristic → (optional) LLM → decision.
    pub async fn decide(
        &self,
        note: &Note,
        ctx: &CuratorContext,
    ) -> Result<CuratorDecision, CuratorError>
}
```

### Types

```rust
pub enum CuratorDecision {
    Admit { section: SectionId },
    Route { from: SectionId, to: SectionId },
    Retire,
    Defer,
}

pub enum FallbackStrategy {
    /// Use heuristic verdict on LLM error.
    UseHeuristic,
    /// Defer the note on LLM error.
    Defer,
    /// Fail hard on LLM error.
    Fail,
}

pub struct CuratorConfig {
    pub confidence_threshold: f32,   // default: 0.85
    pub llm_review_enabled: bool,    // default: true
    pub fallback: FallbackStrategy,  // default: UseHeuristic
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum CuratorError {
    Chat(ChatError),
    Core(GradatumError),
}
```

## Offline-first invariant

The heuristic runs first, always, with no network dependency (invariant #3 / R1).
LLM is only called for low-confidence notes when `llm_review_enabled = true`.

## Workflow

```
Curator::decide(note, ctx)
  step 1: Heuristic::classify_curator(note, ctx)
  step 2: confidence > threshold → fast path (heuristic verdict)
  step 3: llm_review_enabled → C::classify_curator(note, ctx)
  step 4: LLM error → FallbackStrategy applied
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0