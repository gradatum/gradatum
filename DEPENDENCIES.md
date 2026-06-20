# Gradatum — Dépendances

> Cargo dependency tree. Generated semi-manually — to be regenerated via `cargo tree --workspace --depth 1` once the workspace compiles.
> Updated: 2026-06-11 — 0.4.6 bump; new workspace members: gradatum-studio (publish=false), index-parity-tests (test-only); tower-http +fs,set-header; async-trait (dev). Dependency graph otherwise unchanged.
> Updated: 2026-06-11 — 0.4.6 bump; new workspace members: gradatum-studio (publish=false), index-parity-tests (test-only); tower-http +fs,set-header; async-trait (dev). Dependency graph otherwise unchanged.
> Updated: 2026-06-11 — 0.4.6 bump; new workspace members: gradatum-studio (publish=false), index-parity-tests (test-only); tower-http +fs,set-header; async-trait (dev). Dependency graph otherwise unchanged.
> Updated: 2026-06-11 — 0.4.6 bump; new workspace members: gradatum-studio (publish=false), index-parity-tests (test-only); tower-http +fs,set-header; async-trait (dev). Dependency graph otherwise unchanged.

---

## Workspace structure

**3 binaires (control plane)** :

```
gradatum-server         (stateless HTTP/MCP façade)
├── gradatum-core
├── gradatum-vault
├── gradatum-storage
├── gradatum-index
├── gradatum-search
├── gradatum-queue
├── gradatum-cache
├── gradatum-auth
├── gradatum-acl-auth
├── axum
├── tower-http
├── rmcp
└── tokio + tracing

gradatum-worker         (async queue consumer)
├── gradatum-core
├── gradatum-vault
├── gradatum-storage
├── gradatum-index
├── gradatum-queue
├── gradatum-chat
├── gradatum-curator
├── gradatum-embed
├── gradatum-acl-auth
└── tokio + tracing

gradatum-admin          (CLI ops)
├── gradatum-core
├── gradatum-vault
├── gradatum-storage
├── gradatum-index
├── gradatum-acl-policy
├── clap
└── argon2 + rand
```

**22 crates** (3 binaires control plane + 15 lib data plane + 3 clients + 1 umbrella):

```
gradatum-core           (primitives partagées)
├── thiserror
├── chrono
├── serde + serde_json
└── ulid

gradatum-markdown       (parse/serialize MD + frontmatter + wikilinks)
├── gradatum-core
├── serde_yaml
└── regex

gradatum-vault          (multi-vault registry + lifecycle + swap)
├── gradatum-core
├── gradatum-storage
├── gradatum-index
└── tokio

gradatum-storage        (FS abstraction + loci paths)
├── gradatum-core
├── gradatum-markdown
├── walkdir
├── tokio + tracing
└── (cfg(unix)) nix      (statfs NFS detect — RFC-0002 R1)

gradatum-index          (SQLite + FTS5 + migrations idempotentes + drift Phase A)
├── gradatum-core
├── rusqlite 0.32 (bundled — FTS5 activé nativement, 4 PRAGMA C12)
├── async-trait
├── serde + serde_json   (notes + audit_trail + extra_json)
├── sha2                 (drift Phase A — hash prefix 4KB + hash complet)
├── chrono               (timestamps ISO 8601)
├── ulid                 (IDs override + audit_trail)
├── thiserror            (GradatumError typed)
└── tracing
(Phase 3+: tantivy, sqlite-vec ANN — non inclus Phase 1)

gradatum-search         (multi-mode reader + RRF fusion)
├── gradatum-core
├── gradatum-index
├── gradatum-cache
└── tracing

gradatum-queue          (SQLite-backed jobs + lease atomic)
├── gradatum-core
├── rusqlite
└── tokio

gradatum-cache          (moka LRU in-process)
├── gradatum-core
└── moka

gradatum-chat           (trait Chat + OpenAICompat + Heuristic + Noop)
├── gradatum-core
├── async-trait
├── reqwest (rustls-tls default)
├── serde_json
├── tracing
└── (feature windows-native-tls) native-tls — OFF default (RFC-0002 §4.6)

gradatum-curator        (note curation: filtering, routing, tagging, wikilinks)
├── gradatum-core
├── gradatum-vault
├── gradatum-queue
├── gradatum-chat
├── gradatum-embed
├── gradatum-markdown
└── tokio

gradatum-embed          (Embedder trait + remote/local impl)
├── gradatum-core
├── reqwest (rustls-tls default)
├── (feature windows-native-tls) native-tls — OFF default (RFC-0002 §4.6)
└── (feature fastembed-cpu) fastembed + ort — OFF default (bug ort-sys via private registry)

gradatum-engine         (supervisor for llama-server subprocesses)
├── gradatum-core
├── gradatum-dto (QaEvent for event-log)
├── tokio + tracing (async subprocess management)
├── nix (POSIX process group signaling)
└── (feature serve) axum + reqwest + prometheus-client

gradatum-acl-policy     (ACL preset + config model loading)
├── gradatum-core
├── toml
└── serde + serde_yaml

gradatum-acl-auth       (glob pattern matching + bearer token verify)
├── gradatum-core
├── globset
├── argon2
└── rand

gradatum-auth           (JWT/OIDC/API-key auth + token validation)
├── gradatum-core
├── jsonwebtoken
├── serde_json
└── chrono

```

**3 clients** :

```
gradatum-cli            (CLI utilisateur)
├── gradatum-core
├── reqwest
└── clap

gradatum-mcp-stub       (adapter MCP stdio → HTTP)
├── gradatum-core
├── rmcp (stdio mode)
└── reqwest

gradatum-sdk-rs         (SDK Rust pour intégration)
├── gradatum-core
└── reqwest
```

