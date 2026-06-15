# Gradatum

> **Memory backbone for AI agents — graduated.**

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Status: Alpha](https://img.shields.io/badge/Status-Alpha-orange.svg)](CHANGELOG.md)
[![Website](https://img.shields.io/badge/Website-gradatum.org-brightgreen)](https://gradatum.org)

A self-hosted, embedded memory backbone for multi-agent AI systems. Rust + SQLite. Zero external services required.

Website: **[gradatum.org](https://gradatum.org)**

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

- **Version** : `v0.5.2` — public OSS alpha (Apache-2.0). Source available. APIs are **not stable** before v1.0.0.
- **Grade** : **Bronze** — `v0.1.0`, `v0.2.0`, `v0.3.x` (v0.3.5–v0.3.7), `v0.4.0`, `v0.4.1`, `v0.4.2`, `v0.4.3`, and `v0.5.2` shipped. Alpha, under construction.
- **Workspace** : 31 crates (26 publishable), MSRV (minimum supported Rust version) `1.88`, `ort` feature-gated (`onnx-reranker`).
- **Test suite** : 1925 tests PASS / 0 clippy warnings / 0 fmt diff / `cargo deny` GREEN.
- **Search** : RRF k=60 stable-sort fusion of BM25 (SQLite FTS5) + semantic similarity (brute-force cosine over bge-m3 1024d embeddings) + composite multi-factor scoring (recency decay + PageRank) + optional cross-encoder reranker (ONNX, feature `onnx-reranker`).
- **Job queue** : Apalis (SQLite-backed) with dead-letter queue + monitor.
- **Storage** : OpenDAL multi-backend (Local FS default; S3/R2, GCS, Azure via feature flags).
- **MCP** : `gradatum-mcp-stub` (rmcp, stdio transport). Native MCP server + Streamable HTTP transport are planned (see roadmap).

> **LIVE on `:19090`.** The current public release is **v0.5.2** — deployed LIVE. See [CHANGELOG.md](CHANGELOG.md) for the full version history.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the current technical design, and [CHANGELOG.md](CHANGELOG.md) for the full version history.

---

## What's shipped (Bronze: v0.1.0 → v0.5.2)

A condensed view — the authoritative log lives in [CHANGELOG.md](CHANGELOG.md).

| Milestone | Highlights |
|---|---|
| **v0.1.0** | Functional core + HTTP/MCP service: vault registry, curator gating, hybrid search (RRF), bearer auth + hierarchical ACL, queue worker, embed pipeline. |
| **v0.2.0** | Search hardening, dedup, dependency upgrades (axum 0.8, rmcp, serde_yml), MSRV → 1.85. |
| **v0.3.0** | Storage trait decomposition (`DocumentStore` / `IndexStore` / `VectorStore`), event log, curator metadata, secrets DI + persisted JWT key. 26 → 28 crates. |
| **v0.3.1 – v0.3.5** | Worker concurrency fixes (deadlock resolution via `BEGIN IMMEDIATE`). Write-path title backfill. Search read-path enrichment. |
| **v0.3.6** | Public OSS release: GitHub, crates.io (26 crates), Apache-2.0 license. |
| **v0.3.7** | Reliability fixes: wikilink ULID consistency, vault_read round-trip, score documentation. 1178 tests PASS. |
| **v0.4.0** | **Vault durable writes**: note history, optimistic locking, stable wikilinks, write provenance. Migration 0010. |
| **v0.4.1** | Quality & reliability: zero-panic public API, revocation wired, docs.rs coverage, MSRV → 1.88. |
| **v0.4.2** | Note ID in write response, DTO unification, gateway metrics cardinality. |
| **v0.4.3** | **Vault lifecycle**: semantic forget (dry-run + decay), 6-state lifecycle machine, history retention, search scoping (`locus` / `vault_id`), multimodal gateway. 1407 tests. |
| **v0.5.2** | **Code index + timeline + tracing**: `code ingest`/`code update` (tree-sitter, zero LLM), `POST /api/v1/code_scope` + MCP tool, `vault_timeline`, `session-log/trace`, `include_corpus_count`, `vault_write` in-place, native TLS (`[server.tls]`), Studio JWT persistence, defense-in-depth cross-tenant (6-layer fix), optimistic-lock `Conflict` surfaced. 1925 tests PASS. |

---

## What's new in v0.5.2

All features below shipped in v0.5.2 (2026-06-15) and are deployed LIVE on `:19090`. See [CHANGELOG.md](CHANGELOG.md) `[0.5.2]` and [ARCHITECTURE.md](ARCHITECTURE.md) for the full technical detail.

**Code intelligence (F-61)**

- **code-map / code-ingest** : derived code index via tree-sitter (Rust), stored in a logical vault `code-<project>` separate from the memory vault `main`. Zero LLM cost — static analysis only. Endpoint `POST /api/v1/code_scope` + MCP tool `code_scope`. CLI: `gradatum-admin code ingest` (full) + `code update` (O(diff) git-driven, golden `rebuild == incremental` verified). Migrations 0016–0018. Drift-detection per entry (flag `stale` — stored hash vs current file hash at read time).
- **Visibility control** : `--visibility pub|all` on `code ingest` / `code update` (default `pub`, unchanged behaviour). Stored per vault; `code update` reuses the stored mode.
- **`include_body`** : exact source span retrieval per symbol on demand (`include_body: bool`, default `false`; `body_budget_tokens: usize`, default 4000). Anti-traversal guard S1 unconditional (`canonicalize + starts_with(repo_root)`). Token efficiency: −4.6 % vs direct file reads at exhaustive scope, better coverage.

**Memory and temporal**

- **Session-log Tier 1** : `POST /api/v1/session-log/trace` — append-only, 90-day retention, PII-safe (`agent_id` = JWT sub only). Migration 0015 `session_trace`.
- **`vault_timeline` (F-55 zone D)** : `POST /api/v1/vault_timeline` — paginated chronological listing of notes. MCP tool `vault_timeline`. `PROTECTED_FORGET` sections excluded (0/49 leaks confirmed). Validity windowing: `valid_until` frontmatter → `temporal_index`; `as_of_ms` + `include_expired` filter on the endpoint.
- **`include_corpus_count`** : proof-of-absence signal in `vault_search` (opt-in `include_corpus_count: bool`). BM25/FTS5-only count of full-corpus matches, distinct from the returned result set (capped at 10 001).
- **Distillation (F-22)** : semantic distillation job — clusters notes by embedding similarity, writes synthesis notes in `pending-review` with `provenance: "distilled"` and `derived-from` links. Trust-decay scoring (F-17): per-provenance half-lives, `trust_decay_enabled` flag (default on). Lesson recall (F-60): `GET /api/v1/lessons/recall?class=<x>` — BM25-only over `lessons-learned`, 12-class vocabulary, MCP tool `vault_lessons_recall`.
- **Backend-agnostic index parity** (v0.4.5): worker type-erased on `Arc<dyn Index>`; new `index-parity-tests` crate with 24 tests across 7 invariant families.

**Security and tenant hardening**

- **Defense-in-depth cross-tenant (Slice 2 Phase 2a)** : latent P0 where `JWT claims.tenant_id` and `req.tenant_id` (serde-default `main`) were never reconciled — closed across 6 layers: `/auth/exchange` gate, central middleware exhaustive match, all handlers via `tenant_guard::effective_tenant`, cross-read `vault_id` clamp, worker reject, API key creation guard. Smoke 6/6 (`403` on cross-tenant paths, `200` on `main`).
- **`vault_write` in-place update** : `vault_write` now honours `note_id` + `expected_sha256` — present `note_id` = update in-place; absent `expected_sha256` on existing note = `409`; malformed sha = `400` (anti-fail-open, guard ordering verified).
- **Native TLS termination** (`[server.tls]`): rustls 0.23-backed; fail-closed on bad cert; non-loopback without TLS refused at startup.

**Studio and jobs**

- **Studio MVP (F-37)** : 5 surfaces (`/ui/*`) — Dashboard, Notes + detail, Search, Review queue, Jobs — served by `ServeDir` without auth (LAN). Auth flow: api-key → `POST /auth/exchange` → JWT persisted in `localStorage` (24h TTL, no api-key persisted). Strict CSP, `nosniff`, `Referrer-Policy`, self-hosted fonts.
- **Job endpoints hardened** (`/api/v1/jobs`): bearer JWT + ACL required; real `JobKind` deserialization.

**Status** : deployed **LIVE** on `:19090`. **1925 tests PASS**, clippy 0, fmt clean, `cargo deny` GREEN.

See [CHANGELOG.md](CHANGELOG.md) `[0.5.2]` + [ARCHITECTURE.md](ARCHITECTURE.md) for full technical detail.

---

## Roadmap

Gradatum follows a **vault-first** trajectory — make the memory store durable and queryable before layering serving and multimodal features on top.

| Version | Grade | Status | Theme |
|---|---|---|---|
| **v0.1.0 – v0.4.3** | Bronze | ✅ **public** | Functional core → vault lifecycle (forget, state machine, history, search scoping, multimodal gateway). |
| **v0.5.2** | Bronze | ✅ **public (current)** | Code index (tree-sitter, zero LLM), vault_timeline, session-log/trace, include_corpus_count, vault_write in-place, native TLS, Studio MVP, cross-tenant hardening. Deployed LIVE. |
| **v0.6.x** | — | ⬜ planned | Multi-tenant real (VaultRegistry — split index per tenant) + OIDC (pluggable identity provider) + distributed tracing, MCP Streamable HTTP. |
| **v0.7.0** | — | ⬜ planned | Serving — web UI / dashboards, native MCP server. |
| **v1.0.0** | **Gold** | ⬜ planned | Stable APIs, LTS, full audit trail, HA (Litestream), OpenTelemetry. |
| **v2.0.0** | **Platinum** | ⬜ planned | Multimodal + bring-your-own-compute (BYOC), multi-region. |

> **We are here**: **v0.5.2** — current public release (2026-06-15). Next planned: **v0.6.x**.

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

Gradatum is in **alpha (v0.5.x)**. Three installation paths are available depending on your use case.

> APIs are not stable before v1.0.0. Breaking changes will be documented in [CHANGELOG.md](CHANGELOG.md).

### Option A — Pre-built binaries (recommended for deployment)

**Platform: Linux x86_64.** Each [GitHub Release](https://github.com/gradatum/gradatum/releases) ships three archives:

| Archive | Binaries | Use case |
|---|---|---|
| `gradatum-server-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-server`, `gradatum-worker`, `gradatum-admin` | Vault backbone — run on your application host |
| `gradatum-llm-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-gateway`, `gradatum-engine` | LLM routing and inference supervision — run on your GPU host (or alongside the backbone) |
| `gradatum-mcp-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-mcp-stub` | MCP bridge for Claude Code / Claude Desktop |

A `SHA256SUMS` file covering all three archives is also attached to each release. Every archive ships with a **SLSA provenance attestation**.

**Example — deploying the vault backbone:**

```bash
# Replace vX.Y.Z with the release tag, e.g. v0.5.2
VERSION=v0.5.2
ARCH=x86_64-unknown-linux-gnu

# Download the server archive and the checksum file
curl -fLO "https://github.com/gradatum/gradatum/releases/download/${VERSION}/gradatum-server-${VERSION}-${ARCH}.tar.gz"
curl -fLO "https://github.com/gradatum/gradatum/releases/download/${VERSION}/SHA256SUMS"

# Verify the checksum
sha256sum -c SHA256SUMS --ignore-missing

# (Optional) Verify the SLSA provenance attestation
gh attestation verify "gradatum-server-${VERSION}-${ARCH}.tar.gz" \
  --repo gradatum/gradatum

# Extract and install
tar -xzf "gradatum-server-${VERSION}-${ARCH}.tar.gz"
sudo install -m 755 "gradatum-server-${VERSION}-${ARCH}/gradatum-server" /usr/local/bin/
sudo install -m 755 "gradatum-server-${VERSION}-${ARCH}/gradatum-worker"  /usr/local/bin/
sudo install -m 755 "gradatum-server-${VERSION}-${ARCH}/gradatum-admin"   /usr/local/bin/
```

Download the `gradatum-llm` archive on the host running inference engines, and `gradatum-mcp` wherever you use the MCP bridge. The same checksum and attestation workflow applies to each.

See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for configuration and runtime requirements (systemd units, TOML fields, security properties, gateway wiring).

### Option B — crates.io (name reservation — not installable before v1.0)

> **Note**: crates.io carries name-reservation placeholder stubs (v0.0.x) for the gradatum crate family. The source code is open on GitHub (Apache-2.0) but the library is **not published as an installable crate before v1.0**. `cargo add gradatum` or `gradatum = "0.x"` in `Cargo.toml` will not give you a working library.
>
> Use **Option A** (pre-built binaries) or **Option C** (build from source) instead.

The v0.3.6 changelog entry mentioning crates.io (26 crates) referred to these same name-reservation stubs — not installable library releases.

### Option C — Build from source

**Prerequisites:** Rust stable (MSRV 1.88), a C linker (`gcc` / `clang`), and SQLite 3.x development headers (e.g. `libsqlite3-dev` on Debian/Ubuntu).

```bash
git clone https://github.com/gradatum/gradatum.git
cd gradatum

# Build all workspace crates
cargo build --workspace --release

# Optionally run the test suite
cargo test --workspace
```

The release binaries are written to `target/release/`. Note: `gradatum-engine` requires the `serve` feature when building individually (`cargo build -p gradatum-engine --features serve --release`); building with `--workspace` enables it automatically.

arm64, macOS, and Windows are not supported by the pre-built binaries — build from source on those platforms.

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

Gradatum is structured in two layers:

### Memory layer

The core vault stack — notes are Markdown files, the database is an index.

| Component | Crate(s) | Role |
|---|---|---|
| **gradatum-server** | `gradatum-server` | Stateless HTTP façade (REST + MCP) |
| **gradatum-worker** | `gradatum-worker` | Async job worker (curator + maintenance) |
| **Curator** | `gradatum-curator` | LLM-assisted classification and metadata tagging |
| **Embedder** | `gradatum-embed` | Dense vector embeddings (bge-m3 1024d or configurable) |
| **Hybrid search** | `gradatum-search`, `gradatum-index` | BM25 (FTS5) + semantic cosine + PageRank + optional ONNX reranker, fused via RRF |
| **Vault + storage** | `gradatum-vault`, `gradatum-storage` | Markdown source of truth + OpenDAL multi-backend |
| **Lifecycle** | `gradatum-warden` | Downgrade, expiry, distillation policies |
| **ACL / auth** | `gradatum-auth`, `gradatum-acl-auth`, `gradatum-acl-policy` | Bearer-scoped access, JWT signing, hierarchical policies |
| **Queue** | `gradatum-queue` | Apalis SQLite-backed job queue, DLQ, per-kind routing |
| **MCP stub** | `gradatum-mcp-stub` | stdio MCP server for Claude Code / Claude Desktop |

### Agent layer

Local inference infrastructure — optional, deploy alongside the memory layer when you want fully offline LLM-backed features.

| Component | Crate(s) | Role |
|---|---|---|
| **Engine** | `gradatum-engine` | Process supervisor for `llama-server` children. One instance per model. Transparent reverse-proxy (OpenAI-compatible), restart-bounded, Prometheus `/metrics` on loopback. |
| **Gateway** | `gradatum-gateway` | Unified LLM router. Maps logical aliases (`curator`, `embed`, …) to providers, circuit-breaker, primary + fallback routing. Covers both chat and embeddings. |
| **Event log** | `gradatum-server` (B1) | Structured event log for inference calls (table `event_log`, retention-aware). |

### In one diagram

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
                 ↕ LLM calls via gradatum-gateway
        ┌──────────────────────────┐
        │  gradatum-gateway        │  alias routing + circuit-breaker
        └────────┬─────────────────┘
                 ↓ (local or remote)
        ┌──────────────────────────┐
        │  gradatum-engine         │  supervisor per model
        │  └── llama-server child  │  loopback only, GGUF
        └──────────────────────────┘
```

### Example multi-host setup

A typical self-hosted layout: one application host runs the gradatum stack and gateway,
one GPU host runs the inference engines (one `gradatum-engine` instance per model).
All addresses below are documentation examples (RFC 5737 — not routable).

```
                 consumers (apps, agents, MCP clients)
                                  │
                                  ▼
 ┌──────────── app-host (Linux, 203.0.113.10) ────────────────────────────────────┐
 │                                                                                 │
 │  gradatum-server ──┐                       ┌─────────────────────────────┐      │
 │  gradatum-worker ──┴─────────────────────▶ │ gradatum-gateway  :8436     │      │
 │  gradatum-mcp-stub                         │ (router + circuit-breaker)  │      │
 │                                            └───┬─────────────────┬───────┘      │
 │                                       primary  │                 │ fallback     │
 │                                                │                 ▼              │
 │                                                │     ┌─────────────────────┐    │
 │                                                │     │ local CPU fallback  │    │
 │                                                │     │ engine (chat+embed) │    │
 │                                                │     └─────────────────────┘    │
 └───────────────────────────────────────────────┼────────────────────────────────┘
                                                  │ LAN
 ┌──────────── gpu-host (Linux, 203.0.113.20) ────────────────────────────────────┐
 │  gradatum-engine — one instance per model:                                     │
 │                                                                                │
 │   ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌──────────┐                            │
 │   │ curator │ │ embed   │ │ reason  │ │ vision   │                            │
 │   │ :N      │ │ :N+2    │ │ :N+4    │ │ :N+6 +mm │                            │
 │   └────┬────┘ └────┬────┘ └────┬────┘ └────┬─────┘                            │
 │        │ each supervises one llama-server child (loopback child_port)          │
 │        └── bind: gpu-host LAN IP · /metrics always on loopback ────────────────┘
 │   GGUF models read-only under /opt/gradatum/models/                             │
 └────────────────────────────────────────────────────────────────────────────────┘
```

The engine layer is optional. With no GPU host, the gateway falls back to the local CPU
engine instances on `app-host`. See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for the
full configuration reference (systemd units, TOML fields, allow-listed flags, security
properties).

---

## Vocabulary

| Term | Meaning |
|---|---|
| **Vault** | The technical backing store (SQLite + FTS5 + Markdown). Multi-vault first-class — main + staging + bench-* |
| **Locus** | A logical subdivision of a vault, isolated by ACL. From Cicero's *ars memoriae*. |
| **Section** | One of the cognitive categories: `decisions`, `architecture`, `debug`, `reasoning`, `feedback`, `lessons-learned`, `retrospectives`, `experiments`, `agent-issues`, `reference`, `council` |
| **Note** | Atomic Markdown file with YAML frontmatter |
| **Bearer / Consumer** | An authenticated identity with read/write ACL patterns |
| **Preset** | A template configuration shipped in `crates/gradatum-admin/presets/` |

---

## Contributing

This is alpha, built openly on lessons learned from a prior private system. Contributor guidelines, issue tracking, and PR process will open with the first public OSS release. See [CONTRIBUTING.md](CONTRIBUTING.md) and [GOVERNANCE.md](GOVERNANCE.md) for the intended process.

---

## License

[Apache-2.0](LICENSE)
