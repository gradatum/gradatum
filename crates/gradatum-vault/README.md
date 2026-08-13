# gradatum-vault

> Vault domain logic: write pipeline, lifecycle management, metadata overrides, drift detection, and effective-note cache.

**Status**: v2.0.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-vault` is the L2 domain layer that sits above the storage, index, and cache
primitives. It owns the full note write pipeline and vault lifecycle.

Responsibilities:

- **Write pipeline** — computes `ContentHash`, persists the Markdown file via
  `gradatum-storage`, and upserts the note into `gradatum-index`. Curation and embedding
  jobs are enqueued upstream by `gradatum-server` / `gradatum-worker`; this crate depends
  on no queue and enqueues nothing itself.
- **Lifecycle** — `Vault::create` / `Vault::open`: initialises vault layout, sets
  `tenant_id`, and wires storage + index handles.
- **Registry trait** — `Registry` (async trait) decouples the server from the concrete
  `Vault` implementation; exposes `read_note_by_id`, `write_note_with_id_internal`,
  `update_note_status`, `add_tags`, `move_locus`, `history_*`, `delete_note_by_id`,
  and more.
- **Metadata overrides** — `NoteMetadataOverride` applies on top of base frontmatter at
  read time without mutating stored files.
- **Drift detection** — orchestrates the three-level scan (size → prefix-4 KB →
  full SHA-256) via `gradatum-index::drift::scan_phase_a`.
- **Effective-note cache** — wraps the Moka-backed `EffectiveNoteCache` with checksum
  validation on cache hit.

## Usage

```toml
[dependencies]
gradatum-vault = "2.0.0"
```

```rust
use gradatum_vault::registry::Vault;
use gradatum_core::frontmatter::Frontmatter;
use std::path::Path;

// Open an existing vault — loads config and opens the SQLite index.
let vault = Vault::open(Path::new("/var/lib/gradatum")).await?;

// Write a note — returns the complete Note with a generated NoteId.
let frontmatter = Frontmatter::default();
let note = vault.write_note(frontmatter, body).await?;
```

## License

Apache-2.0
