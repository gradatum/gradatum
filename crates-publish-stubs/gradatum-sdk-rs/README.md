# gradatum-sdk-rs

> Rust SDK client for the gradatum-server HTTP API.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API (Phase 2.0+)

### Structs

```rust
/// Async Rust client for the gradatum-server HTTP API.
pub struct GradatumClient { ... }

impl GradatumClient {
    /// Create a new client.
    pub fn new(base_url: &str, bearer_token: &str) -> Self

    /// Search notes (BM25 + semantic + RRF fusion).
    pub async fn search(
        &self,
        vault_id: &str,
        query: &str,
        limit: u32,
    ) -> Result<Vec<SearchResult>, SdkError>

    /// Read a note by path.
    pub async fn read_note(
        &self,
        vault_id: &str,
        path: &str,
    ) -> Result<Note, SdkError>

    /// List notes in a section.
    pub async fn list_notes(
        &self,
        vault_id: &str,
        section: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<NoteSummary>, SdkError>

    /// Get vault status.
    pub async fn vault_status(&self, vault_id: &str) -> Result<VaultStatus, SdkError>

    /// Health check.
    pub async fn health(&self) -> Result<(), SdkError>
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    Http(reqwest::Error),
    Api { status: u16, message: String },
    Deserialize(serde_json::Error),
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0 (Phase 2.0 implementation)
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0