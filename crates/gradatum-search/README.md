# gradatum-search

> Hybrid search orchestration: BM25 full-text (SQLite FTS5), semantic vector (cosine), cross-encoder reranking, and RRF fusion.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
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

The crate also re-exports `SqliteIndex` and the seven query methods added in v0.3.x:
`distinct_authors`, `distinct_tags`, `backlinks`, `neighbors`, `trace_lineage`,
`title_lookup`, `get_note`.

## Usage

```toml
[dependencies]
gradatum-search = "0.4.0"
```

```rust
use gradatum_search::SearchEngine;
use std::sync::Arc;

let engine = SearchEngine::new(Arc::clone(&index), Arc::clone(&embedder));
let results = engine.search_unified("agent memory", "main", 10).await?;
```

## License

Apache-2.0