---

## Dépendances externes principales

| Crate | Version | Usage | Justification |
|---|---|---|---|
| `tokio` | 1.x | Async runtime | Standard Rust async |
| `axum` | 0.7 | HTTP server | Léger, performant, ergonomique |
| `tower` / `tower-http` | 0.5 | Middleware | Compose avec axum |
| `rmcp` | =0.17 | MCP server/client | Lib officielle Rust MCP, pinnée |
| `rusqlite` | 0.32 (bundled) | SQLite + FTS5 | Embedded, multi-feature |
| `tantivy` | (à fixer Phase 3) | Full-text Lucene-quality | Rust pur, embedded |
| `globset` | 0.4 | ACL pattern matching | Lib pure, simple |
| `argon2` | 0.5 | Password hashing OWASP | Standard recommandation |
| `moka` | 0.12 | LRU cache | Performant, thread-safe |
| `reqwest` | 0.12 (rustls) | HTTP client | Standard, no OpenSSL |
| `serde` + `serde_json` + `serde_yaml` + `toml` | latest | Serialization | Markdown frontmatter, JSON-RPC, configs |
| `tracing` + `tracing-subscriber` | latest | Logs structurés | JSON output ready for SIEM ingestion |
| `clap` | 4 | CLI parsing | Standard derive |
| `ulid` | 1.x | Stable note IDs | Lexicographically sortable, time-ordered |
| `chrono` | 0.4 | Dates ISO 8601 | Standard |
| `walkdir` | 2 | FS scan | Pour reindex / migrate |
| `regex` | 1 | Wikilinks parsing | Standard |
| `thiserror` + `anyhow` | latest | Error handling | Standard Rust |
| `tempfile` | 3 (dev) | Tests | Standard |
| `apalis` | `=1.0.0-rc.9` | Job queue framework (v0.2.0 F-15 Monitor multi-worker + Layers Timeout/Retry/CatchPanic/LoadShed) | Type-safe Rust job framework, embedded crate compile-time (pas service runtime — ARCH-D15 F-24). Pin exact D-09 + caveat C1 RC9→v1.0 stable Q3 2026 watch |
| `apalis-sql` | `=1.0.0-rc.9` | Apalis backend traits | Cohérent pin Apalis core. Schema SQLite via SqliteQueueStore custom (pas apalis-sqlite tables — F-24 agnostique) |
| `apalis-cron` | `=1.0.0-rc.8` | Schedules périodiques (cleanup_dlq_daily + cleanup_idempotency) | NB rc.9 non publié pour apalis-cron sur crates.io. Caveat C1 double RC watch |
| `prometheus` | `=0.13.4` | Metrics exporter (v0.2.0 F-15 :19091 + F-16 /metrics) | Pin exact patch level (0.14 breaking change connu) |
| `sqlx` | (workspace) | SqliteQueueStore + GradatumQueue + gradatum-db-sqlite impl QueueStore (v0.2.0 F-14 partiel) | WAL mode + atomic UPDATE...RETURNING leases |

### Crate workspace `gradatum-db-sqlite` (v0.2.0 NEW)

Crate dédié `SqliteQueueStore` impl `QueueStore` trait depuis `gradatum-core` (15 méthodes : enqueue/dequeue/get/complete/fail/cancel/fail_dlq/find_awaiting/set_pending/recover_stale_leases/cancel_expired_deadlines/promote_retries/schedule_retry/list/subscribe). Schema custom `gradatum_jobs` (id TEXT ULID + payload JSON + status + priority + class + timestamps + lease_until + attempt_count + deadline + last_error + await_jobs + kind dénormalisé). Migrations 006_apalis_bootstrap + 007_jobs_kind_indexed + 008_idempotency + 009_jobs_v2_drain.

Pattern F-24 agnostique : QueueStore trait dans gradatum-core, impl SqliteQueueStore Bronze v0.2.0, futur Postgres/libsql/LanceDB sans breaking Apalis worker layer.

---

## Dépendances optionnelles (feature flags)

| Feature | Activates | Crate | Phase |
|---|---|---|---|
| `local-encoder` (default) | Local CPU embedding fallback | `fastembed` | Phase 1 |
| `reranker` | Cross-encoder rerank | `fastembed` (Jina model) | Phase 3 |
| `tantivy-index` | Full-text index v2 | `tantivy` | Phase 3 |
| `sqlite-vec` (default) | Vector ANN | `sqlite-vec` C extension | Phase 3 |
| `prometheus` | `/metrics` endpoint | `prometheus` | Phase 3 |
| `tokio-console` | Async runtime debugging | `console-subscriber` | Phase 4 (dev only) |

---

## Externes (services réseau, pas crates)

| Service | Usage | Dépendance core ? |
|---|---|---|
| OpenAI-compatible gateway | Gatekeeper LLM + embeddings | NON (R1 single-source-of-LLM-auth — pluggable, pas hardcodé) |
| Litestream | Backup continu DB | NON (operator-defined deployment) |
| SIEM / audit log sink | Audit log ingestion | NON (operator-defined deployment) |
| Notification system | Ops alerts | NON (operator-defined deployment) |

→ **Aucun service externe requis** dans le code core. Tout est pluggable ou optionnel.

---

## Mise à jour

Régénérer après `cargo build --workspace` :

```bash
cd /path/to/gradatum
cargo tree --workspace --depth 1 > /tmp/cargo-tree.txt
# Comparer avec ce fichier, mettre à jour si divergence.
```

Skill associé : `dependency-architecture-tree` peut être invoqué pour automatiser la mise à jour.

---

*Document maintenu par l'Architect (Général). Dernière mise à jour : 2026-05-01 — initial scaffold.*
