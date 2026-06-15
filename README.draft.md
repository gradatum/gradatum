# Gradatum

> **Self-hosted memory backbone for AI agents** — indexable, queryable, graduated.

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Status: Alpha](https://img.shields.io/badge/Status-Alpha-orange.svg)](CHANGELOG.md)
[![Website](https://img.shields.io/badge/Website-gradatum.org-brightgreen)](https://gradatum.org)

**Transform local Markdown folders into a queryable memory server — MCP-ready, fully offline, zero vendor lock-in.**

Gradatum turns any directory of notes into a distributed knowledge backbone for AI agents, coding assistants, and autonomously reasoning systems. Write once in Markdown. Query via HTTP API, MCP, or native CLI. Hybrid search (BM25 + semantic similarity + graph ranking) ranks signals across text and metadata. One Rust binary. No PostgreSQL, no external LLM required.

### Why Gradatum vs. the alternatives

| Property | Why it matters |
|---|---|
| **Embedded + MCP-ready** | One binary. Speaks MCP natively — plug into Claude, Cursor, or your agents directly. No infrastructure. |
| **Markdown is truth** | Notes live as `.md` files with YAML frontmatter. Readable by humans, by any editor, by `cat`. The database is an index, not the source of truth. |
| **Graduated memory** | Inspired by Cicero's *Ars Memoriae* — memories decay, contexts fade, insights concentrate. Semantic forget policies + distillation pipelines built in. |
| **Hybrid signals** | BM25 (lexical) + semantic cosine + PageRank graph + optional cross-encoder rerank, fused via RRF. One query finds what keyword search AND semantic search both miss. |
| **Self-hosted, offline** | Your memory, your machine. No telemetry. No vendor lock-in. Deploy on bare metal, Docker, or LXC. Optional GPU for LLM-backed features. |
| **Multi-vault, hierarchical ACL** | Separate `main`, `staging`, `bench-*` vaults. Bearer-scoped access to memory sections. Preset configs (`flat`, `hierarchical`, `multi-project`) or custom. |

---

## Quickstart — Get running in 5 minutes

> Download the `gradatum-server` archive first (it ships `gradatum-server`, `gradatum-worker`,
> `gradatum-admin`) — see [Installation](#installation) below — then:

### 1. Initialize a data root + mint an API key

```bash
# init generates the config, JWT signing keys, and ACL preset under ./data
gradatum-admin init --root ./data --preset flat

# create an API key WITH write scope (default scope is read-only)
gradatum-admin api-key create --root ./data --owner quickstart \
  --scopes vault_read,vault_search,vault_write
# → prints  ak_<secret>  — copy it
```

### 2. Start the server

```bash
gradatum-server --config ./data/config/server.toml

# in another terminal, verify it's alive:
curl http://127.0.0.1:19090/health
```

### 3. Exchange your API key for a session token (JWT)

Gradatum issues short-lived JWTs from your API key (`/auth/exchange` is the only token issuer):

```bash
export AK=ak_<secret>          # from step 1
JWT=$(curl -s -X POST http://127.0.0.1:19090/auth/exchange \
  -H "Authorization: Bearer $AK" | jq -r .token)
```

### 4. Write your first note

```bash
curl -X POST http://127.0.0.1:19090/api/v1/vault_write \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/json" \
  -d '{
    "title": "Why we chose ULID for stable note identity",
    "body":  "Titles change during editing. ULIDs are immutable and sortable.",
    "section": "decisions",
    "locus": "backend/search"
  }'
# → 202 Accepted with a job_id (writes are async — poll /api/v1/jobs/<job_id>/v2)
```

### 5. Search

```bash
curl -X POST http://127.0.0.1:19090/api/v1/vault_search \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/json" \
  -d '{"query": "ULID identity stable"}'
# → ranked results (title, body, locus, section, relevance score)
```

### 6. Plug into Claude Code / Cursor (MCP)

```bash
# the gradatum-mcp archive ships gradatum-mcp-stub
echo -n "ak_<secret>" > ./data/quickstart.api-key   # the key from step 1
```

```jsonc
// add to your Claude Code / Cursor settings.json:
{
  "mcpServers": {
    "gradatum": {
      "command": "/path/to/gradatum-mcp-stub",
      "args": [],
      "env": {
        "GRADATUM_SERVER_URL": "http://127.0.0.1:19090",
        "GRADATUM_API_KEY_FILE": "/abs/path/to/data/quickstart.api-key"
      }
    }
  }
}
```

Restart your assistant. The `vault_search` / `vault_read` / `vault_write` tools are now available —
the assistant calls them automatically when relevant, or just ask: *"search my gradatum vault for ULID"*.

---

## Concept: Loci and Memory Graduation

Gradatum stores knowledge in **loci** — a name borrowed from Cicero's *Ars Memoriae*, the ancient mnemonic method where memories are placed in mental locations of an imagined palace. In Gradatum:

- **Vault**: the technical backing store (SQLite + FTS5 + Markdown files).
- **Locus**: a logical location within a vault, isolated by ACL (e.g. `backend/search`, `frontend/ui`, `deployments`).
- **Section**: the cognitive category — `decisions`, `architecture`, `debug`, `reasoning`, `feedback`, `lessons-learned`, `retrospectives`, `experiments`, `agent-issues`, `reference`, `council`.
- **Note**: atomic Markdown file with YAML frontmatter — author, created_at, updated_at, wikilinks, tags.

Memory **graduates** through lifecycle policies:

- **decay**: semantic meaning concentrates over time; older notes fade in rank unless refreshed.
- **distillation**: long notes extract compact summaries; summaries promote to new notes.
- **downgrade**: sensitive notes expire to read-only on a schedule.

---

## Architecture & Multi-Host Setup

### Two-layer design

**Memory layer** (stateless, on your app host):
- `gradatum-server` — HTTP façade (REST + MCP)
- `gradatum-worker` — async curator + maintenance jobs
- Vault store (SQLite + Markdown files, OpenDAL multi-backend)
- Hybrid search (BM25 + semantic + graph, fused via RRF)

**Agent layer** (optional, on a GPU host or co-hosted):
- `gradatum-gateway` — unified LLM router (aliases: `curator`, `embed`, `vision`, `reason`)
- `gradatum-engine` — per-model supervisor (llama-server child, restart-bounded, metrics on loopback)

```
        AI agents / coding assistants
              ↓ MCP / HTTP
        ┌──────────────────────────┐
        │  gradatum-server         │  stateless façade
        └────────┬─────────────────┘
                 ↓ async queue
        ┌──────────────────────────┐
        │  gradatum-worker         │  curator + maintenance
        └────────┬─────────────────┘
                 ↓
        ┌──────────────────────────┐
        │  vault (Markdown+SQLite) │  hierarchical loci + sections
        └──────────────────────────┘
                 ↕ LLM calls
        ┌──────────────────────────┐
        │  gradatum-gateway        │  router + circuit-breaker
        └────────┬─────────────────┘
                 ↓ (optional)
        ┌──────────────────────────┐
        │  gradatum-engine         │  per-model supervisor
        │  └── llama-server child  │
        └──────────────────────────┘
```

Typical multi-host: one application host (gradatum-server + gateway), one GPU host (gradatum-engine instances per model). See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for full setup (systemd, config, security, gateway wiring).

---

## Installation

Gradatum is in **alpha (v0.4.x)**. Three paths depending on your use case.

> ⚠️ **APIs not stable before v1.0.0.** Review [CHANGELOG.md](CHANGELOG.md) before upgrading.

### Option A — Pre-built binaries (recommended for production)

**Platform: Linux x86_64.** Each [GitHub Release](https://github.com/gradatum/gradatum/releases) ships three archives:

| Archive | Binaries | Use case |
|---|---|---|
| `gradatum-server-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-server`, `gradatum-worker`, `gradatum-admin` | Vault backbone |
| `gradatum-llm-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-gateway`, `gradatum-engine` | LLM routing (optional, GPU host) |
| `gradatum-mcp-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-mcp-stub` | MCP bridge for Claude / Cursor |

Each archive includes SHA256SUMS and SLSA provenance attestations.

**Example:**

```bash
VERSION=v0.4.3
ARCH=x86_64-unknown-linux-gnu

# Download, verify, extract
curl -fLO "https://github.com/gradatum/gradatum/releases/download/${VERSION}/gradatum-server-${VERSION}-${ARCH}.tar.gz"
curl -fLO "https://github.com/gradatum/gradatum/releases/download/${VERSION}/SHA256SUMS"
sha256sum -c SHA256SUMS --ignore-missing

# (Optional) verify SLSA provenance
gh attestation verify "gradatum-server-${VERSION}-${ARCH}.tar.gz" --repo gradatum/gradatum

# Install
tar -xzf "gradatum-server-${VERSION}-${ARCH}.tar.gz"
sudo install -m 755 "gradatum-server-${VERSION}-${ARCH}/gradatum-server" /usr/local/bin/
sudo install -m 755 "gradatum-server-${VERSION}-${ARCH}/gradatum-worker" /usr/local/bin/
sudo install -m 755 "gradatum-server-${VERSION}-${ARCH}/gradatum-admin"  /usr/local/bin/
```

See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for systemd units, TOML config, security, gateway wiring.

### Option B — crates.io (Rust library integration)

```toml
[dependencies]
gradatum = "0.4"        # curated facade with Cargo features
```

Or: `cargo add gradatum`

Pin a minor version and review [CHANGELOG.md](CHANGELOG.md) before upgrading (alpha APIs).

### Option C — Build from source

**Prerequisites:** Rust 1.88+, C linker (`gcc`/`clang`), SQLite 3.x dev headers.

```bash
git clone https://github.com/gradatum/gradatum.git
cd gradatum

# Build workspace (includes all binaries)
cargo build --workspace --release

# Run tests (optional)
cargo test --workspace

# Binaries → target/release/
```

Note: `gradatum-engine` requires `--features serve` when built individually; `--workspace` enables it automatically.

**Platform support:** arm64, macOS, Windows — build from source (pre-built binaries are Linux x86_64 only).

---

## API Overview

All write operations are **async** (202 Accepted, poll via `/jobs/{id}/v2`). Reads are synchronous.

### Read API (synchronous, 200 OK)

| Method | Path | Use |
|---|---|---|
| GET  | `/vault_status` | Server health + uptime |
| POST | `/vault_search` | Hybrid search (BM25 + semantic + graph RRF) |
| POST | `/vault_read` | Fetch note by ID |
| POST | `/vault_list` | Browse vault (cursor pagination) |
| POST | `/vault_graph` | Graph edges (wikilinks, citations) |
| POST | `/vault_links` | Backlinks (what references this note) |
| POST | `/vault_trace` | Lineage (ancestor path from note) |
| POST | `/vault_context` | Semantic context window (k-nearest neighbors) |

### Write API (async, 202 Accepted)

| Method | Path | Use |
|---|---|---|
| POST | `/vault_write` | Create or update note |
| POST | `/vault_classify` | Curator classification job |
| POST | `/vault_forget` | Semantic forget (decay, dry-run) |
| POST | `/vault_history` | Note history (versions + timestamps) |
| POST | `/vault_restore` | Restore from history |

All write operations return `job_id` (ULID). Poll `/jobs/{job_id}/v2` to check status and retrieve results.

### MCP Surface (Claude Code / Claude Desktop / Cursor)

`gradatum-mcp-stub` exposes:

```typescript
gradatum: {
  vault_search(query, locus?, limit?)   → Promise<SearchResult[]>
  vault_read(note_id)                   → Promise<Note>
  vault_write(title, body, section)     → Promise<{note_id}>
}
```

Auth is automatic (api-key → token exchange → JWT with TTL auto-refresh).

See [ARCHITECTURE.md](ARCHITECTURE.md) for endpoint contracts and request/response schemas.

---

## CLI Surface (planned)

The following CLI commands are planned for v0.5.0+ but **not yet available in v0.4.3**:

```bash
# planned: Write a note
gradatum write --locus=projecta/backend --section=decisions \
  "Use ULID for stable note identity" \
  "Why: titles change, ULID doesn't."

# planned: Search
gradatum search "ULID identity" --locus="projecta/*"

# planned: List vaults
gradatum-admin vault list

# planned: Export
gradatum export --vault=main --format=json
```

Current release (v0.4.3) uses HTTP API. CLI is in [github.com/gradatum/gradatum/issues](https://github.com/gradatum/gradatum) — please upvote or comment if CLI is blocking your workflow.

---

## Deployment

See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for:
- Systemd unit files (server, worker, gateway, engines)
- TOML configuration (ports, storage backends, ACL presets, LLM routing)
- Security hardening (Bearer tokens, JWT, ACL policies, secret rotation)
- Multi-host wiring (app host + GPU host)
- Monitoring (Prometheus metrics on loopback)

---

## Current Status (v0.4.3)

**Grade:** Bronze (alpha) — APIs not stable before v1.0.0.

| Metric | Value |
|---|---|
| **Version** | v0.4.3 (public OSS, Apache-2.0) |
| **Test suite** | 1407 tests PASS / 0 clippy warnings |
| **Crates** | 28 workspace crates (26 published to crates.io), MSRV 1.88 |
| **Search** | RRF k=60: BM25 + semantic cosine + PageRank + optional ONNX cross-encoder |
| **Storage** | SQLite + Markdown (OpenDAL multi-backend: S3, GCS, Azure via feature flags) |
| **Job queue** | Apalis (SQLite-backed) with dead-letter queue |
| **MCP** | `gradatum-mcp-stub` (stdio transport, http planned) |

### What's shipped (v0.1.0 → v0.4.3)

See [CHANGELOG.md](CHANGELOG.md) for full history. Highlights:

| Milestone | Includes |
|---|---|
| **v0.1.0** | Functional core: vault registry, curator, hybrid search, bearer auth, hierarchical ACL, queue worker, embeddings |
| **v0.2.0** | Search hardening, dedup, dependency upgrades |
| **v0.3.0** | Storage trait decomposition, event log, secrets DI, persisted JWT keys |
| **v0.3.6** | Public OSS release: GitHub + crates.io (26 crates), Apache-2.0 license |
| **v0.4.0** | Vault durable writes: note history, optimistic locking, write provenance |
| **v0.4.1** | Zero-panic public API, revocation, docs.rs coverage |
| **v0.4.2** | Note ID in write response, DTO unification, gateway metrics cardinality |
| **v0.4.3** | Vault lifecycle: semantic forget, 6-state lifecycle machine, temporal search, multimodal gateway |

### Roadmap

Gradatum follows a **vault-first** trajectory through v0.5 (Silver grade), then expands to multi-tenant + agent layers + serving.

| Version | Theme | Scope |
|---|---|---|
| **v0.4.x** | Vault Core | Durable writes, lifecycle, distillation, storage backends (current) |
| **v0.5.0** | **Silver** | Vault fully queryable via MCP — core memory surface stable for agents |
| **v0.5.1** | Multi-tenant + OAuth | Multi-vault ACL, OIDC, production auth |
| **v0.6.0** | Context + Agent layer | Agentic context synthesis, local execution |
| **v0.7.0** | Serving | Web UI, dashboards, analytics |
| **v1.0.0** | **Gold** | Production baseline — stable APIs, LTS support |
| **v2.0.0** | **Platinum** | Multimodal + bring-your-own-compute (BYOC) |

Near-term items: `sqlite-vec` ANN index (replacing brute-force cosine), native MCP server + Streamable HTTP transport.

---

## Documentation

### Project overview

| Document | Purpose |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Technical design — layers, ACL hierarchy, search pipeline, concurrency, storage layout |
| [DEPENDENCIES.md](DEPENDENCIES.md) | Workspace dependency tree and version policy |
| [PORTS.md](PORTS.md) | Port matrix and override conventions |
| [docs/BENCH.md](docs/BENCH.md) | Benchmark results (curator F1, search relevance) |
| [CHANGELOG.md](CHANGELOG.md) | Full version history and breaking changes |

### Deployment & operations

| Document | Purpose |
|---|---|
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Systemd units, config, security, multi-host setup, monitoring |

### Governance & contributing

| Document | Purpose |
|---|---|
| [GOVERNANCE.md](GOVERNANCE.md) | Decision-making, RFC process, roles |
| [RELEASE-POLICY.md](RELEASE-POLICY.md) | Versioning, anti-fragility gates, public-release criteria |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Contributor guide, PR process, CI/CD |
| [SECURITY.md](SECURITY.md) | Vulnerability disclosure, supported versions |
| [MAINTAINERS.md](MAINTAINERS.md) | Current maintainers |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | Contributor Covenant 2.1 |
| [CLA.md](CLA.md) | Contributor License Agreement |
| [AGENTS.md](AGENTS.md) | Guidance for AI assistants working on this repository |

> **Security note:** The default ACL in v0.x is permissive single-tenant (allow-all). Configure a stricter AclPolicy before exposing gradatum on a network.

---

## Vocabulary

| Term | Meaning |
|---|---|
| **Vault** | Technical backing store (SQLite + FTS5 + Markdown files). Multi-vault first-class. |
| **Locus** | Logical subdivision of a vault, isolated by ACL. From Cicero's *ars memoriae*. |
| **Section** | Cognitive category: `decisions`, `architecture`, `debug`, `reasoning`, `feedback`, `lessons-learned`, `retrospectives`, `experiments`, `agent-issues`, `reference`, `council` |
| **Note** | Atomic Markdown file with YAML frontmatter (title, body, author, tags, wikilinks) |
| **Bearer / Consumer** | Authenticated identity with read/write ACL patterns |
| **Preset** | Template ACL configuration shipped in `crates/gradatum-admin/presets/` |
| **Job** | Async write operation (create note, classify, forget, restore). Returns ULID. Poll `/jobs/{id}/v2` for status. |

---

## Contributing

Gradatum is built openly. Contributor guidelines, issue tracking, and PR process are documented in [CONTRIBUTING.md](CONTRIBUTING.md) and [GOVERNANCE.md](GOVERNANCE.md).

To report a vulnerability, see [SECURITY.md](SECURITY.md).

---

## License

[Apache-2.0](LICENSE)

---

## Tags (SEO / discovery)

`RAG`, `MCP`, `Model Context Protocol`, `autonomous agents`, `vector database alternative`, `local AI`, `Rust`, `self-hosted`, `AI memory`, `semantic search`, `hybrid search`, `knowledge base`, `offline-first`, `no-vendor-lock-in`
