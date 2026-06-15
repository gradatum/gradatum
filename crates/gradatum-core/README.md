# gradatum-core

> Shared primitives: traits, canonical types, and typed errors. The L0 foundation every other gradatum crate depends on.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-core` is the dependency floor of the gradatum workspace. It defines the canonical
types (`NoteId`, `ContentHash`, `NoteVersion`), the 11 canonical vault sections (`Section`
enum), the three storage traits (`DocumentStore`, `IndexStore`, `VectorStore`), and the
top-level `GradatumError` enum (typed with `thiserror`, no `Box<dyn Error>`).

Every other gradatum crate depends on `gradatum-core`; `gradatum-core` itself has zero
workspace dependencies.

## Usage

```toml
[dependencies]
gradatum-core = "0.4.3"
```

```rust
use gradatum_core::error::GradatumError;
use gradatum_core::note::Note;
use gradatum_core::identity::NoteId;
use gradatum_core::section::Section;
```

## Key Modules

| Module | Contents |
|---|---|
| `error` | `GradatumError` — typed error enum (thiserror, no `Box<dyn Error>`) |
| `note` | `Note`, `NoteBody`, `EffectiveNote` |
| `identity` | `NoteId` (ULID newtype), `ContentHash`, `NoteVersion` |
| `section` | `Section` — 11 canonical sections (kebab-case, serde) |
| `tag` | `Tag` — normalized kebab-case note tag |
| `author` | `AuthorId` — note authorship |
| `status` | `NoteStatus` — note lifecycle state machine |
| `trust` | `TrustContext` — auth context propagated through layers |
| `scope` | `Scope` — JWT audience scopes (read / write / admin) |
| `document_store` | `DocumentStore` trait — raw Markdown persistence |
| `index_store` | `IndexStore` trait — SQLite full-text + vector index |
| `vector_store` | `VectorStore` trait — embedding storage |
| `config` | `VaultConfig` — root configuration deserialization |
| `frontmatter` | `Frontmatter` — YAML frontmatter canonical type |
| `acl` | ACL filter types and visibility markers |
| `audit` | `AuditEntry` — immutable append-only audit trail |

## License

Apache-2.0
