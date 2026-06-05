# gradatum-search

> Multi-mode search orchestration: BM25 full-text, semantic vector, graph traversal, and RRF fusion.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API (Phase 2.0)

### Structs

```rust
/// Search orchestrator — combines BM25, semantic, and graph modes.
pub struct SearchEngine { ... }

impl SearchEngine {
    pub fn new(index: Arc<dyn Index>, embedder: Arc<dyn Embedder>) -> Self

    /// Full-text BM25 search.
    pub async fn search_fts(
        &self,
        query: &str,
        vault_id: &str,
        limit: u32,
    ) -> Result<Vec<SearchResult>, SearchError>

    /// Semantic vector search.
    pub async fn search_semantic(
        &self,
        query: &str,
        vault_id: &str,
        limit: u32,
    ) -> Result<Vec<SearchResult>, SearchError>

    /// RRF fusion of BM25 + semantic results (k=60).
    pub async fn search_unified(
        &self,
        query: &str,
        vault_id: &str,
        limit: u32,
    ) -> Result<Vec<SearchResult>, SearchError>
}

pub struct SearchResult {
    pub note_id: NoteId,
    pub score: f32,
    pub title: Option<String>,
    pub section: SectionId,
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0 (Phase 2.0 implementation)
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0