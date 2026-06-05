#!/usr/bin/env bash
#
# gen-stubs-v0.0.1.sh — Génère les 26 stubs v0.0.1 avec README enrichis (signatures publiques).
#
# Chaque stub est un mini-projet Cargo ISOLÉ dans crates-publish-stubs/<name>/
# NE CONTIENT PAS le code source réel (D5 criterion — repo privé jusqu'à v1.0).
# Le lib.rs est minimal avec la doc-string officielle du crate.
#
# Usage:
#   bash scripts/gen-stubs-v0.0.1.sh
#
# Génère : crates-publish-stubs/<name>/  (Cargo.toml + src/lib.rs|main.rs + README.md)
# Puis utiliser scripts/publish-stubs-v0.0.1.sh pour publier.

set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STUBS_DIR="${WORKSPACE_ROOT}/crates-publish-stubs"

readonly AUTHOR='Gradatum Maintainers <maintainer@gradatum.org>'
readonly LICENSE="Apache-2.0"
readonly REPO_URL="https://github.com/gradatum/gradatum"
readonly VERSION="0.0.1"
readonly STATUS_LINE="**Status** : Alpha — placeholder \`v0.0.1\`. Source code private until \`v1.0\` public release. See [gradatum.org](https://gradatum.org) for project context."
readonly PART_OF="**Part of [\`gradatum\`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents."
readonly ROADMAP="- Roadmap : Phase 2.0b → \`v0.1.0-alpha.3\` → \`v0.1.0-beta\` (post-Phase 2.1) → \`v0.1.0\` public"

mkdir -p "$STUBS_DIR"

echo "==> Génération stubs v${VERSION} dans ${STUBS_DIR}/"
echo

# ────────────────────────────────────────────────────────────────────────────
# Helper : crée Cargo.toml pour un lib crate
# ────────────────────────────────────────────────────────────────────────────
make_lib_cargo_toml() {
    local name="$1"
    local description="$2"
    cat <<EOF
[package]
name = "${name}"
version = "${VERSION}"
edition = "2021"
authors = ["${AUTHOR}"]
license = "${LICENSE}"
description = "${description}"
repository = "${REPO_URL}"
homepage = "https://gradatum.org"
readme = "README.md"
keywords = ["memory", "agents", "knowledge-base", "markdown", "embedded"]
categories = ["database", "memory-management"]

[lib]
path = "src/lib.rs"

# Déclare explicitement que ce mini-crate n'appartient pas au workspace parent.
# Sans cela, cargo remonte jusqu'à /home/maintainer-user/projects/gradatum/Cargo.toml
# et refuse de compiler "outside workspace".
[workspace]
EOF
}

# ────────────────────────────────────────────────────────────────────────────
# Helper : crée Cargo.toml pour un binary crate
# ────────────────────────────────────────────────────────────────────────────
make_bin_cargo_toml() {
    local name="$1"
    local description="$2"
    cat <<EOF
[package]
name = "${name}"
version = "${VERSION}"
edition = "2021"
authors = ["${AUTHOR}"]
license = "${LICENSE}"
description = "${description}"
repository = "${REPO_URL}"
homepage = "https://gradatum.org"
readme = "README.md"
keywords = ["memory", "agents", "knowledge-base", "cli", "server"]
categories = ["command-line-utilities"]

[[bin]]
name = "${name}"
path = "src/main.rs"

# Déclare explicitement que ce mini-crate n'appartient pas au workspace parent.
[workspace]
EOF
}

# ────────────────────────────────────────────────────────────────────────────
# Helper : lib.rs minimal avec doc-comment officiel
# ────────────────────────────────────────────────────────────────────────────
make_lib_rs() {
    local name="$1"
    local doc="$2"
    cat <<EOF
//! ${doc}
//!
//! ## Status
//!
//! Placeholder v${VERSION}. Source code private until v1.0 public release.
//! See <https://gradatum.org> for project context and roadmap.
//!
//! ## Stability
//!
//! \`0.x\` — no API stability guarantee.
//! See the [versioning policy](${REPO_URL}/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
EOF
}

# ────────────────────────────────────────────────────────────────────────────
# Helper : main.rs minimal pour les binaires
# ────────────────────────────────────────────────────────────────────────────
make_main_rs() {
    local name="$1"
    local doc="$2"
    cat <<EOF
//! ${doc}
//!
//! ## Status
//!
//! Placeholder v${VERSION}. Source code private until v1.0 public release.
//! See <https://gradatum.org> for project context and roadmap.

fn main() {
    println!(
        "${name} v{} — placeholder. See https://gradatum.org for roadmap.",
        env!("CARGO_PKG_VERSION")
    );
}
EOF
}

# ────────────────────────────────────────────────────────────────────────────
# Création d'un stub lib
# ────────────────────────────────────────────────────────────────────────────
create_lib_stub() {
    local name="$1"
    local description="$2"
    local readme="$3"
    local lib_doc="${4:-${description}}"

    local dir="${STUBS_DIR}/${name}"
    mkdir -p "${dir}/src"
    make_lib_cargo_toml "$name" "$description" > "${dir}/Cargo.toml"
    make_lib_rs "$name" "$lib_doc" > "${dir}/src/lib.rs"
    printf '%s' "$readme" > "${dir}/README.md"
    echo "  [LIB] ${name}"
}

# ────────────────────────────────────────────────────────────────────────────
# Création d'un stub binary
# ────────────────────────────────────────────────────────────────────────────
create_bin_stub() {
    local name="$1"
    local description="$2"
    local readme="$3"
    local main_doc="${4:-${description}}"

    local dir="${STUBS_DIR}/${name}"
    mkdir -p "${dir}/src"
    make_bin_cargo_toml "$name" "$description" > "${dir}/Cargo.toml"
    make_main_rs "$name" "$main_doc" > "${dir}/src/main.rs"
    printf '%s' "$readme" > "${dir}/README.md"
    echo "  [BIN] ${name}"
}

