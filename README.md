# Gradatum

> **Memory backbone for AI agents — graduated.**

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Status: Stable](https://img.shields.io/badge/Status-Stable-brightgreen.svg)](CHANGELOG.md)
[![Website](https://img.shields.io/badge/Website-gradatum.org-brightgreen)](https://gradatum.org)

**[Quickstart](#quickstart)** · **[Architecture](ARCHITECTURE.md)** · **[Docs](#documentation)** · **[Changelog](CHANGELOG.md)** · **[Upgrading 2.0→2.1](docs/UPGRADING-2.0.0-to-2.1.0.md)** · **[Upgrading 1.0→2.0](docs/UPGRADING-1.0.0-to-2.0.0.md)** · **[crates.io](https://crates.io/crates/gradatum)** · **[gradatum.org](https://gradatum.org)**

A self-hosted, embedded memory backbone for multi-agent AI systems. Rust + SQLite. Zero external services required.

> Gradatum stores knowledge in **loci** — a name borrowed from Cicero's *Ars Memoriae*, the ancient mnemonic method where memories are placed in mental locations of an imagined palace. Agents don't share rooms — they share *places of memory*.

<!-- Studio screenshot reserved here. None is currently inserted — the Studio UI bundle is not
     shipped in any release archive, Docker image, or `cargo build` output (see
     docs/guides/D-mcp-and-studio.md § Studio login & API-key lifecycle); a screenshot of a
     feature that isn't installable by following this README would be a false claim by
     illustration. When the bundle ships, add the image under docs/assets/ and reference it by
     absolute URL pinned to the release tag (raw.githubusercontent.com/gradatum/gradatum/<tag>/docs/assets/…),
     never a relative path — relative paths break on crates.io's rendered README. -->

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

## Quickstart

Three ways to run gradatum, in order of speed:

| Path | Best for | |
|---|---|---|
| **Docker Compose** | Trying it out locally (Linux, or Windows via Docker Desktop/WSL2), no Rust toolchain | [Guide A →](docs/guides/A-docker-quickstart.md) |
| **Pre-built binaries** | Deploying on Linux x86_64 | [Guide B →](docs/guides/B-install-binaries.md) |
| **crates.io / build from source** | Embedding as a library, or Linux arm64 | [Guide C →](docs/guides/C-build-from-source.md) |

Platform support: Linux native · Windows via Docker · no macOS — see
[docs/DEPLOYMENT.md § Platform support](docs/DEPLOYMENT.md#platform-support).

```bash
git clone https://github.com/gradatum/gradatum.git
cd gradatum
bash scripts/quickstart-docker.sh
```

Then connect an MCP client (Claude Code) or log into the Studio UI — see
**[Guide D — MCP & Studio](docs/guides/D-mcp-and-studio.md)**.

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
| [**rusqlite**](https://github.com/rusqlite/rusqlite) | SQLite driver (bundled) — vault index, job queue, sessions |
| [**Apalis**](https://github.com/geofmureithi/apalis) | Background job queue (SQLite-backed, DLQ, per-kind routing, monitoring) |
| [**OpenDAL**](https://github.com/apache/opendal) | Storage abstraction — local FS (default) or S3 object storage, selected by configuration; GCS/Azure planned |
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
| **Vault + storage** | `gradatum-vault`, `gradatum-storage` | Markdown source of truth + OpenDAL storage abstraction (local FS or S3, by configuration; GCS/Azure planned) |
| **Lifecycle** | `gradatum-warden` | IP CIDR allowlist + per-IP rate limiting + loopback bypass |
| **ACL / auth** | `gradatum-auth`, `gradatum-acl-auth`, `gradatum-acl-policy` | Bearer-scoped access, JWT signing, hierarchical policies |
| **Queue** | `gradatum-queue` | Apalis SQLite-backed job queue, DLQ, per-kind routing |

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

Multi-host layout (separate app-host and GPU host, one `gradatum-engine` instance per model):
see [docs/DEPLOYMENT.md §2](docs/DEPLOYMENT.md#2-example-topology--multi-host-gpu-serving-with-gateway-routing)
for the full diagram and config wiring. The engine layer is optional — with no GPU host, the
gateway falls back to a local CPU engine instance on the app-host.

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
| **v1.0.0** | ✅ shipped | First stable release. Multi-tenant / multi-vault isolation foundation, multi-user identity, per-note usage salience, reversible delete (on-demand delete archives the note, registry-driven retention GC, operator-only restore), FR→EN **user-facing** string migration complete (runtime literals, CLI, HTTP API) — internal rustdoc migration deferred to a `1.x` minor, SemVer strict from here. |
| **v2.0.0** *(Alluvium)* | ✅ shipped | Identity is strictly credential-derived — no default identity, no client-declared `author`, no silent fallback — closing the `1.x` line. Vault storage on an S3-compatible object backend as an alternative to local filesystem, notes written in plaintext with no encryption applied by gradatum itself — see [SECURITY.md § Privacy posture](SECURITY.md#privacy-posture); the network-filesystem startup restriction is removed. Link-edge reconciliation (`gradatum-admin repair-note-links`), Docker deployment, workspace dependency refresh. |
| **v2.1.0** | ✅ shipped | SemVer since v1.0.0, with documented exceptions. v2.1.0 is a minor release that removes public API surface in four crates (`gradatum-core`, `gradatum-engine`, `gradatum-index`, `gradatum-queue`); every removal is inventoried in the release manifest and matched against the previous release in CI. sqlx retired from the entire workspace dependency graph — replaced by rusqlite in the auth, ACL, and job-queue stores; SQLite engine `3.46.0` → `3.53.2`. Legacy `jobs_v2` queue path removed; `compute_distill_trust` moved to the new `gradatum-distill` crate. Read the [2.0.0 → 2.1.0 upgrade guide](docs/UPGRADING-2.0.0-to-2.1.0.md) before moving. |
| — | ⬜ planned | Agent runtime — terminal agent that reasons over the codebase using the vault as its memory. No version is committed to it yet. |

> **What the Status column means.** ✅ *shipped* = the milestone is complete in this repository
> and carries a git tag. 🔄 *in progress* = the milestone's code and CHANGELOG entry are on
> `main`, but no git tag has been cut yet. A git tag and a crates.io release are independent
> facts: a tagged milestone is not necessarily on the registry. For what is on the registry at
> any given moment, **[crates.io/crates/gradatum](https://crates.io/crates/gradatum)** is
> authoritative — this document does not mirror it.

> Full roadmap: **[gradatum.org](https://gradatum.org)**. Per-version detail:
> **[CHANGELOG.md](CHANGELOG.md)**.

---

## Highlights by version

Condensed — the authoritative, detailed log for every version lives in
**[CHANGELOG.md](CHANGELOG.md)**.

- **v2.1.0** — SemVer since v1.0.0, with documented exceptions: this minor release removes
  public API surface in four crates (`gradatum-core`, `gradatum-engine`, `gradatum-index`,
  `gradatum-queue`), each removal inventoried in the release manifest and matched against the
  previous release in CI. sqlx is retired from the entire workspace dependency graph —
  replaced by rusqlite in `gradatum-auth`'s revocation store, `gradatum-acl-auth`'s API-key
  store, and `gradatum-queue`'s job queue; SQLite engine bumped `3.46.0` → `3.53.2`. The legacy
  `jobs_v2` queue path is removed (`GET /api/v1/jobs/:id`, `gradatum_queue::queue` module);
  `compute_distill_trust` moves to the new `gradatum-distill` crate; `KindKind::Chore` /
  `::Spike` are removed for good. `Section`, `HealthSnapshot`, and `DriftScanResult` are now
  `#[non_exhaustive]`. **Breaking changes** — see
  [docs/UPGRADING-2.0.0-to-2.1.0.md](docs/UPGRADING-2.0.0-to-2.1.0.md).
- **v2.0.0** *(Alluvium)* — Identity is strictly credential-derived: no default identity, no
  client-declared `author`, no silent fallback. Vault storage on an S3-compatible object
  backend as an alternative to local filesystem — notes stay in plaintext, no encryption
  applied by gradatum ([SECURITY.md § Privacy posture](SECURITY.md#privacy-posture)). Link-edge
  reconciliation (`gradatum-admin repair-note-links`), Docker deployment. **Breaking changes**
  — see [docs/UPGRADING-1.0.0-to-2.0.0.md](docs/UPGRADING-1.0.0-to-2.0.0.md).
- **v1.0.0** — Multi-tenant / multi-vault isolation foundation (`multi_tenant.enabled`,
  default `false`), multi-user identity with per-`jti` audit attribution, per-note usage
  salience, reversible delete (archive + retention GC + operator restore), `build_sha` in
  `--version` for deploy-time verification. Two MCP tools introduced:
  `create_feature_card`, `job_status`.
- **v0.7.6** — Memory intelligence layer: `vault_context` became a full BM25 + semantic + RRF
  + composite-score pipeline; proactive recall (server-initiated surfacing +
  `POST /api/v1/proactive_recall`); temporal search (`from_ms`/`to_ms`, `occurred_at` decay
  scoring); agent identity injection via MCP `initialize`; curated metrics timeseries with
  Studio charts; deterministic distill quality gate.

See [CHANGELOG.md](CHANGELOG.md) `[2.1.0]`, `[2.0.0]`, `[1.0.0]`, `[0.7.6]` for the full field-by-field
detail, including every breaking change and deprecation.

---

## Documentation

### Project overview and design

| Document | Purpose |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Technical design — planes, ACL hierarchy, source-of-truth model, search pipeline, concurrency, storage layout. |
| [DEPENDENCIES.md](DEPENDENCIES.md) | Workspace dependency tree, level invariants, version pinning policy. |
| [docs/guides/E-ports-and-config.md](docs/guides/E-ports-and-config.md) | Port matrix (`19090 + offset`), override precedence, `server.toml` field reference. |
| [docs/BENCH.md](docs/BENCH.md) | Benchmark results (curator F1w, search relevance). |
| [CHANGELOG.md](CHANGELOG.md) | Version history and notable changes per release. |

### Installation guides

| Guide | Purpose |
|---|---|
| [A — Docker quickstart](docs/guides/A-docker-quickstart.md) | `docker compose` stack — fastest local path. |
| [B — Install from binaries](docs/guides/B-install-binaries.md) | Pre-built Linux x86_64 archives, systemd. |
| [C — crates.io & build from source](docs/guides/C-build-from-source.md) | Library use, or arm64/macOS/Windows. |
| [D — MCP & Studio](docs/guides/D-mcp-and-studio.md) | Connect an MCP client, API keys, Studio login. |
| [E — Ports & configuration](docs/guides/E-ports-and-config.md) | Port matrix, config field reference. |
| [Upgrading 2.0.0 → 2.1.0](docs/UPGRADING-2.0.0-to-2.1.0.md) | Breaking-change migration guide — **read this if your build broke after adopting 2.1.0**. |
| [Upgrading 1.0.0 → 2.0.0](docs/UPGRADING-1.0.0-to-2.0.0.md) | Breaking-change migration guide. |

### Governance and process

| Document | Purpose |
|---|---|
| [GOVERNANCE.md](GOVERNANCE.md) | Decision-making, project-map feature-card tracking, maintainer roles. |
| [RELEASE-POLICY.md](RELEASE-POLICY.md) | Versioning policy, anti-fragility gates, public-release criteria. |
| [MAINTAINERS.md](MAINTAINERS.md) | Current maintainers. |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Contributor guide, PR process. |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | Contributor Covenant 2.1. |
| [CLA.md](CLA.md) | Contributor License Agreement. |
| [SECURITY.md](SECURITY.md) | Vulnerability disclosure process and supported versions. |
| [AGENTS.md](AGENTS.md) | Guidance for AI assistants working on this repository. |

### Deployment (exploitation, once installed)

| Document | Purpose |
|---|---|
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Engine multi-instance deployment — topology, sizing, upgrade ordering, troubleshooting. |
| [packaging/systemd/README.md](packaging/systemd/README.md) | Systemd unit reference — server, worker, engine template, gateway service, smoke tests. |

> **Security note**: the default ACL is fail-closed — with no preset file present, every locus is denied. Configure an ACL preset (`[acl] preset_path`, a TOML of `[[consumer]]` blocks — from a shipped preset or your own) to grant access. See [SECURITY.md](SECURITY.md) for the hardening defaults and their known limitations.

### Platform-specific guides (archived)

| Document | Purpose |
|---|---|
| [docs/WINDOWS-GUIDE.md](docs/WINDOWS-GUIDE.md) | Windows guide — **deferred** (archived, Linux-only as of 2026-06-05). |
| [docs/KNOWN_ISSUES-WINDOWS.md](docs/KNOWN_ISSUES-WINDOWS.md) | Windows known issues — **deferred** (archived, Linux-only as of 2026-06-05). |

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
