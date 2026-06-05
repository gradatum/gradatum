# gradatum-core

> Shared primitives: traits, canonical types, errors. The L0 crate every other Gradatum crate depends on.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Modules

```rust
pub mod acl;           // ACL filter types + visibility markers
pub mod audit;         // AuditEntry — immutable append-only audit trail
pub mod author;        // AuthorId — note authorship
pub mod config;        // GradatumConfig — root configuration deserialization
pub mod error;         // GradatumError — typed error enum (thiserror)
pub mod frontmatter;   // Frontmatter struct — YAML frontmatter canonical type
pub mod identity;      // ConsumerId, TenantId — identity primitives
pub mod index;         // Index trait — storage-agnostic index contract
pub mod note;          // Note, NoteId, NoteStatus, ContentHash
pub mod overrides;     // Overridable trait + OverridePayload
pub mod schema_registry; // Schema version negotiation
pub mod scope;         // Scope — JWT audience scopes (read / write / admin)
pub mod section;       // SectionId — vault section identifier
pub mod status;        // NoteStatus enum
pub mod tag;           // Tag — normalized note tag
pub mod trust;         // TrustContext — auth context propagated through layers
```

### Key Types

```rust
// Core error type
#[derive(Debug, thiserror::Error)]
pub enum GradatumError { ... }

// Note identity
pub struct NoteId(Ulid);  // ULID-based note identifier

// Content integrity
pub struct ContentHash(String);  // SHA-256 hex digest

// Trust context propagated through all layers
pub enum TrustContext {
    Unauthenticated,
    Authenticated { consumer_id: ConsumerId, scopes: Vec<Scope> },
    Admin,
}

// Index trait — implemented by gradatum-index::SqliteIndex
pub trait Index: Send + Sync {
    async fn upsert(&self, note: &Note) -> Result<(), GradatumError>;
    async fn search_fts(&self, query: &str, limit: u32) -> Result<Vec<NoteId>, GradatumError>;
    async fn delete(&self, id: &NoteId) -> Result<(), GradatumError>;
}
```

### Multi-tenancy invariant

Every persisted row carries `tenant_id TEXT NOT NULL`.
Default tenant: `"main"`. Aliased to `vault` in user-facing UI/CLI/SDK.
Enforced at storage layer; ACL filters by `tenant_id` first.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0-alpha.3` → `v0.1.0` public
- License : Apache-2.0