# ============================================================================
# 1. gradatum — umbrella SDK facade
# ============================================================================
create_lib_stub "gradatum" \
    "Umbrella SDK facade — re-exports curated subsets of focused crates via Cargo features" \
    "$(cat <<'README'
# gradatum

> Umbrella SDK facade — re-exports curated subsets of focused crates via Cargo features for downstream ergonomics.

**Status** : Alpha — placeholder \`v0.0.1\`. Source code private until \`v1.0\` public release. See [gradatum.org](https://gradatum.org) for project context.

**Memory backbone for AI agents — graduated.**

## Feature Flags

| Feature | Crates re-exported | Usage |
|---|---|---|
| `core` | `gradatum-core` | Shared primitives (always available) |
| `client` | `gradatum-sdk-rs` | Rust SDK for HTTP API integration |

## Public API

```rust
// Version constant
pub const VERSION: &str = "...";

// Re-exports (feature-gated)
#[cfg(feature = "core")]
pub use gradatum_core as core;

#[cfg(feature = "client")]
pub use gradatum_sdk_rs as sdk;
```

## Usage

```rust
[dependencies]
gradatum = { version = "0.0.1", features = ["core"] }
```

```rust
use gradatum::core::error::GradatumError;
```

## Crates in the Gradatum ecosystem

| Crate | Role |
|---|---|
| [`gradatum-core`](https://crates.io/crates/gradatum-core) | Shared primitives: errors, IDs, types |
| [`gradatum-markdown`](https://crates.io/crates/gradatum-markdown) | Parse/serialize MD + frontmatter + wikilinks |
| [`gradatum-index`](https://crates.io/crates/gradatum-index) | SQLite + FTS5 index layer |
| [`gradatum-storage`](https://crates.io/crates/gradatum-storage) | Storage trait + OpenDAL backends |
| [`gradatum-vault`](https://crates.io/crates/gradatum-vault) | Multi-vault registry + lifecycle |
| [`gradatum-cache`](https://crates.io/crates/gradatum-cache) | Moka LRU in-process cache |
| [`gradatum-embed`](https://crates.io/crates/gradatum-embed) | Embedder trait + HTTP/CPU backends |
| [`gradatum-chat`](https://crates.io/crates/gradatum-chat) | Chat trait + LLM backends + circuit breaker |
| [`gradatum-curator`](https://crates.io/crates/gradatum-curator) | LLM-powered note curation workflow |
| [`gradatum-search`](https://crates.io/crates/gradatum-search) | BM25 + semantic + RRF fusion search |
| [`gradatum-auth`](https://crates.io/crates/gradatum-auth) | JWT (Ed25519) + OIDC + API-key |
| [`gradatum-acl-policy`](https://crates.io/crates/gradatum-acl-policy) | ACL policy engine — deny-wins |
| [`gradatum-acl-auth`](https://crates.io/crates/gradatum-acl-auth) | Bearer verification + scope enforcement |
| [`gradatum-queue`](https://crates.io/crates/gradatum-queue) | SQLite-backed jobs queue with atomic leases |
| [`gradatum-engine`](https://crates.io/crates/gradatum-engine) | On-device inference (candle / llama.cpp) |
| [`gradatum-server`](https://crates.io/crates/gradatum-server) | HTTP/MCP facade :19090 |
| [`gradatum-worker`](https://crates.io/crates/gradatum-worker) | Async queue consumer |
| [`gradatum-admin`](https://crates.io/crates/gradatum-admin) | CLI ops: init/migrate/backup/restore |
| [`gradatum-cli`](https://crates.io/crates/gradatum-cli) | End-user CLI: read/write/search |
| [`gradatum-mcp-stub`](https://crates.io/crates/gradatum-mcp-stub) | MCP stdio → HTTP proxy |
| [`gradatum-sdk-rs`](https://crates.io/crates/gradatum-sdk-rs) | Rust SDK for HTTP API integration |

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0 (early access via maintainer)
- Roadmap : Phase 2.0b → `v0.1.0-alpha.3` → `v0.1.0-beta` → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 2. gradatum-core — shared primitives
# ============================================================================
create_lib_stub "gradatum-core" \
    "Shared primitives: errors, IDs, types" \
    "$(cat <<'README'
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
README
)"

# ============================================================================
# 3. gradatum-markdown
# ============================================================================
create_lib_stub "gradatum-markdown" \
    "Parse/serialize MD + frontmatter YAML + wikilinks extraction" \
    "$(cat <<'README'
# gradatum-markdown

> Parse and serialize Markdown notes with YAML frontmatter and wikilink extraction.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Functions

```rust
/// Parse a Markdown file (YAML frontmatter + body) into a ParsedNote.
pub fn parse(src: &str) -> Result<ParsedNote, MarkdownError>

/// Serialize a ParsedNote back to on-disk Markdown format.
pub fn write_parsed(note: &ParsedNote) -> String

/// Serialize a full gradatum_core::note::Note to Markdown.
pub fn write(note: &Note) -> String

/// Extract all [[wikilinks]] from a Markdown body.
pub fn wikilinks(body: &str) -> Vec<Wikilink>
```

### Structs

```rust
pub struct ParsedNote {
    pub frontmatter: Frontmatter,
    pub body: String,
}

