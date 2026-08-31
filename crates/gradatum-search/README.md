# gradatum-search

> Hybrid search orchestration: BM25 full-text (SQLite FTS5), semantic vector (cosine), cross-encoder reranking, and RRF fusion.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-search` orchestrates multi-mode retrieval across the index layer. It combines
two complementary signals and fuses them with Reciprocal Rank Fusion:

- **BM25 full-text** — SQLite FTS5 with BM25 ranking over note bodies and frontmatter.
- **Semantic vector** — cosine similarity over dense embeddings stored in the SQLite index.
- **RRF fusion** — `score(d) = Σ 1/(k + rank_i)` with k=60 (standard constant), combining
  both ranked lists into a single unified result set.
- **Cross-encoder reranker** — optional ONNX-backed reranker for precision re-scoring of
  the top-N fusion results.

The crate exposes RRF fusion, multi-factor scoring and the reranker abstraction only.
Index query types (`SqliteIndex`, `AuthorRow`, `Lineage`, `NoteRecord`, `SearchHitRaw`) are
**not** re-exported here — depend on `gradatum-index` directly for those.

**Recency signal**: the `recency_factor` in composite scoring uses `anchor_ms`
from the temporal index as the decay reference (exponential decay applied at the RRF layer).
Falls back to `created_at` when no temporal anchor is present. Semantic-only hits are enriched
with `anchor_ms` before composite scoring to ensure consistent ranking across both retrieval
paths.

## Usage

```toml
[dependencies]
gradatum-search = "2.1.0"
```

```rust
use gradatum_search::{RrfHit, rrf_fuse};
// Ranked (note_id, score) pairs from SqliteIndex.
let bm25: Vec<(String, f64)> = /* BM25 hits */ vec![];
let semantic: Vec<(String, f32)> = /* cosine hits */ vec![];
let fused: Vec<RrfHit> = rrf_fuse(&bm25, &semantic, 60.0, 10); // k=60, limit=10
```

For composite scoring:

```rust
use gradatum_search::{composite_score, recency_factor};
```

## Feature Flags

| Feature | Default | Description |
|---|---|---|
| `onnx-reranker` | no | Enables a cross-encoder reranker backed by ONNX Runtime. Adds `ort` and `tokenizers` dependencies. |

## License

Apache-2.0
