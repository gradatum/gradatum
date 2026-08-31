# gradatum-distill

> Semantic distillation primitives: cosine clustering, cluster synthesis and distilled trust scoring.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-distill` groups the distillation processing logic of the gradatum workspace
(formerly scattered across `gradatum-worker` and `gradatum-core`):

- **`distill_cluster`** — cosine-similarity clustering of embeddings (connected components
  of an adjacency graph; pair linked iff cosine ≥ threshold).
- **`synthesizer`** — the `DistillSynthesizer` abstraction (cluster → synthesis note) with a
  deterministic template MVP (`TemplateSynthesizer`, `PendingReview` output).
- **`trust`** — `compute_distill_trust`: mean of source trusts × confidence, clamped to `[0, 1]`.

The job vocabulary (`DistillMode`, `DistillSource`, `Job::Distill`) stays in `gradatum-core`
— those are payload contracts, not processing logic (F-248 architecture decision, 2026-08-25).

## Usage

```toml
[dependencies]
gradatum-distill = "2.1.0"
```

```rust
use gradatum_distill::{cluster_by_cosine, compute_distill_trust};
```

## License

Apache-2.0