pub struct Wikilink {
    pub target: String,
    pub alias: Option<String>,
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum MarkdownError {
    MissingFrontmatter,
    FrontmatterParse(serde_yaml::Error),
    InvalidSchemaVersion { found: u32, expected: u32 },
}
```

## On-disk format

```
---
schema_version: 1
vault_id: main
section: decisions
status: live
created: "2026-05-04T11:00:00Z"
---

# Note title

Body with [[wikilinks]] and [[target|alias]] support.
```

## Round-trip guarantee

`parse(write_parsed(parse(x)?))` is semantically equivalent to `parse(x)` (1-cycle idempotence on values, not exact text representation).

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 4. gradatum-storage
# ============================================================================
create_lib_stub "gradatum-storage" \
    "Storage trait + OpenDAL backends + NFS reject guard (caveat C11)" \
    "$(cat <<'README'
# gradatum-storage

> Storage trait abstraction with OpenDAL backends (filesystem, S3, Azure Blob) and NFS rejection guard.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Trait

```rust
/// Storage abstraction — async read/write/list/delete/stat primitives.
#[async_trait]
pub trait Storage: Send + Sync {
    async fn read(&self, path: &str) -> Result<Bytes, StorageError>;
    async fn write(&self, path: &str, data: Bytes) -> Result<(), StorageError>;
    async fn delete(&self, path: &str) -> Result<(), StorageError>;
    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, StorageError>;
    async fn exists(&self, path: &str) -> Result<bool, StorageError>;
    async fn stat(&self, path: &str) -> Result<StorageEntry, StorageError>;
}
```

### Implementations

```rust
/// Filesystem backend via OpenDAL (feature = "fs", enabled by default).
pub struct FileStorage { ... }

impl FileStorage {
    /// Create a new FileStorage rooted at `root`.
    /// Returns Err if root is on an NFS mount (caveat C11).
    pub fn new(root: &Path) -> Result<Self, StorageError>
}
```

### Functions

```rust
/// Verify via statfs(2) that `path` is not on an NFS mount.
/// Called automatically by FileStorage::new().
pub fn ensure_local_filesystem(path: &Path) -> Result<(), StorageError>
```

### Types

```rust
pub struct StorageEntry {
    pub path: String,
    pub size: u64,
    pub last_modified: Option<DateTime<Utc>>,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    Io(std::io::Error),
    Core(GradatumError),
    NfsMountDetected { path: PathBuf },
    Backend(String),
}
```

## Feature flags

| Feature | Description | Default |
|---|---|---|
| `fs` | OpenDAL filesystem backend (`FileStorage`) | enabled |
| `s3` | S3 backend (Phase 2+, not yet implemented) | disabled |
| `azblob` | Azure Blob backend (Phase 2+, not yet implemented) | disabled |

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 5. gradatum-index
# ============================================================================
create_lib_stub "gradatum-index" \
    "SQLite + FTS5 + drift detection Phase A — index layer Gradatum Phase 1" \
    "$(cat <<'README'
# gradatum-index

> SQLite + FTS5 index layer with three-level drift detection (Phase A).

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Structs

```rust
/// SQLite + FTS5 implementation of gradatum_core::index::Index.
/// Applies 4 mandatory PRAGMAs on open: WAL, synchronous=NORMAL,
/// busy_timeout=5000, foreign_keys=ON.
pub struct SqliteIndex { ... }

impl SqliteIndex {
    /// Open (or create) an index at the given SQLite file path.
    pub async fn open(path: &Path) -> Result<Self, IndexError>

    /// Open an in-memory index (tests / ephemeral use).
    pub async fn open_in_memory() -> Result<Self, IndexError>
}
```

### Drift detection

```rust
/// Three-level drift scan (Phase A).
///
/// Level 1 — file size check (fast, no I/O).
/// Level 2 — first 4KB prefix hash.
/// Level 3 — full SHA-256 (only when Level 2 mismatch).
///
/// Returns the list of note IDs whose on-disk content diverges from index.
pub async fn scan_phase_a(
    index: &SqliteIndex,
    storage: &dyn Storage,
) -> Result<Vec<NoteId>, IndexError>
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 6. gradatum-vault
# ============================================================================
create_lib_stub "gradatum-vault" \
    "Multi-vault registry + lifecycle (create/list/swap/delete) + forward-compat" \
    "$(cat <<'README'
# gradatum-vault

> Multi-vault registry, lifecycle management (create/list/swap/delete), note write pipeline, and drift orchestration.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Structs

```rust
/// Top-level vault handle — registry + lifecycle operations.
pub struct Vault { ... }

impl Vault {
    /// Create a new vault at `root` with `tenant_id`.
    pub fn create(root: &Path, tenant_id: &str) -> Result<Self, VaultError>

    /// Open an existing vault at `root`.
    pub fn open(root: &Path) -> Result<Self, VaultError>

    /// Write a note to the vault (ContentHash + persist .md + upsert index).
    pub async fn write_note(&self, note: Note) -> Result<NoteId, VaultError>

    /// Read a note by ID from the vault.
    pub async fn read_note(&self, id: &NoteId) -> Result<Note, VaultError>

    /// Trigger Phase A drift check.
    pub async fn drift_check(&self) -> Result<Vec<NoteId>, VaultError>
}

/// Metadata override — applies on top of base frontmatter at read time.
pub struct NoteMetadataOverride { ... }

/// Immutable history entry for a note (scaffold Phase 1).
pub struct NoteHistoryEntry {
    pub note_id: NoteId,
    pub timestamp: DateTime<Utc>,
    pub content_hash: ContentHash,
    pub author: Option<AuthorId>,
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    Storage(StorageError),
    Index(IndexError),
    Markdown(MarkdownError),
    Core(GradatumError),
    NoteNotFound(NoteId),
    VaultAlreadyExists(PathBuf),
    Io(std::io::Error),
}
```

## Architecture (L2)

```
Vault (L2)
├── gradatum-core    (L0) — primitives, traits, errors
├── gradatum-markdown (L1) — parse/write .md
├── gradatum-cache   (L1) — EffectiveNoteCache (moka)
├── gradatum-index   (L1) — SqliteIndex
└── gradatum-storage (L1) — FileStorage (OpenDAL)
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 7. gradatum-cache
# ============================================================================
create_lib_stub "gradatum-cache" \
    "Moka LRU in-process cache, key=(vault_id, query_hash)" \
    "$(cat <<'README'
# gradatum-cache

> Moka LRU in-process cache with checksum validation on hit. Implements D-perf-2 / B22 spec §6.1.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Structs

```rust
/// Cache key: (vault_id, content_hash).
pub struct CacheKey {
    pub vault_id: String,
    pub content_hash: ContentHash,
}

/// Configuration for EffectiveNoteCache.
pub struct EffectiveNoteCacheConfig {
    pub max_capacity: u64,   // max entries (default: 1000)
    pub ttl_secs: u64,       // entry TTL (default: 300)
}

/// Moka-backed cache for EffectiveNote with checksum validation.
///
/// On cache hit: caller provides an async validator returning the current hash
/// from SQLite. If match → returns cached value. If mismatch → invalidates + returns None.
pub struct EffectiveNoteCache { ... }

impl EffectiveNoteCache {
    pub fn new(config: EffectiveNoteCacheConfig) -> Self

