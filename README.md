# Gradatum

> **Memory backbone for AI agents — graduated.**

![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)
![Status: Alpha](https://img.shields.io/badge/Status-Alpha-orange.svg)

A self-hosted, embedded memory backbone for multi-agent AI systems. Rust + SQLite. Zero external services required.

> Gradatum stores knowledge in **loci** — a name borrowed from Cicero's *Ars Memoriae*, the ancient mnemonic method where memories are placed in mental locations of an imagined palace. Agents don't share rooms — they share *places of memory*.

---

## Why Gradatum

**The problem.** AI agents need persistent memory that's structured, searchable, and shared across sessions. Existing solutions either lock you into a SaaS, require heavy stacks (Postgres + pgvector + vector DBs), or aren't designed for agents at all.

**The Gradatum approach.**

| Property | Why it matters |
|---|---|
| **Embedded** | One Rust binary. No PostgreSQL. No Redis. No external services. |
| **Self-hosted** | Your memory, your machine. No telemetry. No vendor lock-in. |
| **LLM-agnostic** | Plug any OpenAI-compatible backend (Ollama, vLLM, llama.cpp, OpenRouter, Anthropic) — or run heuristic-only with no LLM at all. |
| **Multi-vault** | Separate `main` from `staging` and `bench-*` vaults for testing, migration, A/B prompts. Atomic swap when ready. |
| **Hierarchical ACL** | Bearer-scoped access to memory loci. Configure from presets (`flat`, `hierarchical`, `multi-project`, `team`) or write your own. |
| **Markdown truth** | Notes are Markdown files with YAML frontmatter. Readable by humans, by any text editor, by `cat`. The database is an index, not the source of truth. |
| **Hybrid search** | BM25 (SQLite FTS5) + semantic similarity + PageRank graph + optional cross-encoder rerank. Multi-signal fusion via RRF (Reciprocal Rank Fusion). |

---

## Status

- **Version** : `v0.3.5` — public OSS alpha (Apache-2.0). Source available. APIs are **not stable** before v1.0.0.
- **Grade** : **Bronze** — `v0.1.0` / `v0.2.0` / `v0.3.0` shipped, plus patch releases `v0.3.1`–`v0.3.5`. Alpha, under construction.
- **Workspace** : 28 crates, MSRV (minimum supported Rust version) `1.85`, `ort` feature-gated (`onnx-reranker`).
- **Test suite** : ~1223 tests PASS / 0 clippy warnings / 0 fmt diff / `cargo deny` GREEN.
- **Search** : RRF k=60 stable-sort fusion of BM25 (SQLite FTS5) + semantic similarity (brute-force cosine over bge-m3 1024d embeddings) + composite multi-factor scoring (recency × log-decay + PageRank backlinks) + an optional cross-encoder reranker (ONNX, feature `onnx-reranker`).
- **Job queue** : Apalis (SQLite-backed) with dead-letter queue + monitor.
- **Storage** : OpenDAL multi-backend (Local FS, S3/R2, Azure, GCS, WebDAV, SFTP, HDFS, IPFS).
- **MCP** : `gradatum-mcp-stub` (rmcp, stdio transport). Native MCP server + Streamable HTTP transport are planned (see roadmap).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the current technical design, and [CHANGELOG.md](CHANGELOG.md) for the full version history.

---

## What's shipped (Bronze: v0.1.0 → v0.3.5)

A condensed view — the authoritative log lives in [CHANGELOG.md](CHANGELOG.md).

| Milestone | Highlights |
|---|---|
| **v0.1.0** | Functional core + HTTP/MCP service: vault registry, curator gating, hybrid search (RRF), bearer auth + hierarchical ACL, queue worker, embed pipeline. |
| **v0.2.0** | Search hardening, dedup, dependency/supply-chain bumps (axum 0.8, rmcp, serde_yml), MSRV → 1.85. |
| **v0.3.0** | Trait carve-out (`DocumentStore` / `IndexStore` / `VectorStore`), `Arc<dyn Index>` indirection, server-side event log, curator metadata kinds, secrets via DI + persisted JWT signing key, curate→embed cascade. Workspace 26 → 28 crates. |
| **v0.3.1 – v0.3.3** | Worker concurrency fix: SQLite read-then-write deadlock under parallel dequeue resolved via `BEGIN IMMEDIATE` (write lock upfront). Job routing by kind + backfill migration. |
| **v0.3.4** | Write-path fix: `notes.title` column now populated at curate time + migration backfill — `vault_search` title:null eliminated for newly written notes. |
| **v0.3.5** | Search read-path fix: semantic-only hits now enriched (title + snippet) in final batch pass — `title:null` count=0 on semantic hits. |

---

## Roadmap

Gradatum follows a **vault-first** trajectory — make the memory store durable and queryable before layering serving and multimodal features on top.

| Version | Grade | Theme |
|---|---|---|
| **v0.3.x** | Bronze | Current — service foundation, hybrid search, trait carve-out, worker stability. |
| **v0.4.x** | — | **Vault Core** — durable writes, temporal/lifecycle management, distillation, storage backends. |
| **v0.5.0** | **Silver** | Vault interrogeable via MCP — queryable memory surface for agents. |
| **v0.5.1** | — | Multi-tenant + OAuth. |
| **v0.6.0** | — | Context + agent layer. |
| **v0.7.0** | — | Serving. |
| **v1.0.0** | **Gold** | Production baseline. |
| **v2.0.0** | **Platinum** | Multimodal + bring-your-own-compute (BYOC). |

Near-term planned items: `sqlite-vec` ANN (approximate nearest-neighbour) index (replacing brute-force cosine), native MCP server + Streamable HTTP transport.

---

## Documentation

### Project overview and design

| Document | Purpose |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Technical design — planes, ACL hierarchy, source-of-truth model, search pipeline, concurrency, storage layout. |
| [DEPENDENCIES.md](DEPENDENCIES.md) | Workspace dependency tree, level invariants, version pinning policy. |
| [PORTS.md](PORTS.md) | Default port matrix (`19090 + offset`) and override conventions (CLI > env > TOML). |
| [docs/BENCH.md](docs/BENCH.md) | Benchmark results (curator F1w, search relevance). |
| [CHANGELOG.md](CHANGELOG.md) | Version history and notable changes per release. |

### Governance and process

| Document | Purpose |
|---|---|
| [GOVERNANCE.md](GOVERNANCE.md) | Decision-making, RFC process, maintainer roles. |
| [RELEASE-POLICY.md](RELEASE-POLICY.md) | Versioning policy, anti-fragility gates, public-release criteria. |
| [MAINTAINERS.md](MAINTAINERS.md) | Current maintainers. |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Contributor guide, PR process. |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | Contributor Covenant 2.1. |
| [CLA.md](CLA.md) | Contributor License Agreement. |
| [SECURITY.md](SECURITY.md) | Vulnerability disclosure process and supported versions. |
| [AGENTS.md](AGENTS.md) | Guidance for AI assistants working on this repository. |

### RFCs (Requests for Comments)

| Document | Purpose |
|---|---|
| [docs/RFC/README.md](docs/RFC/README.md) | RFC index and process. |
| [RFC-TEMPLATE.md](RFC-TEMPLATE.md) | Skeleton template for new RFCs. |
| [RFC-0001](docs/RFC/RFC-0001-versioning-gradatum-core.md) | Trait stability tiers and versioning for `gradatum-core`. |
| [RFC-0002](docs/RFC/RFC-0002-cross-platform-support.md) | Cross-platform support — superseded; gradatum is Linux-only (deferred 2026-06-05). |
| [RFC-0003](docs/RFC/RFC-0003-http-api-surface-and-mcp-integration.md) | HTTP API surface and MCP integration topology. |

### Deployment

| Document | Purpose |
|---|---|
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Engine multi-instance deployment guide — systemd, config, security, gateway wiring. |

> **Security note**: the default ACL is a permissive single-tenant policy (allow-all) in v0.x. Configure a stricter AclPolicy before exposing gradatum on a network.

### Platform-specific guides (archived)

| Document | Purpose |
|---|---|
| [docs/WINDOWS-GUIDE.md](docs/WINDOWS-GUIDE.md) | Windows guide — **deferred** (archived, Linux-only as of 2026-06-05). |
| [docs/KNOWN_ISSUES-WINDOWS.md](docs/KNOWN_ISSUES-WINDOWS.md) | Windows known issues — **deferred** (archived, Linux-only as of 2026-06-05). |

---

## Installation

Gradatum is in **alpha (v0.3.x)**. No pre-built binaries are distributed yet — build from source.

**Prerequisites:** Rust stable (MSRV 1.85), a C linker (`gcc` / `clang`), and SQLite 3.x development headers (e.g. `libsqlite3-dev` on Debian/Ubuntu).

```bash
# Clone and build all workspace crates
git clone https://github.com/gradatum/gradatum.git
cd gradatum
cargo build --workspace --release

# Optionally run the test suite
cargo test --workspace
```

The release binaries (`gradatum-server`, `gradatum-worker`, `gradatum-admin`) are written to `target/release/`. See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for configuration and runtime requirements.

> APIs are not stable before v1.0.0. Breaking changes will be documented in [CHANGELOG.md](CHANGELOG.md).

---

## Concepts

### Manual usage (planned CLI surface)

```bash
# Write a note
gradatum write --locus=projecta/backend --section=decisions \
  "Use ULID for stable note identity" \
  "Why: titles change, ULID doesn't. See ARCHITECTURE.md."

# Search
gradatum search "ULID identity" --locus="projecta/*"

# List vaults
gradatum-admin vault list
```

### MCP integration (Claude Code / Claude Desktop)

Gradatum exposes its memory surface over MCP via `gradatum-mcp-stub` (stdio transport). A sample MCP server fragment:

```json
{
  "mcpServers": {
    "gradatum": {
      "command": "gradatum-mcp-stub",
      "args": [],
      "env": {
        "GRADATUM_SERVER_URL": "http://127.0.0.1:19090"
      }
    }
  }
}
```

The stub handles the auth flow automatically (api-key → token exchange → JWT with TTL auto-refresh).

---

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design.

In one diagram:

```
        AI agents / coding assistants / orchestrators
              ↓ MCP / HTTP / CLI
        ┌──────────────────────────┐
        │  gradatum-server         │  stateless façade
        └────────┬─────────────────┘
                 ↓ async queue (Apalis)
        ┌──────────────────────────┐
        │  gradatum-worker         │  curator + maintenance jobs
        └────────┬─────────────────┘
                 ↓
        ┌──────────────────────────┐
        │  vault (one)             │
        │  ├─ vault_id="main"      │
        │  └─ vault_id="staging"   │
        │     ├─ locus paths       │  hierarchical ACL via bearer
        │     ├─ sections          │  decisions / debug / etc.
        │     └─ notes (MD+meta)   │
        └──────────────────────────┘
```

---

## Vocabulary

| Term | Meaning |
|---|---|
| **Vault** | The technical backing store (SQLite + FTS5 + Markdown). Multi-vault first-class — main + staging + bench-* |
| **Locus** | A logical subdivision of a vault, isolated by ACL. From Cicero's *ars memoriae*. |
| **Section** | One of the cognitive categories: `decisions`, `architecture`, `debug`, `reasoning`, `feedback`, `lessons-learned`, `retrospectives`, `experiments`, `agent-issues`, `reference` |
| **Note** | Atomic Markdown file with YAML frontmatter |
| **Bearer / Consumer** | An authenticated identity with read/write ACL patterns |
| **Preset** | A template configuration shipped in `examples/presets/` |

---

## Contributing

This is alpha, built openly on lessons learned from a prior private system. Contributor guidelines, issue tracking, and PR process will open with the first public OSS release. See [CONTRIBUTING.md](CONTRIBUTING.md) and [GOVERNANCE.md](GOVERNANCE.md) for the intended process.

---

## License

[Apache-2.0](LICENSE)
