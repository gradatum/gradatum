# gradatum-core

> Shared primitives: traits, canonical types, and typed errors. The L0 foundation every other gradatum crate depends on.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-core` is the dependency floor of the gradatum workspace. It defines the canonical
types (`NoteId`, `ContentHash`, `NoteVersion`), the 14 canonical vault sections (`Section`
enum), the three storage traits (`DocumentStore`, `IndexStore`, `VectorStore`), and the
top-level `GradatumError` enum (typed with `thiserror`, no `Box<dyn Error>`).

Every gradatum crate that manipulates notes, jobs or storage depends on `gradatum-core`;
`gradatum-core` itself has zero workspace dependencies.

## Usage

```toml
[dependencies]
gradatum-core = "2.1.0"
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
| `section` | `Section` — 14 canonical sections (kebab-case, serde) |
| `tag` | `Tag` — normalized kebab-case note tag |
| `author` | `AuthorKind`, `AuthorRef` — note authorship |
| `status` | `NoteStatus` — note lifecycle state machine |
| `trust` | `TrustContext` — auth context propagated through layers |
| `scope` | `VaultId`, `TenantId`, `LocusId`, `BearerId` — validated identifier newtypes; `VaultGrant`, `GrantAccess`, `TenantStatus`, `AclCheckedVaultId` — multi-vault access model |
| `document_store` | `DocumentStore` trait — raw Markdown persistence |
| `index_store` | `IndexStore` trait — SQLite full-text + vector index |
| `vector_store` | `VectorStore` trait — embedding storage |
| `metric_sample` | `MetricSamplePoint { series: String, ts_ms: i64, value: f64 }` — timeseries point returned by `IndexStore` metric methods |
| `config` | `VaultConfig` — root configuration deserialization |
| `frontmatter` | `Frontmatter` — YAML frontmatter canonical type |
| `audit` | `AuditEvent`, `AuditEventType` — append-only audit trail (application-level; no hash chain or signature — see SECURITY.md) |
| `job` | `Job` enum, `JobSpec`, `JobRecord`, `JobStatus`, `ValidateSpec`, `QueueStore` trait, and all job-pipeline types |
| `temporal_query` | `TimelineFilter`, `TimelineCursor`, `TimelineRow`, `parse_temporal_str_as_ms` — time-range queries over the index |
| `provenance` | Provenance metadata and trust-scoring fields attached to notes |
| `project_map` | Typed wikilink schema validator for project-map notes |
| `soul` | Agent soul / persona schema types |
| `paths` | Canonical path helpers (`vault_index_path`, `vault_dir_index_path`, `queue_db_path`) |

### `IndexStore` metric methods

Four methods added to the `IndexStore` trait for the curated metrics timeseries pipeline.
Default implementations are no-ops (`Ok(0)` / `Ok(vec![])`) so mock backends compile without change.

| Method | Signature (simplified) | Purpose |
|---|---|---|
| `insert_metric_samples` | `(ts_ms: i64, samples: &[(String, f64)]) -> Result<usize>` | Batch-insert one tick of curated samples |
| `query_metric_timeseries` | `(series: &[String], from_ms: i64, to_ms: i64, bucket_ms: i64) -> Result<Vec<MetricSamplePoint>>` | Range query with server-side downsample |
| `purge_metric_samples` | `(cutoff_ms: i64) -> Result<usize>` | Delete samples older than cutoff |
| `list_distinct_metric_series` | `() -> Result<Vec<String>>` | Catalog of series present in the table |

## License

Apache-2.0