    /// Get a cached note, validating freshness via the provided async validator.
    pub async fn get<F, Fut>(
        &self,
        key: &CacheKey,
        validator: F,
    ) -> Option<Arc<EffectiveNote>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Option<ContentHash>>,

    pub fn insert(&self, key: CacheKey, note: Arc<EffectiveNote>, hash: ContentHash)

    pub fn invalidate(&self, key: &CacheKey)
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 8. gradatum-embed
# ============================================================================
create_lib_stub "gradatum-embed" \
    "Trait Embedder + HTTP remote backends + embedder_id/dim invariants (local impl provided by gradatum-engine)" \
    "$(cat <<'README'
# gradatum-embed

> `Embedder` trait with HTTP and CPU backends, fallback decorator. Local inference via [`gradatum-engine`](https://crates.io/crates/gradatum-engine).

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Trait

```rust
/// Embedding backend — produces fixed-dimension float vectors.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Unique identifier for this backend (e.g. "bge-small-en-v1.5-cpu").
    fn embedder_id(&self) -> &str;

    /// Output vector dimension. Must be consistent across all calls.
    fn dim(&self) -> usize;

    /// Embed a batch of texts. Returns one vector per input.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

pub trait EmbedBackend: Embedder {}
```

### Implementations

```rust
/// HTTP OpenAI-compatible /v1/embeddings backend (any embedding server, e.g. bge-m3, dim=1024).
pub struct HttpEmbedder { ... }

impl HttpEmbedder {
    pub fn new(base_url: &str, model: &str, bearer: Option<&str>) -> Self
}

/// Local CPU inference via fastembed (ONNX). Feature-gated.
/// feature = "fastembed-cpu" (disabled by default).
#[cfg(feature = "fastembed-cpu")]
pub struct FastEmbedCpu { ... }

/// No-op embedder — returns zero vectors (tests / disabled state).
pub struct Noop { dim: usize }

/// Decorator: tries primary, falls back to secondary on error.
/// Implements circuit-breaker pattern.
pub struct FallbackEmbedder<P: Embedder, F: Embedder> { ... }
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    Http(reqwest::Error),
    DimMismatch { expected: usize, got: usize },
    EmptyBatch,
    Backend(String),
}
```

## Feature flags

| Feature | Description | Default |
|---|---|---|
| `fastembed-cpu` | ONNX local inference via fastembed | disabled |

## Anti-cycle invariant

`gradatum-embed` MUST NOT depend on `gradatum-engine`.
`gradatum-engine` MAY depend on `gradatum-embed`.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 9. gradatum-chat
# ============================================================================
create_lib_stub "gradatum-chat" \
    "Trait Chat + 3 impls (Heuristic/HttpChat/Noop) + CircuitBreaker — OpenAI-compatible backend pour curator LLM gating" \
    "$(cat <<'README'
# gradatum-chat

> `Chat` trait with heuristic, HTTP (OpenAI-compatible), and no-op backends plus circuit breaker decorator.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Trait

```rust
/// LLM chat backend for curator gating decisions.
#[async_trait]
pub trait Chat: Send + Sync {
    /// Classify a note in context — returns curator verdict + confidence.
    async fn classify_curator(
        &self,
        note: &Note,
        ctx: &CuratorContext,
    ) -> Result<(CuratorVerdict, f32), ChatError>;
}

pub trait ChatBackend: Chat {}
```

### Types

```rust
pub enum CuratorVerdict {
    Admit,
    Route { section: SectionId },
    Retire,
    Defer,
}

pub struct CuratorContext {
    pub vault_id: String,
    pub existing_sections: Vec<SectionId>,
}
```

### Implementations

```rust
/// Rule-based heuristic classifier — no network dependency (invariant #3 / R1).
pub struct Heuristic { ... }

/// OpenAI-compatible HTTP backend (local inference server / gateway-v2).
pub struct HttpChat { ... }

impl HttpChat {
    pub fn new(base_url: &str, model: &str, bearer: Option<&str>) -> Self
}

/// No-op backend — always returns Defer with confidence 0.0 (tests / disabled).
pub struct Noop;

/// Circuit breaker decorator: opens after N consecutive failures, resets after cooldown.
pub struct CircuitBreakerChat<C: Chat> { ... }

impl<C: Chat> CircuitBreakerChat<C> {
    pub fn new(inner: C, failure_threshold: u32, cooldown: Duration) -> Self
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    Http(reqwest::Error),
    Serialization(serde_json::Error),
    CircuitOpen,
    Backend(String),
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 10. gradatum-curator
# ============================================================================
create_lib_stub "gradatum-curator" \
    "LLM-powered note curation layer: admits, routes, classifies, organises and retires notes within a vault" \
    "$(cat <<'README'
# gradatum-curator

> LLM-powered note curation: heuristic-first gating with optional LLM review for low-confidence notes.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Structs

```rust
/// Main curator workflow — generic over Chat backend.
pub struct Curator<C: Chat> { ... }

impl<C: Chat> Curator<C> {
    pub fn new(chat: C, config: CuratorConfig) -> Self

    /// Evaluate a note: heuristic → (optional) LLM → decision.
    pub async fn decide(
        &self,
        note: &Note,
        ctx: &CuratorContext,
    ) -> Result<CuratorDecision, CuratorError>
}
```

### Types

```rust
pub enum CuratorDecision {
    Admit { section: SectionId },
    Route { from: SectionId, to: SectionId },
    Retire,
    Defer,
}

pub enum FallbackStrategy {
    /// Use heuristic verdict on LLM error.
    UseHeuristic,
    /// Defer the note on LLM error.
    Defer,
    /// Fail hard on LLM error.
    Fail,
}

pub struct CuratorConfig {
    pub confidence_threshold: f32,   // default: 0.85
    pub llm_review_enabled: bool,    // default: true
    pub fallback: FallbackStrategy,  // default: UseHeuristic
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum CuratorError {
    Chat(ChatError),
    Core(GradatumError),
}
```

## Offline-first invariant

The heuristic runs first, always, with no network dependency (invariant #3 / R1).
LLM is only called for low-confidence notes when `llm_review_enabled = true`.

## Workflow

```
Curator::decide(note, ctx)
  step 1: Heuristic::classify_curator(note, ctx)
  step 2: confidence > threshold → fast path (heuristic verdict)
  step 3: llm_review_enabled → C::classify_curator(note, ctx)
  step 4: LLM error → FallbackStrategy applied
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 11. gradatum-auth
# ============================================================================
create_lib_stub "gradatum-auth" \
    "JWT (Ed25519, audience-scoped, mandatory kid) + OIDC + API-key" \
    "$(cat <<'README'
# gradatum-auth

> JWT verification (Ed25519, audience-scoped, mandatory `kid`), OIDC integration, and API-key support.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Modules

```rust
pub mod jwt;        // JWT Ed25519 verification + claims
pub mod revocation; // RevocationStore — in-memory + SQLite-backed jti revocation
```

### JWT

```rust
/// Verify a JWT bearer token against the Ed25519 public key.
/// Validates: signature, expiry, audience, mandatory `kid` claim.
pub fn verify_jwt(
    token: &str,
    public_key: &Ed25519PublicKey,
    expected_audience: &str,
) -> Result<Claims, AuthError>

pub struct Claims {
    pub sub: String,           // consumer_id
    pub aud: String,           // audience scope
    pub exp: u64,              // expiry (unix timestamp)
    pub jti: String,           // unique token ID (for revocation)
    pub kid: String,           // key ID (mandatory)
    pub scopes: Vec<Scope>,    // granted scopes
}
```

### Revocation

```rust
/// Token revocation store (blocks reuse of revoked JTIs).
pub trait RevocationStore: Send + Sync {
    async fn revoke(&self, jti: &str, expires_at: u64) -> Result<(), AuthError>;
    async fn is_revoked(&self, jti: &str) -> Result<bool, AuthError>;
    async fn purge_expired(&self) -> Result<u64, AuthError>;
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    InvalidSignature,
    Expired,
    InvalidAudience,
    MissingKid,
    Revoked(String),
    Decode(String),
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 12. gradatum-acl-policy
# ============================================================================
create_lib_stub "gradatum-acl-policy" \
    "AclPolicy trait + globset matching + ACLFilter visibility marker" \
    "$(cat <<'README'
# gradatum-acl-policy

> ACL policy engine with globset pattern matching, deny-wins semantics, and personal-classified circuit breaker.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Enums

```rust
/// ACL operation being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclOp {
    Read,
    Write,
}

/// Result of an ACL policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclDecision {
    /// Access granted.
    Allow,
    /// Explicit deny — negation pattern matched or personal-classified circuit breaker (B3).
    DenyExplicit,
    /// Implicit deny — no allow pattern matched (default-deny B2).
    DenyImplicit,
}
```

### Structs

```rust
/// Compiled ACL policy loaded from a TOML preset.
pub struct AclPolicy { ... }

impl AclPolicy {
    /// Load and compile a policy from TOML bytes.
    pub fn from_toml(toml: &str) -> Result<Self, AclError>

