# gradatum-vault

> Vault domain logic: write pipeline, lifecycle management, metadata overrides, drift detection, and effective-note cache.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-vault` is the L2 domain layer that sits above the storage, index, and cache
primitives. It owns the full note write pipeline and vault lifecycle.

Key responsibilities in v0.3.x:

- **Write pipeline** — computes `ContentHash`, persists the Markdown file via
  `gradatum-storage`, upserts the note into `gradatum-index`, and enqueues curation and
  embedding jobs.
- **Lifecycle** — `Vault::create` / `Vault::open`: initialises vault layout, sets
  `tenant_id`, and wires storage + index handles.
- **Metadata overrides** — `NoteMetadataOverride` applies on top of base frontmatter at
  read time without mutating stored files.
- **Drift detection** — orchestrates the Phase A three-level scan (size → prefix-4KB →
  full SHA-256) via `gradatum-index::drift::scan_phase_a`.
- **Effective-note cache** — wraps the Moka-backed `EffectiveNoteCache` with checksum
  validation on cache hit.
- **Downgrade** — moves notes to a trash section for soft-delete and recovery.

## Usage

```toml
[dependencies]
gradatum-vault = "0.4.0"
```

```rust
use gradatum_vault::registry::Vault;
use gradatum_core::frontmatter::Frontmatter;
use std::path::Path;

// Ouverture async — charge la config + ouvre l'index SQLite.
let vault = Vault::open(Path::new("/var/lib/gradatum")).await?;

// Écriture — retourne la Note complète avec NoteId généré.
let frontmatter = Frontmatter::default();
let note = vault.write_note(frontmatter, body).await?;
```

## License

Apache-2.0
