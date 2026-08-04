# Gradatum

> **Memory backbone for AI agents — graduated.**

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Status: Stable](https://img.shields.io/badge/Status-Stable-brightgreen.svg)](CHANGELOG.md)
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
| **Multi-vault** | Separate `main` from `staging` and `bench-*` vaults for testing, migration, A/B prompts. Vault lifecycle management (provision, suspend, soft-delete, purge) ships in `1.0.0`, gated behind `multi_tenant.enabled`. |
| **Hierarchical ACL** | Bearer-scoped access to memory loci, fail-closed by default. Configure from a shipped preset (`hierarchical`, `flat`) or write your own. |
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
| [**rmcp**](https://github.com/modelcontextprotocol/rust-sdk) | Native MCP server over Streamable HTTP — tool surface at `/mcp` |
| [**SQLx**](https://github.com/launchbadge/sqlx) | Async SQLite driver — vault index, job queue, sessions |
| [**Apalis**](https://github.com/geofmureithi/apalis) | Background job queue (SQLite-backed, DLQ, per-kind routing, monitoring) |
| [**OpenDAL**](https://github.com/apache/opendal) | Storage abstraction — local FS today; S3/GCS/Azure planned (feature flags reserved, no backend implemented yet) |
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
| **Vault + storage** | `gradatum-vault`, `gradatum-storage` | Markdown source of truth + OpenDAL storage abstraction (local FS backend; S3/GCS/Azure planned) |
| **Lifecycle** | `gradatum-warden` | IP CIDR allowlist + per-IP rate limiting + loopback bypass |
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

## Real Live Setup feedback

Field notes from running gradatum's gateway + engine layer on an AMD AI HX 395 MAX 128 Go mini-PC — 128 GB unified memory, 8060S iGPU, Vulkan backend. Highlights: a default `llama-server` cache setting caused ~27× slower turns under concurrent sessions until tuned; non-zero sampling penalties can silently fall off the GPU path and cost ~34% decode throughput; speculative decoding (MTP) is a net loss on some model architectures and a clear win on others.

Full write-up, numbers, and fixes: **[REALFEEDBACK.md](REALFEEDBACK.md)**.

---

## Roadmap

**On crates.io**: [crates.io/crates/gradatum](https://crates.io/crates/gradatum) · Apache-2.0 · Rust 1.91+

Gradatum is built in three chapters: **memory first, then agents, then the sovereign terminal**.

| Version | Status | What it brings |
|---|---|---|
| **0.1.0-alpha – v0.4.3** | ✅ shipped | Working knowledge store: write, search, trust-scored sources, version history, stable links, lifecycle (forget, compact, distil). |
| **v0.5.2** | ✅ shipped | Code awareness: index any codebase from source, search by symbol or file. Optional native TLS termination, chronological browsing, agent tracing. |
| **v0.6.4** | ✅ shipped | Native MCP server — any MCP client connects directly over HTTP. Security baseline, 5-language code-map, 12 correctness fixes. Health endpoint with version proof, hardened API surface. 2337 tests PASS. |
| **v0.7.6** | ✅ shipped | Memory intelligence layer: assembled context pipeline (BM25 + semantic + RRF + composite scoring), proactive recall (server-initiated + pull surface), session-window context efficiency, temporal search filters and decay scoring, agent identity injection via MCP, scheduled-task health observability, curated metrics timeseries with Studio charts, and deterministic distill validation gate. |
| **v0.8.0** | 🔶 in repo | Reversible delete: an on-demand delete archives the note instead of destroying it, with registry-driven retention GC and operator-only restore. |
| **v1.0.0** | ✅ shipped | First stable release. Multi-tenant / multi-vault isolation foundation, multi-user identity, per-note usage salience, FR→EN **user-facing** string migration complete (runtime literals, CLI, HTTP API) — internal rustdoc migration deferred to a `1.x` minor, SemVer strict from here. |
| **v2.0.0** | ⬜ planned | Agent runtime — terminal agent that reasons over the codebase using the vault as its memory. |

> **What the Status column means.** ✅ *shipped* = the milestone is complete in this repository
> and carries a git tag. A git tag and a crates.io release are independent facts: a tagged
> milestone is not necessarily on the registry. 🔶 *in repo* = the work is merged and its
> changelog entry is written.
>
> For what is on the registry at any given moment,
> **[crates.io/crates/gradatum](https://crates.io/crates/gradatum)** is authoritative — this
> document does not mirror it.

> Full roadmap: **[gradatum.org](https://gradatum.org)**.

---

## What's shipped (0.1.0-alpha → v1.0.0)

A condensed view — the authoritative log lives in [CHANGELOG.md](CHANGELOG.md).

| Milestone | What it delivered |
|---|---|
| **0.1.0-alpha** | First working knowledge store: write, search, and retrieve notes over HTTP and MCP. Background job queue, hybrid search, bearer auth. |
| **v0.2.0 – v0.3.7** | Background jobs that survive restarts and never silently drop failures. Pluggable storage (swap the database without rewriting the app). Built-in LLM proxy and cost attribution. First public OSS release on GitHub + crates.io. |
| **v0.4.0 – v0.4.3** | Durable memory: full version history for every note, safe concurrent writes, stable internal links, trust-scored sources, automatic compaction, and semantic forget. |
| **v0.5.2** | Code awareness: index any Rust codebase from source (no LLM needed), search by symbol or file, updates in milliseconds. Optional native TLS termination, chronological note browsing, agent action tracing. |
| **v0.6.4** | The MCP pivot, shipped in one release: native MCP server — any MCP-compatible client (Claude Code, IDEs, custom agents) connects directly over standard HTTP; notes validated and auto-repaired on write; real-time health endpoint with version proof and a hardened API surface. Plus the security baseline + code-map for 5 languages + correctness audit: Studio sessions expire in 1h, strict browser security policy, request size limits. Code index understands Rust, Bash, TypeScript, React, and Python. 12 correctness fixes from a deep audit round. 2337 tests PASS. |
| **v0.7.6** | Memory intelligence layer: assembled context pipeline, proactive recall, session-window context efficiency, temporal search and decay, agent identity injection via MCP, scheduled-task health observability, curated metrics timeseries with Studio charts, and deterministic distill validation gate. |
| **v0.8.0** | Reversible delete: an on-demand delete archives the note (`.md` + `.history/` moved under `.archive/`) instead of destroying it. Registry-driven retention GC, restore-to-quarantine, operator-only CLI surface, read-only archive listing over MCP. |
| **v1.0.0 ✅ shipped** | First stable release. Multi-tenant / multi-vault isolation foundation, multi-user identity with `jti` audit attribution, per-note usage salience, retrospective audit/dedup job, `build_sha` in `--version`. FR→EN **user-facing** string migration complete (runtime literals, CLI, HTTP API); internal rustdoc migration is deferred to a `1.x` minor. The workspace suite is run with `cargo nextest run --workspace --release --no-fail-fast`; that run is the authoritative count, not any figure written here. |

---

## What's new in v1.0.0

`1.0.0` is the first stable release. SemVer strict starts at `1.0.0`, and
public APIs on `1.x` will follow the LTS promise in [RELEASE-POLICY.md](RELEASE-POLICY.md). See [CHANGELOG.md](CHANGELOG.md) `[1.0.0]` for the full list, including **breaking changes** and **deprecations**.

**Multi-tenant vault isolation foundation**

A `VaultGrant` substrate backs a per-vault handle registry with full vault lifecycle management (provision, suspend, soft-delete, purge). Every read/write path — notes, search, ANN, archive, temporal index, cache, jobs, config — is scoped through an ACL-checked vault handle instead of a single implicit vault. Gated behind `multi_tenant.enabled` (default `false`): single-vault deployments behave identically to `0.x`.

**Multi-user identity**

JWT identity issuance is governed by a configurable allow-list of keys, and the JWT `jti` is propagated into the audit trail for per-identity attribution. Per-key **write**-scope enforcement is active only under `multi_tenant.enabled`, and requires the key to carry one of `write`, `admin`, `service`. Read access is never governed by key scopes — see [SECURITY.md](SECURITY.md) *Known limitations*.

**Per-note usage salience**

An opt-in, default-off usage-weighted salience factor (reads, search hits, top-3 surfacing, recall acceptance) can be folded into `vault_search` ranking. A companion audit path detects notes that have become irrelevant and can auto-downgrade them.

**Operational proof**

`build_sha` is reported by `--version` for `gradatum-server` and `gradatum-worker`, with deploy-time verification — making "what's actually running" a checkable fact.

---

## Memory intelligence layer (shipped in v0.7.6)

See [CHANGELOG.md](CHANGELOG.md) `[0.7.6]` and [ARCHITECTURE.md](ARCHITECTURE.md) for the full technical detail.

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

**MCP tools**

Two tools are new in the upcoming `1.0.0`: `create_feature_card`, which has the server assign a project-map feature number rather than the caller, and `job_status`, which resolves an asynchronous write by polling a job until it reports `terminal = true`. For the exact set a given build exposes, call `tools/list` on the running server — that response is authoritative, not any count written here.

---

## Documentation

### Project overview and design

| Document | Purpose |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Technical design — planes, ACL hierarchy, source-of-truth model, search pipeline, concurrency, storage layout. |
| [DEPENDENCIES.md](DEPENDENCIES.md) | Workspace dependency tree, level invariants, version pinning policy. |
| [PORTS.md](PORTS.md) | Default port matrix (`19090 + offset`) and TOML/env override conventions. |
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
| [packaging/systemd/README.md](packaging/systemd/README.md) | Systemd unit reference — server, worker, engine template, gateway service, smoke tests. |

> **Security note**: the default ACL is fail-closed — with no preset file present, every locus is denied. Configure an ACL preset (`[acl] preset_path`, a TOML of `[[consumer]]` blocks — from a shipped preset or your own) to grant access. See [SECURITY.md](SECURITY.md) for the hardening defaults and their known limitations.

### Agent skills

Gradatum ships an optional companion plugin, **[gradatum-skills](https://github.com/gradatum/gradatum-skills)** (Apache-2.0, separate repository). It packages **10 skills** that teach an agent harness *when* to reach for the vault and *which* MCP tool to call — a search-before-write discipline, section routing, just-in-time lesson recall, and a code-navigation path over the derived code index.

The skills contain no script, no binary and no local dependency: each one names an MCP tool of the `gradatum` server and the harness performs the call. Transport, authentication and response format belong to the server, not to the plugin. See that repository's `README.md` and `ARCHITECTURE.md` for the skill catalogue and the L1 → L0 composition model.

### Platform-specific guides (archived)

| Document | Purpose |
|---|---|
| [docs/WINDOWS-GUIDE.md](docs/WINDOWS-GUIDE.md) | Windows guide — **deferred** (archived, Linux-only as of 2026-06-05). |
| [docs/KNOWN_ISSUES-WINDOWS.md](docs/KNOWN_ISSUES-WINDOWS.md) | Windows known issues — **deferred** (archived, Linux-only as of 2026-06-05). |

---

## Installation

Gradatum is at **v1.0.0**. Three installation paths are available depending on your use case.

> Public APIs follow SemVer strict from `1.0.0`. Breaking changes are documented in [CHANGELOG.md](CHANGELOG.md).

### Option A — Pre-built binaries (recommended for deployment)

**Platform: Linux x86_64.** Each [GitHub Release](https://github.com/gradatum/gradatum/releases) ships the archives below:

| Archive | Contents | Use case |
|---|---|---|
| `gradatum-server-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-server`, `gradatum-worker`, `gradatum-admin` | Vault backbone — run on your application host |
| `gradatum-llm-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-gateway`, `gradatum-engine` | LLM routing and inference supervision — run on your GPU host (or alongside the backbone) |
| `gradatum-mcp-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-mcp-stub` | MCP bridge for Claude Code / Claude Desktop |
| `gradatum-sbom-vX.Y.Z.tar.gz` | One CycloneDX SBOM (`.cdx.json`) per publishable crate | Supply-chain review — dependency inventory of the released source |

A `SHA256SUMS` file covering every archive above is also attached to each release. Every archive ships with a **SLSA provenance attestation**.

**Example — deploying the vault backbone:**

```bash
# Replace vX.Y.Z with the release tag, e.g. v1.0.0
VERSION=v1.0.0
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

The gradatum workspace has **31 members**, of which **27 are publishable** to crates.io (4 carry `publish = false`). These are **source releases**; from `1.0.0` their public APIs will follow SemVer strict.

```bash
# Add individual crates as needed — cargo resolves the current version from the registry
cargo add gradatum-core     # core types, note model, ACL
cargo add gradatum-vault    # vault read/write/lifecycle
cargo add gradatum-search   # hybrid search (BM25 + semantic)
cargo add gradatum-embed    # dense embeddings
cargo add gradatum-ingest   # code index (tree-sitter)
```

> `cargo add gradatum` gives you the meta-crate (re-exports). Individual crates (`gradatum-core`, `gradatum-vault`, …) are the intended entry points. See [crates.io/crates/gradatum](https://crates.io/crates/gradatum) for the full list.
>
> **`gradatum-cli` is not part of the `1.0.0` release.** It is a placeholder that was published once at `0.7.6`; that version remains on crates.io and is installable, but it is not republished at `1.0.0` and has no implementation. A real CLI is expected with the agent runtime at `2.0.0`.

### Option C — Build from source

**Prerequisites:** Rust stable (MSRV 1.91), a C linker (`gcc` / `clang`), and SQLite 3.x development headers (e.g. `libsqlite3-dev` on Debian/Ubuntu).

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

**Automated install.** The repo ships an idempotent install script that handles systemd units, the `gradatum` system user (UID 985), and service startup:

```bash
# Vault backbone (server + worker + admin)
sudo bash scripts/install-gradatum-services.sh --build

# With engine (supervisor for llama-server) — adds the systemd template and example configs
sudo bash scripts/install-gradatum-services.sh --build --with-engine

# With gateway (LLM router) in addition to engine
sudo bash scripts/install-gradatum-services.sh --build --with-engine --with-gateway
```

For subsequent deploys (binary update without re-init), use the deploy script:

```bash
# Server + worker only
bash scripts/deploy-gradatum-local.sh --build

# Server + worker + engine (restarts active gradatum-engine@* instances)
bash scripts/deploy-gradatum-local.sh --build --engine
```

See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for the full engine configuration reference, and [packaging/systemd/README.md](packaging/systemd/README.md) for the systemd unit reference.

### Agent skills (optional)

The three paths above install Gradatum itself. The agent-facing skills are distributed separately, as the **[gradatum-skills](https://github.com/gradatum/gradatum-skills)** plugin (Apache-2.0) — see [Agent skills](#agent-skills) under Documentation for what they do.

**Server requirement.** The skills name MCP tools by their exact names, including `job_status`, which the write path polls to resolve an asynchronous write. A server older than **v1.0.0** does not expose it: `0.7.6` predates the tool, and a skill naming a tool the server does not expose fails at call time rather than at install time. Install the skills against a `v1.0.0` or later server.

**Install by syncing from a committed reference — never by symlinking the clone.** A symlink turns every edit in the repository into an immediate production change, removing the window in which a commit or a review can happen. The canonical procedure is the `SYNC-INSTALL` block under *Installation* in the [gradatum-skills `README.md`](https://github.com/gradatum/gradatum-skills); it is executed verbatim by that repository's test suite, which is why it lives there and is not duplicated here. It needs only `git` and `tar`, is idempotent, and removes skills that left the product while leaving unrelated skills untouched.

**Verify that the installation is operational, not merely present.** The plugin ships two independent checks — one asserting that every installed skill matches the committed reference, the other asserting that every MCP tool named by a skill actually exists in the list the server exposes. The second catches the failure the first cannot see: a correctly installed skill that names a tool your server does not have. Both are documented under *Vérifier que l'installation est opérationnelle* in the plugin's `README.md`.

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
gradatum-admin vault create <vault-id>
```

### MCP integration (Claude Code / Claude Desktop)

Gradatum exposes its memory surface over MCP. First, create an API key — long-lived, and revocable for future token issuance — carrying a scope that permits writes:

```bash
gradatum-admin api-key create \
  --root /var/lib/gradatum \
  --owner claude-code \
  --scopes write
# Prints the secret once (ak_...). Store it with mode 600, e.g. ~/.config/gradatum/api-key
```

> **The write scopes are a closed set: `write`, `admin`, `service`.** With `multi_tenant.enabled = true`, every write path requires the key to carry at least one of those three, matched by exact string equality (`WRITE_SCOPES`, `gradatum-acl-auth`). Any other value — including `vault_write` — yields a read-only key that takes `403 write scope required (read-only token)` on every write. With `multi_tenant.enabled = false` (the default) scopes are not checked at all, so the same key writes fine until multi-tenant mode is turned on. `api-key create` enforces the same set at creation time: a scope set that grants no write access is **refused**, unless you pass `--read-only` to confirm a read-only key is intended. The check covers creation only — `api-key rotate` carries the source key's scopes over unchanged, and keys minted before this release are not revalidated, so an existing key may still name a scope that grants nothing.
>
> Read access is not governed by key scopes in either mode — it is governed by vault grants and the locus ACL.

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

**Option 2 — native HTTP endpoint.** `gradatum-server` serves MCP directly over Streamable HTTP at `/mcp` — no separate bridge process. Point an HTTP MCP client at it with the API key as a Bearer credential (no token-refresh needed; the key is long-lived):

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

Gradatum ships a web-based Studio UI at `/ui/` (`http://<host>:19090/ui/`). Authentication uses an **API key** (`ak_…`).

**Login flow**

1. Enter the API key on the Studio login screen.
2. The browser posts it to `POST /auth/exchange` and receives a **JWT** stored in `localStorage`. The original key is not retained after the exchange.
3. Sessions expire after **1 hour**; a new login (key → JWT exchange) is required after expiry.
4. **Any valid (non-revoked) API key logs in.** `POST /auth/exchange` verifies the key's
   Argon2id hash and the tenant allow-list — it does **not** inspect the key's scopes. The
   `scope: "human"` the Studio sends selects the **token TTL** (1 h), not a permission level.

> **`admin` is a recommended convention, not an enforced requirement.** The key's scopes are
> copied into the JWT and checked only on **write** paths, and only when
> `multi_tenant.enabled = true` — where any of `write`, `admin` or `service` is accepted
> (`WRITE_SCOPES`). With `multi_tenant.enabled = false` (the default), no scope is checked at
> all: a key with an empty or arbitrary scope list has the same access as an `admin` key.
> Grant `admin` so that access stays correct if you later enable multi-tenancy.

**Create a key for the Studio**

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
gradatum-admin api-key rotate --prefix <prefix> --root /var/lib/gradatum

# Revoke a key — blocks new token issuance from it
gradatum-admin api-key revoke --prefix <prefix> --root /var/lib/gradatum
```

**Lost your key?** Run `list` to find the prefix, then `rotate --prefix <prefix>` to get a new secret.

> **Revocation is not immediate for tokens already issued.** `revoke` and `rotate` stop a key being exchanged for new JWTs, but a JWT minted before the revocation stays valid until it expires (1 h for `human` scope, 24 h for service scope). To cut every outstanding token at once, rotate the signing seed as described in [SECURITY.md](SECURITY.md).

---

## Vocabulary

| Term | Meaning |
|---|---|
| **Vault** | The technical backing store (SQLite + FTS5 + Markdown). Separate vaults (main, staging, bench-*) are readable side-by-side; vault lifecycle management ships in `1.0.0` behind `multi_tenant.enabled`. |
| **Locus** | A logical subdivision of a vault, isolated by ACL. From Cicero's *ars memoriae*. |
| **Section** | One of the cognitive categories: `decisions`, `architecture`, `debug`, `reasoning`, `feedback`, `lessons-learned`, `retrospectives`, `experiments`, `agent-issues`, `reference`, `council`, `project-map`, `identity` |
| **Note** | Atomic Markdown file with YAML frontmatter |
| **Bearer / Consumer** | An authenticated identity with read/write ACL patterns |
| **Preset** | A template configuration shipped in `crates/gradatum-admin/presets/` |

---

## Contributing

Gradatum is built openly on lessons learned from a prior private system. Issues and PRs are welcome. Public APIs are stable from `1.0.0`; internals may still move between minor releases. See [CONTRIBUTING.md](CONTRIBUTING.md) and [GOVERNANCE.md](GOVERNANCE.md) for the process.

---

## License

[Apache-2.0](LICENSE)