    /// Evaluate the policy for a given consumer, locus, and operation.
    pub fn evaluate(
        &self,
        consumer_id: &str,
        trust: &TrustContext,
        locus: &str,
        op: AclOp,
    ) -> AclDecision
}
```

### Evaluation order (priority descending)

1. `TrustContext::Unauthenticated` → `DenyImplicit` (immediate)
2. Unknown identity (no consumer match) → `DenyImplicit`
3. B3: `personal-classified` bypass → `DenyExplicit`
4. Negation pattern (`!glob`) match → `DenyExplicit` (deny-wins)
5. Allow pattern match → `Allow`
6. Default → `DenyImplicit`

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum AclError {
    Toml(toml::de::Error),
    Glob(globset::Error),
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 13. gradatum-acl-auth
# ============================================================================
create_lib_stub "gradatum-acl-auth" \
    "argon2id bearer verification + scope enforcement" \
    "$(cat <<'README'
# gradatum-acl-auth

> Argon2id bearer credential verification and scope enforcement per vault.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Functions

```rust
/// Verify a bearer token against its argon2id hash.
/// Constant-time comparison to prevent timing attacks.
pub fn verify_bearer(
    token: &str,
    hash: &str,
) -> Result<bool, AclAuthError>

/// Hash a new bearer token with argon2id (m=65536, t=3, p=4).
pub fn hash_bearer(token: &str) -> Result<String, AclAuthError>

/// Enforce that a TrustContext carries the required scope.
pub fn enforce_scope(
    trust: &TrustContext,
    required: Scope,
) -> Result<(), AclAuthError>
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum AclAuthError {
    InsufficientScope { required: Scope, granted: Vec<Scope> },
    Unauthenticated,
    HashError(String),
    InvalidHash,
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 14. gradatum-queue
# ============================================================================
create_lib_stub "gradatum-queue" \
    "SQLite-backed jobs queue with atomic UPDATE...RETURNING leases" \
    "$(cat <<'README'
# gradatum-queue

> SQLite-backed durable job queue with atomic lease acquisition and automatic recovery.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Structs

```rust
/// Durable job queue backed by SQLite.
/// Guarantees: atomic claim (UPDATE…RETURNING), lease recovery, 4 mandatory PRAGMAs.
pub struct Queue { ... }

impl Queue {
    /// Open (or create) a queue at the given SQLite file path.
    pub async fn open(path: &Path) -> Result<Self, QueueError>

    /// Open an in-memory queue (tests / ephemeral use).
    pub async fn open_in_memory() -> Result<Self, QueueError>

    /// Enqueue a job. Returns the job ULID.
    pub async fn enqueue(
        &self,
        job_type: &str,
        payload: &str,
    ) -> Result<String, QueueError>

    /// Claim the next available job with a lease of `lease_ms` milliseconds.
    /// Returns None if no job is available.
    pub async fn claim_one(
        &self,
        lease_ms: u64,
    ) -> Result<Option<Job>, QueueError>

    /// Mark a job as completed (removes from queue).
    pub async fn complete(&self, id: &str) -> Result<(), QueueError>

    /// Mark a job as failed (increments attempts, releases lease).
    pub async fn fail(&self, id: &str, reason: &str) -> Result<(), QueueError>

    /// Recover expired leases (called periodically by gradatum-worker).
    pub async fn recover_expired(&self) -> Result<u64, QueueError>
}

pub struct Job {
    pub id: String,          // ULID
    pub job_type: String,
    pub payload: String,     // JSON
    pub attempts: u32,
    pub created_at: DateTime<Utc>,
    pub leased_until: DateTime<Utc>,
}

pub enum JobStatus {
    Pending,
    Leased { until: DateTime<Utc> },
    Completed,
    Failed { reason: String, attempts: u32 },
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    UlidParse(ulid::DecodeError),
}
```

## Guarantees

- **Atomic claim**: `UPDATE…RETURNING` ensures at-most-one consumer per job under concurrency.
- **Lease recovery**: expired leases automatically become claimable; `attempts` is incremented.
- **4 PRAGMAs on open**: `WAL`, `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 15. gradatum-search
# ============================================================================
create_lib_stub "gradatum-search" \
    "Multi-mode reader (BM25 + semantic + graph) + RRF fusion" \
    "$(cat <<'README'
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
README
)"

# ============================================================================
# 16. gradatum-engine
# ============================================================================
create_lib_stub "gradatum-engine" \
    "On-device inference adapter: provides Chat, Embedder and Reranker trait implementations backed by a shared local compute stack (candle / llama.cpp); optional via feature gate engine-local" \
    "$(cat <<'README'
# gradatum-engine

> On-device inference adapter: provides `Chat`, `Embedder`, and `Reranker` trait implementations backed by candle or llama.cpp. Optional via `engine-local` feature gate.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API (Phase 1+)

### Feature flags

| Feature | Description | Default |
|---|---|---|
| `engine-local` | Enables local inference backends (candle / llama.cpp) | disabled |

### Implementations (feature = `engine-local`)

```rust
/// Chat implementation backed by local llama.cpp model.
pub struct LocalChat { ... }

/// Embedder implementation backed by local candle model (bge-small-en-v1.5).
pub struct LocalEmbedder { ... }

/// Reranker implementation backed by local cross-encoder model.
pub struct LocalReranker { ... }
```

## Anti-cycle invariant

`gradatum-engine` MAY depend on `gradatum-chat` and `gradatum-embed`.
`gradatum-chat` and `gradatum-embed` MUST NOT depend on `gradatum-engine`.
Composition happens at binary level (`gradatum-server`, `gradatum-worker`).

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 1+ (candle CPU benchmarked at 17ms/embed on a modern CPU)
- License : Apache-2.0
README
)"

# ============================================================================
# 17. gradatum-sdk-rs
# ============================================================================
create_lib_stub "gradatum-sdk-rs" \
    "Rust SDK for direct integration with gradatum-server HTTP API" \
    "$(cat <<'README'
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
README
)"

# ============================================================================
# 18. gradatum-server (BINARY)
# ============================================================================
create_bin_stub "gradatum-server" \
    "Stateless HTTP/MCP façade :19090 — handles read/search + enqueues writes" \
    "$(cat <<'README'
# gradatum-server

> Stateless HTTP/MCP facade on port 19090. Handles read/search requests and enqueues write operations.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Usage

```
gradatum-server [--config <path>]
```

## HTTP Endpoints

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/health` | None | Health check — returns `{"status":"ok","version":"..."}` |
| `GET` | `/metrics` | Loopback only | Prometheus metrics (port :19091) |
| `POST` | `/api/v1/vault_search` | Bearer | Full-text + semantic search |
| `POST` | `/api/v1/vault_read` | Bearer | Read note by path |
| `POST` | `/api/v1/vault_list` | Bearer | List notes with pagination |
| `GET` | `/api/v1/vault_status` | Bearer | Vault status and stats |
| `GET` | `/api/v1/vault_authors` | Bearer | List note authors |
| `GET` | `/api/v1/vault_tags` | Bearer | List tags with frequencies |
| `POST` | `/api/v1/vault_graph` | Bearer | Wikilink graph from a root note |
| `POST` | `/api/v1/vault_links` | Bearer | Wikilinks for a note |
| `POST` | `/api/v1/vault_trace` | Bearer | Trace chain through a note |
| `POST` | `/api/v1/vault_context` | Bearer | Context window for a note |

## MCP Endpoint

| Path | Description |
|---|---|
| `/mcp` | Streamable HTTP (MCP 2025-03-26) |
| `/sse` | SSE legacy transport |

## Configuration (TOML)

```toml
bind = "127.0.0.1:19090"     # C3: TLS required for non-loopback
data_root = "/var/lib/gradatum"
jwt_public_key_path = "/etc/gradatum/jwt_ed25519.pub"
```

## Auth

JWT Ed25519 bearer token. Audience-scoped (`read` / `write` / `admin`).
Generate via: `gradatum-admin init --preset hierarchical --root /var/lib/gradatum`

## Graceful shutdown

SIGTERM → 30-second drain.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation + Read API → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 19. gradatum-worker (BINARY)
# ============================================================================
create_bin_stub "gradatum-worker" \
    "Async queue consumer — curator LLM + maintenance jobs" \
    "$(cat <<'README'
# gradatum-worker

> Async queue consumer for curator LLM processing and maintenance jobs.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Usage (Phase 2.0b)

```
gradatum-worker [--config <path>]
```

## Job types processed

| Job type | Description |
|---|---|
| `curate_note` | LLM curator decision for a queued note |
| `drift_check` | Periodic drift detection (Phase A scan) |
| `index_rebuild` | Full index rebuild for a vault |
| `purge_expired_revocations` | Clean up expired JWT revocations |

## Configuration (TOML)

```toml
data_root = "/var/lib/gradatum"
queue_path = "/var/lib/gradatum/queue.db"
worker_concurrency = 4           # parallel job consumers
lease_timeout_ms = 300000        # 5 minutes per job
llm_endpoint = "http://127.0.0.1:8080"   # OpenAI-compat endpoint
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0 (Phase 2.0b implementation)
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 20. gradatum-admin (BINARY)
# ============================================================================
create_bin_stub "gradatum-admin" \
    "CLI ops — init/migrate/backup/restore + vault create/list/swap/delete" \
    "$(cat <<'README'
# gradatum-admin

> Operator CLI for Gradatum: bootstrap, migration, backup/restore, and vault lifecycle management.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Subcommands

### init

Bootstrap a Gradatum root directory (Phase 2.0a).

```
gradatum-admin init --preset hierarchical --root /var/lib/gradatum
gradatum-admin init --root /var/lib/gradatum --force   # re-init
```

Generates:
- `jwt_ed25519.key` / `jwt_ed25519.pub` (Ed25519 keypair, chmod 600/644)
- `admin_bearer.txt` (auto-generated admin token, chmod 600)
- `config.toml` (default server configuration)
- `queue.db` (SQLite queue)
- `acl/hierarchical.toml` (ACL preset)

### vault

```
gradatum-admin vault create <name>
gradatum-admin vault list
gradatum-admin vault swap <from> <to>
gradatum-admin vault delete <name> [--confirm]
```

### migrate

```
gradatum-admin migrate --from v0.x --to v0.1 --root /var/lib/gradatum
```

### backup / restore

```
gradatum-admin backup --root /var/lib/gradatum --output /backup/gradatum-$(date +%Y%m%d).tar.gz
gradatum-admin restore --input /backup/gradatum-20260504.tar.gz --root /var/lib/gradatum
```

## ACL Presets

| Preset | Description |
|---|---|
| `hierarchical` | Recommended — section-based RBAC with personal-classified guard |
| `open` | All authenticated consumers have read access (no write by default) |
| `strict` | Explicit whitelist per consumer per section |

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 21. gradatum-cli (BINARY)
# ============================================================================
create_bin_stub "gradatum-cli" \
    "End-user CLI — write/read/search via gradatum-server HTTP API" \
    "$(cat <<'README'
# gradatum-cli

> End-user CLI for reading, writing, and searching notes via the gradatum-server HTTP API.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Usage (Phase 2.0+)

```
gradatum [--server <url>] [--token <bearer>] <command>
```

## Subcommands

```
gradatum write <file.md>                     # Write a note from file
gradatum write --title "My note" --section decisions  # Write from stdin
gradatum read <note-path>                    # Read a note
gradatum search <query>                      # Search (BM25 + semantic fusion)
gradatum list [--section <name>] [--limit N] # List notes
gradatum status                              # Vault status
gradatum tags                                # List tags with frequency
gradatum authors                             # List authors
```

## Configuration

Environment variables:
```
GRADATUM_SERVER_URL=http://127.0.0.1:19090
GRADATUM_BEARER_TOKEN=<jwt>
```

Or config file at `~/.config/gradatum/config.toml`:
```toml
server_url = "http://127.0.0.1:19090"
bearer_token_file = "~/.config/gradatum/token"
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0 (Phase 2.0 implementation)
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0
README
)"

# ============================================================================
# 22. gradatum-mcp-stub (BINARY)
# ============================================================================
create_bin_stub "gradatum-mcp-stub" \
    "Adapter MCP stdio → HTTP gradatum-server (thin proxy)" \
    "$(cat <<'README'
# gradatum-mcp-stub

> Thin MCP stdio adapter: forwards MCP tool calls to gradatum-server HTTP API.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Usage

Configure in your MCP host (Claude Desktop, Claude Code, Continue.dev):

```json
{
  "mcpServers": {
    "gradatum": {
      "command": "gradatum-mcp-stub",
      "env": {
        "GRADATUM_SERVER_URL": "http://127.0.0.1:19090",
        "GRADATUM_BEARER_TOKEN": "<your-jwt-token>"
      }
    }
  }
}
```

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `GRADATUM_SERVER_URL` | `http://127.0.0.1:19090` | gradatum-server base URL |
| `GRADATUM_BEARER_TOKEN` | **(required)** | JWT bearer token |

## MCP Tools exposed (10 tools)

| Tool | Type | Description |
|---|---|---|
| `vault_search` | POST | Full-text + semantic search |
| `vault_read` | POST | Read note by path |
| `vault_list` | POST | List notes with filters |
| `vault_status` | GET | Vault status and stats |
| `vault_graph` | POST | Wikilink graph from root note |
| `vault_links` | POST | Wikilinks for a note |
| `vault_trace` | POST | Trace chain through notes |
| `vault_context` | POST | Context window for a note |
| `vault_authors` | GET | List note authors |
| `vault_tags` | GET | List tags with frequencies |

## Reconnect

Exponential backoff 100ms → 5s, max 10 attempts.
On 11th failure: `McpError::internal_error("server unavailable")`.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation + Read API
- License : Apache-2.0
README
)"

# ============================================================================
# Crates hors workspace (stubs futurs réservés)
# ============================================================================

# 23. gradatum-distill
create_lib_stub "gradatum-distill" \
    "k-anonymity distillation pipeline for PII-free knowledge exports (Phase v1.x+)" \
    "$(cat <<'README'
# gradatum-distill

> k-anonymity distillation pipeline for PII-free knowledge exports (reserved for Phase v1.x+).

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Planned scope (Phase v1.x+)

`gradatum-distill` will implement privacy-preserving knowledge export:

- k-anonymity distillation of note corpora
- PII detection and removal pipeline
- Differential privacy mechanisms for knowledge graphs
- Export format: structured JSON/JSONL with privacy budget metadata

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase v1.x+ (post-public-release)
- License : Apache-2.0
README
)"

