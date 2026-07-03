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
| **Multi-vault** | Separate `main` from `staging` and `bench-*` vaults for testing, migration, A/B prompts (read-side today; full multi-vault management planned v1.0). |
| **Hierarchical ACL** | Bearer-scoped access to memory loci. Configure from presets (`flat`, `hierarchical`, `multi-project`, `team`) or write your own. |
| **Markdown truth** | Notes are Markdown files with YAML frontmatter. Readable by humans, by any text editor, by `cat`. The database is an index, not the source of truth. |
| **Hybrid search** | BM25 (SQLite FTS5) + semantic similarity + PageRank graph + optional cross-encoder rerank. Multi-signal fusion via RRF (Reciprocal Rank Fusion). |


---

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design.

### OSS stack

Gradatum is built on these open-source foundations:

| Library | What it does in gradatum |
|---|---|
| [**Tokio**](https://github.com/tokio-rs/tokio) | Async runtime — all I/O, timers, and task scheduling |
| [**Axum**](https://github.com/tokio-rs/axum) | HTTP server (REST endpoints + studio `ServeDir`) |
| [**Tower**](https://github.com/tower-rs/tower) / **tower-http** | Middleware stack — rate-limiting, CORS, auth, body limits |
| [**rmcp**](https://github.com/modelcontextprotocol/rust-sdk) | Native MCP server over Streamable HTTP (23 tools at `/mcp`) |
| [**SQLx**](https://github.com/launchbadge/sqlx) | Async SQLite driver — vault index, job queue, sessions |
| [**Apalis**](https://github.com/geofmureithi/apalis) | Background job queue (SQLite-backed, DLQ, per-kind routing, monitoring) |
| [**OpenDAL**](https://github.com/apache/opendal) | Storage abstraction — local FS today, S3/GCS/Azure via feature flags |
| [**tree-sitter**](https://github.com/tree-sitter/tree-sitter) | Deterministic code parsing for the code index (Rust, Python, Bash, TS, TSX) — zero LLM |
| [**rustls**](https://github.com/rustls/rustls) + **axum-server** | Native TLS termination (TLS 1.2+/1.3, fail-closed) |
| [**Moka**](https://github.com/moka-rs/moka) | In-process LRU cache (EffectiveNote, TTL-based invalidation) |
| [**serde**](https://github.com/serde-rs/serde) | Serialization layer for all wire formats (JSON, YAML, TOML, bincode) |
| [**argon2**](https://github.com/RustCrypto/password-hashes) | API key hashing (Argon2id) |
| [**ed25519-dalek**](https://github.com/dalek-cryptography/curve25519-dalek) | JWT signing (Ed25519) |
| [**ONNX Runtime**](https://github.com/microsoft/onnxruntime) (`ort`) | Optional neural reranker (feature `onnx-reranker`) |
| [**Figment**](https://github.com/SergioBenitez/Figment) | Layered config (TOML + env + CLI) |
| [**Prometheus**](https://github.com/prometheus/client_rust) | Metrics export (job counts, latencies, embedder stats) |
| [**Clap**](https://github.com/clap-rs/clap) | CLI for `gradatum-admin` |

---

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

## Roadmap

**Current public release**: `v0.7.6` · Apache-2.0 · 3038 tests PASS · Rust 1.88+

Gradatum is built in three chapters: **memory first, then agents, then the sovereign terminal**.

| Version | Status | What it brings |
|---|---|---|
| **v0.1.0 – v0.4.3** | ✅ shipped | Working knowledge store: write, search, trust-scored sources, version history, stable links, lifecycle (forget, compact, distil). |
| **v0.5.2** | ✅ shipped | Code awareness: index any codebase from source, search by symbol or file. Encrypted connections, chronological browsing, agent tracing. |
| **v0.5.5** | ✅ shipped | Health endpoint with version proof, Rust 2024 edition, hardened API surface. |
| **v0.6.0** | ✅ shipped | Native MCP server — any MCP client connects directly over HTTP. |
| **v0.6.4** | ✅ shipped | Security baseline, 5-language code-map, 12 correctness fixes. 2337 tests PASS. |
| **v0.7.6** | ✅ **current public** | Memory intelligence layer: assembled context pipeline (BM25 + semantic + RRF + composite scoring), proactive recall (server-initiated + pull surface), session-window context efficiency, temporal search filters and decay scoring, agent identity injection via MCP, scheduled-task health observability, curated metrics timeseries with Studio charts, and deterministic distill validation gate. 3038 tests PASS. |
| **v0.8.0** | ⬜ planned | Enrichment: groundwork for richer note sources (document ingestion / OCR evaluation). |
| **v0.9.0** | ⬜ planned | Memory optimization: deterministic retrieval and lifecycle improvements. |
| **v1.0.0** | ⬜ planned | Complete vault: full multi-vault management, local sovereignty (gateway + engine). |
| **v2.0.0** | ⬜ planned | Agent runtime — terminal agent that reasons over the codebase using the vault as its memory. |

> **Current release**: **v0.7.6**. Full roadmap: **[gradatum.org](https://gradatum.org)**.

---

## What's shipped (v0.1.0 → v0.7.6)

A condensed view — the authoritative log lives in [CHANGELOG.md](CHANGELOG.md).

| Milestone | What it delivered |
|---|---|
| **v0.1.0** | First working knowledge store: write, search, and retrieve notes over HTTP and MCP. Background job queue, hybrid search, bearer auth. |
| **v0.2.0 – v0.3.7** | Background jobs that survive restarts and never silently drop failures. Pluggable storage (swap the database without rewriting the app). Built-in LLM proxy and cost attribution. First public OSS release on GitHub + crates.io. |
| **v0.4.0 – v0.4.3** | Durable memory: full version history for every note, safe concurrent writes, stable internal links, trust-scored sources, automatic compaction, and semantic forget. |
| **v0.5.2** | Code awareness: index any Rust codebase from source (no LLM needed), search by symbol or file, updates in milliseconds. Encrypted connections, chronological note browsing, agent action tracing. |
| **v0.5.5** | Foundation polish before the MCP pivot: real-time health endpoint with version proof, Rust 2024 edition, API surface hardened. |
| **v0.6.0** | Native MCP server — any MCP-compatible client (Claude Code, IDEs, custom agents) connects directly over standard HTTP. Notes validated and auto-repaired on write. |
| **v0.6.4** | Security baseline + code-map for 5 languages + correctness audit. Studio sessions expire in 1h, strict browser security policy, request size limits. Code index understands Rust, Bash, TypeScript, React, and Python. 12 correctness fixes from a deep audit round. 2337 tests PASS. |
| **v0.7.6 ✅ current** | Memory intelligence layer: assembled context pipeline, proactive recall, session-window context efficiency, temporal search and decay, agent identity injection via MCP, scheduled-task health observability, curated metrics timeseries with Studio charts, and deterministic distill validation gate. 3038 tests PASS. |

---

## What's new in v0.7.6

All features below are available in the v0.7.6 release. See [CHANGELOG.md](CHANGELOG.md) `[0.7.6]` and [ARCHITECTURE.md](ARCHITECTURE.md) for the full technical detail.

**Assembled context pipeline**

`vault_context` is now a full context-assembly pipeline rather than a raw FTS dump. It runs BM25 + semantic retrieval, fuses results via Reciprocal Rank Fusion, applies a composite score (recency × PageRank × trust), and produces budget-aware structured Markdown — inlining the highest-scoring notes and returning lighter stubs for the rest. Agents can dereference stubs on demand via `vault_read`. A session window tracks which notes have already been sent inline, avoiding redundant re-delivery across turns.

**Proactive recall**

The server now surfaces relevant memory unprompted. A background task periodically derives an implicit query from the most recently written notes, runs a cross-section retrieval, and stores the top results. Agents pull this surface via `POST /api/v1/proactive_recall`. An acceptance feedback endpoint (`/proactive_recall/feedback`) records which surfaced notes were actually used — groundwork for feedback-weighted surfacing in a future release.

**Temporal search and decay**

`vault_search` accepts `from_ms` / `to_ms` epoch bounds to filter results by when an event occurred — not just when the note was created. Notes can carry an `occurred_at` timestamp (ISO 8601) at write time. The recency scoring signal uses this canonical anchor rather than the creation timestamp, making composite scores reflect real event timing.

**Agent identity injection via MCP**

A protected `identity` section stores per-agent soul notes (structured Markdown with invariants and behavioral guidelines). On MCP `initialize`, the server injects the requesting agent's identity note into the MCP `instructions` field — no client-side setup required.

**Scheduled task health**

A new System page in the Studio shows the health of all background tasks: last run time, outcome (ok / error / overdue), error count in the last 24 hours, and run duration. All internal maintenance tasks are instrumented. The same data is available via `GET /api/v1/system/scheduled`.

**Curated metrics timeseries**

The server collects a curated set of ~60 operational metrics every 60 seconds, stored in a `metric_sample` table with 14-day retention. The Studio System page displays these as interactive time-series charts (1 h / 24 h / 7 d / 14 d range, auto-refresh). REST endpoints at `/api/v1/system/metrics/catalog` and `/api/v1/system/metrics/timeseries` expose the same data programmatically.

**Distill validation gate**

Synthesized notes produced by the distillation job now pass through a deterministic quality-scoring gate before being stored. The scorer computes a composite of embedding-grounding, source trust, source recency, numeric coherence, and orphan-entity checks — all without an LLM call. Notes scoring below the threshold are stored with a degraded trust value and a `quality-low` tag rather than being discarded.

**MCP: 23 tools**

Two additional MCP tools — `vault_proactive_recall` and `vault_proactive_recall_feedback` — bring the total MCP surface to **23 tools**.

**Status**: **3038 tests PASS** (optimized build), clippy 0, fmt clean, `cargo deny` GREEN.

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

Gradatum is in **alpha (v0.7.6)**. Three installation paths are available depending on your use case.

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
# Replace vX.Y.Z with the release tag, e.g. v0.7.6
VERSION=v0.7.6
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

### Option B — crates.io

The full gradatum workspace (**27 crates**) is published on crates.io at `v0.7.6`. These are **source releases** — the crate API is not stable before v1.0.0 and breaking changes will occur.

```toml
# Cargo.toml — add individual crates as needed
gradatum-core = "0.7"     # core types, note model, ACL
gradatum-vault = "0.7"    # vault read/write/lifecycle
gradatum-search = "0.7"   # hybrid search (BM25 + semantic)
gradatum-embed = "0.7"    # dense embeddings
gradatum-ingest = "0.7"   # code index (tree-sitter)
```

> **APIs are not stable before v1.0.0.** `cargo add gradatum` gives you the meta-crate (re-exports). Individual crates (`gradatum-core`, `gradatum-vault`, …) are the intended entry points. See [crates.io/crates/gradatum](https://crates.io/crates/gradatum) for the full list.

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

Gradatum exposes its 23-tool memory surface over MCP. First, create an API key — long-lived and revocable — with the scopes the tools need:

```bash
gradatum-admin api-key create \
  --root /var/lib/gradatum \
  --owner claude-code \
  --scopes vault_read,vault_search,vault_write
# Prints the secret once (ak_...). Store it with mode 600, e.g. ~/.config/gradatum/api-key
```

Two transports are available.

**Option 1 — stdio bridge (`gradatum-mcp-stub`).** The stub runs as a stdio MCP server and proxies to the HTTP backbone, handling the auth flow automatically (api-key → token exchange → JWT with TTL auto-refresh):

```json
{
  "mcpServers": {
    "gradatum": {
      "command": "gradatum-mcp-stub",
      "args": [],
      "env": {
        "GRADATUM_SERVER_URL": "http://127.0.0.1:19090",
        "GRADATUM_API_KEY_FILE": "/path/to/api-key"
      }
    }
  }
}
```

**Option 2 — native HTTP endpoint.** `gradatum-server` serves MCP directly over Streamable HTTP at `/mcp` — no separate bridge process. Point an HTTP MCP client at it with the API key as a Bearer credential (no token-refresh needed; the key is long-lived and revocable):

```json
{
  "mcpServers": {
    "gradatum": {
      "type": "http",
      "url": "http://127.0.0.1:19090/mcp",
      "headers": {
        "Authorization": "Bearer ak_your_api_key"
      }
    }
  }
}
```

---

### Studio login & API-key lifecycle

Gradatum ships a web-based Studio UI at the root path (`/`). Authentication uses an **API key** (`ak_…`).

**Login flow**

1. Enter the API key on the Studio login screen.
2. The browser posts it to `POST /auth/exchange` and receives a **JWT** stored in `localStorage`. The original key is not retained after the exchange.
3. Sessions expire after **1 hour**; a new login (key → JWT exchange) is required after expiry.
4. The Studio requires a key with scope **`admin`**.

**Create an admin key**

```bash
gradatum-admin api-key create \
  --root /var/lib/gradatum \
  --owner studio-admin \
  --scopes admin \
  --description "studio login"
# Prints the secret once (ak_...). Store it securely — it cannot be retrieved later.
```

> **Secret is shown once.** The key is stored as an Argon2id hash; the plaintext `ak_…` value is never retrievable after creation. If the value is lost, use `rotate`.

**Manage keys**

```bash
# List all keys — shows prefix, owner, scopes, and status (never the secret)
gradatum-admin api-key list --root /var/lib/gradatum

# Rotate a key — revokes the current one and prints a new secret once
gradatum-admin api-key rotate <prefix> --root /var/lib/gradatum

# Revoke a key immediately
gradatum-admin api-key revoke <prefix> --root /var/lib/gradatum
```

**Lost your key?** Run `list` to find the prefix, then `rotate <prefix>` to get a new secret.

---

## Vocabulary

| Term | Meaning |
|---|---|
| **Vault** | The technical backing store (SQLite + FTS5 + Markdown). Separate vaults (main, staging, bench-*) readable side-by-side today; full multi-vault management lands at v1.0. |
| **Locus** | A logical subdivision of a vault, isolated by ACL. From Cicero's *ars memoriae*. |
| **Section** | One of the cognitive categories: `decisions`, `architecture`, `debug`, `reasoning`, `feedback`, `lessons-learned`, `retrospectives`, `experiments`, `agent-issues`, `reference`, `council`, `project-map`, `identity` |
| **Note** | Atomic Markdown file with YAML frontmatter |
| **Bearer / Consumer** | An authenticated identity with read/write ACL patterns |
| **Preset** | A template configuration shipped in `crates/gradatum-admin/presets/` |

---

## Contributing

Gradatum is in alpha, built openly on lessons learned from a prior private system. Issues and PRs are welcome while the project is in alpha; expect fast-moving internals before v1.0. See [CONTRIBUTING.md](CONTRIBUTING.md) and [GOVERNANCE.md](GOVERNANCE.md) for the process.

---

## License

[Apache-2.0](LICENSE)
