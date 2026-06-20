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

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design.

### OSS stack

Gradatum is built on these open-source foundations:

| Library | What it does in gradatum |
|---|---|
| [**Tokio**](https://github.com/tokio-rs/tokio) | Async runtime — all I/O, timers, and task scheduling |
| [**Axum**](https://github.com/tokio-rs/axum) | HTTP server (REST endpoints + studio `ServeDir`) |
| [**Tower**](https://github.com/tower-rs/tower) / **tower-http** | Middleware stack — rate-limiting, CORS, auth, body limits |
| [**rmcp**](https://github.com/modelcontextprotocol/rust-sdk) | Native MCP server over Streamable HTTP (21 tools at `/mcp`) |
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

**Current release**: `v0.6.4` · Apache-2.0 · 2337 tests PASS · Rust 1.88+

Gradatum is built in three chapters: **memory first, then agents, then the sovereign terminal**.

| Version | Status | What it brings |
|---|---|---|
| **v0.1.0 – v0.4.3** | ✅ shipped | Working knowledge store: write, search, trust-scored sources, version history, stable links, lifecycle (forget, compact, distil). |
| **v0.5.2** | ✅ shipped | Code awareness: index any codebase from source, search by symbol or file. Encrypted connections, chronological browsing, agent tracing. |
| **v0.5.5** | ✅ shipped | Health endpoint with version proof, Rust 2024 edition, hardened API surface. |
| **v0.6.0** | ✅ shipped | Native MCP server — any MCP client connects directly over HTTP. |
| **v0.6.4** | ✅ **current** | Security baseline, 5-language code-map, 12 correctness fixes. 2337 tests PASS. |
| **v0.7.0** | ⬜ planned | Memory layer — context assembly, proactive recall, sliding-window memory across sessions. |
| **v0.8.0** | ⬜ planned | gradatum-code — terminal agent that reasons over the codebase using the vault as its memory. Runs entirely on local hardware. |
| **v1.0.0** | ⬜ planned | Stable API contracts (semver), multi-user + OAuth login, 30-day production proof. |
| **v2.0.0** | ⬜ planned | Multimodal inputs (images, audio, documents) + long-horizon memory consolidation. |

> **We are here**: **v0.6.4** — current public release. Full roadmap: **[gradatum.org](https://gradatum.org)**.

---

## What's shipped (v0.1.0 → v0.6.4)

A condensed view — the authoritative log lives in [CHANGELOG.md](CHANGELOG.md).

| Milestone | What it delivered |
|---|---|
| **v0.1.0** | First working knowledge store: write, search, and retrieve notes over HTTP and MCP. Background job queue, hybrid search, bearer auth. |
| **v0.2.0 – v0.3.7** | Background jobs that survive restarts and never silently drop failures. Pluggable storage (swap the database without rewriting the app). Built-in LLM proxy and cost attribution. First public OSS release on GitHub + crates.io. |
| **v0.4.0 – v0.4.3** | Durable memory: full version history for every note, safe concurrent writes, stable internal links, trust-scored sources, automatic compaction, and semantic forget. |
| **v0.5.2** | Code awareness: index any Rust codebase from source (no LLM needed), search by symbol or file, updates in milliseconds. Encrypted connections, chronological note browsing, agent action tracing. |
| **v0.5.5** | Foundation polish before the MCP pivot: real-time health endpoint with version proof, Rust 2024 edition, API surface hardened. |
| **v0.6.0** | Native MCP server — any MCP-compatible client (Claude Code, IDEs, custom agents) connects directly over standard HTTP. Notes validated and auto-repaired on write. |
| **v0.6.4 ✅ current** | Security baseline + code-map for 5 languages + correctness audit. Studio sessions expire in 1h, strict browser security policy, request size limits. Code index understands Rust, Bash, TypeScript, React, and Python. 12 correctness fixes from a deep audit round. 2337 tests PASS. |

---

## What's new in v0.6.4

All features below are deployed LIVE on `:19090`. See [CHANGELOG.md](CHANGELOG.md) `[0.6.4]` and [ARCHITECTURE.md](ARCHITECTURE.md) for the full technical detail.

**Connect any agent directly (native MCP)**

The vault is now directly accessible from any MCP-compatible client — Claude Code, IDEs, custom agents — over a standard HTTP connection at `/mcp`. No sidecar process, no protocol shim. 21 tools available including search, write, classify, timeline, and code-scope.

**Code index in five languages**

The code index now understands Rust, Bash, TypeScript, React (TSX), and Python — built from source using tree-sitter, no LLM required. Find any function, type, or file by name or keyword. Updates in milliseconds when files change.

**Security hardening**

- Studio sessions now expire in **1 hour** (previously 24h) — shorter exposure window if credentials are compromised
- Strict browser security policy on the admin UI: no external scripts can run, clickjacking protection for all browsers
- MCP endpoint capped at **512 KB** per request (anti-DoS)
- Authentication required to list available tools (previously open)

**Correctness (12 fixes from deep audit)**

A full round-2 audit caught and fixed 12 real bugs, including: note deletion failing on legacy note layouts, embedding index validation errors, and internal identifiers accidentally accessible from outside the library. See [CHANGELOG.md](CHANGELOG.md) for the full list.

**Status** : deployed **LIVE** on `:19090`. **2337 tests PASS** (optimized build), clippy 0, fmt clean, `cargo deny` GREEN.

---

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

Gradatum is in **alpha (v0.6.4)**. Three installation paths are available depending on your use case.

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
# Replace vX.Y.Z with the release tag, e.g. v0.6.4
VERSION=v0.6.4
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

The full gradatum workspace (**27 crates**) is now published on crates.io at `v0.6.4`. These are **source releases** — the crate API is not stable before v1.0.0 and breaking changes will occur.

```toml
# Cargo.toml — add individual crates as needed
gradatum-core = "0.6"     # core types, note model, ACL
gradatum-vault = "0.6"    # vault read/write/lifecycle
gradatum-search = "0.6"   # hybrid search (BM25 + semantic)
gradatum-embed = "0.6"    # dense embeddings
gradatum-ingest = "0.6"   # code index (tree-sitter)
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

Gradatum exposes its 21-tool memory surface over MCP. First, create an API key — long-lived and revocable — with the scopes the tools need:

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

**Option 2 — native HTTP endpoint (v0.6.0+).** `gradatum-server` serves MCP directly over Streamable HTTP at `/mcp` — no separate bridge process. Point an HTTP MCP client at it with the API key as a Bearer credential (no token-refresh needed; the key is long-lived and revocable):

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