# 24. gradatum-mcp
create_lib_stub "gradatum-mcp" \
    "Full MCP server implementation for gradatum (Phase 2.x+, vs current gradatum-mcp-stub proxy)" \
    "$(cat <<'README'
# gradatum-mcp

> Full MCP server implementation for gradatum (Phase 2.x+). See [`gradatum-mcp-stub`](https://crates.io/crates/gradatum-mcp-stub) for the current stdio proxy.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Planned scope (Phase 2.x+)

`gradatum-mcp` will implement a full MCP server (not a proxy):

- Native MCP Streamable HTTP transport (MCP 2025-03-26 spec)
- Direct in-process connection to storage/index layers
- Extended tool set beyond the 10 tools in `gradatum-mcp-stub`
- Sampling and resource endpoints
- MCP authorization integration

## Current alternative

Use [`gradatum-mcp-stub`](https://crates.io/crates/gradatum-mcp-stub) — stdio proxy that forwards to `gradatum-server` HTTP API.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.x+ (post-Phase 2.1)
- License : Apache-2.0
README
)"

# 25. gradatum-protocol
create_lib_stub "gradatum-protocol" \
    "Wire protocol types and serialization for gradatum inter-component communication (reserved)" \
    "$(cat <<'README'
# gradatum-protocol

> Wire protocol types and serialization for gradatum inter-component communication (reserved name).

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Planned scope

`gradatum-protocol` is reserved for shared wire protocol types:

- HTTP API request/response schemas (Serde types shared between server and SDK)
- MCP message types
- Stable serialization formats for cross-version compatibility

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : TBD (reserved name)
- License : Apache-2.0
README
)"

# 26. gradatum-studio
create_bin_stub "gradatum-studio" \
    "Admin and visualization web UI for gradatum (Phase 2.x+)" \
    "$(cat <<'README'
# gradatum-studio

> Admin and visualization web UI for gradatum vault management (reserved for Phase 2.x+).

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Planned scope (Phase 2.x+)

`gradatum-studio` will provide a web-based admin UI:

- Vault management (create/list/swap/delete)
- Note browser with full-text and semantic search
- Wikilink graph visualization (D3.js)
- Curator activity dashboard
- User/token management (admin scope)
- Metrics and health monitoring

## Current alternative

Use `gradatum-admin` CLI for vault operations and `gradatum-cli` for note management.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.x+ (post-Phase 2.1)
- License : Apache-2.0
README
)"

echo
echo "==> Génération terminée."
echo "    26 stubs dans : ${STUBS_DIR}/"
echo
echo "Vérification rapide :"
ls "${STUBS_DIR}/" | wc -l
echo "  crates générés"
echo
echo "Prochaine étape : bash scripts/publish-stubs-v0.0.1.sh"
