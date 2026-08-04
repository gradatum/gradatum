# Deployment Guide

> Platform: **Linux only** (x86_64 and aarch64). gradatum does not support Windows or macOS.

This guide covers:
- [§0 — Obtaining binaries](#0-obtaining-binaries): which archive to use for each role.
- [§1–§11 — Engine deployment](#1-how-the-supervisor-works): deploying `gradatum-engine` in single-host or multi-instance mode, wired into `gradatum-gateway`.
- [§12 — App-host upgrade order](#12-app-host-upgrade-order-gradatum-server-before-gradatum-worker): why `gradatum-server` must be upgraded before `gradatum-worker`, and how to prove which commit is live.
- [§13 — Troubleshooting](#13-troubleshooting): engine startup and runtime symptoms.

---

## 0. Obtaining binaries

### 0.1 Pre-built archives (Linux x86_64, recommended)

Each [GitHub Release](https://github.com/gradatum/gradatum/releases) ships three archives plus a `SHA256SUMS` file and SLSA provenance attestations.

| Archive | Binaries inside | Deploy on |
|---|---|---|
| `gradatum-server-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-server`, `gradatum-worker`, `gradatum-admin` | **app-host** — vault backbone |
| `gradatum-llm-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-gateway`, `gradatum-engine` | **gpu-host** (engines) + **app-host** (gateway) |
| `gradatum-mcp-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-mcp-stub` | **app-host** — MCP bridge |

All three archives are covered by a single `SHA256SUMS` file. Each archive ships with an individual SLSA provenance attestation verifiable via the GitHub CLI.

**Download and verify (example for the server archive):**

```bash
VERSION=v1.0.0
ARCH=x86_64-unknown-linux-gnu

curl -fLO "https://github.com/gradatum/gradatum/releases/download/${VERSION}/gradatum-server-${VERSION}-${ARCH}.tar.gz"
curl -fLO "https://github.com/gradatum/gradatum/releases/download/${VERSION}/SHA256SUMS"

# Integrity
sha256sum -c SHA256SUMS --ignore-missing

# Provenance (requires gh CLI, v2.49+)
gh attestation verify "gradatum-server-${VERSION}-${ARCH}.tar.gz" \
  --repo gradatum/gradatum

# Extract
tar -xzf "gradatum-server-${VERSION}-${ARCH}.tar.gz"
```

Repeat the same steps for `gradatum-llm` and `gradatum-mcp` as needed. Install the extracted binaries to `/usr/local/bin/` (adjust `ExecStart` in your systemd units accordingly).

### 0.2 Build from source

**Prerequisites:** Rust stable (MSRV 1.91), `gcc` or `clang`, `libsqlite3-dev`.

```bash
git clone https://github.com/gradatum/gradatum.git
cd gradatum
cargo build --workspace --release
```

Binaries land in `target/release/`. `gradatum-engine` requires the `serve` feature when building individually; `--workspace` enables it automatically.

arm64 binaries are not shipped pre-built — build from source on aarch64.

---

## 1. How the supervisor works

`gradatum-engine` is a Rust binary that acts as a **process supervisor** for a
`llama-server` child. The design is intentionally simple:

```
gradatum-engine  (supervisor, port N)
    └── llama-server  (child, port N+1, loopback-only)
```

- The supervisor listens on `port` (configurable — can be loopback or a specific LAN IP).
- The child listens on `child_port` bound to `127.0.0.1` (loopback only — never exposed
  directly).
- Proxy: every `/v1/*` request received by the supervisor is forwarded to the child via
  `reqwest`. The supervisor adds no authentication layer of its own — ACL is handled by
  `gradatum-gateway` upstream.
- Concurrency: handled by `--parallel N` passed to `llama-server`. No mutex on the
  supervisor side. Multiple concurrent requests are handled by the child natively.
- Restart: the supervisor restarts a crashed child up to `child_restart_max` times total
  (budget, not a rate-limit). After exhaustion the engine marks itself as `unhealthy`,
  which the gateway detects and routes around.

### Lifecycle

```
start → spawn child → poll /health (child_port) → Ready
                           ↓ (child crashes)
                       restart + backoff → poll /health
                           ↓ (budget exhausted)
                       unhealthy → gateway triggers fallback
```

Shutdown sequence (SIGTERM to the supervisor unit):

```
SIGTERM → supervisor sets shutdown_requested → SIGTERM to child process group
       → wait up to 5s → SIGKILL if not exited → reap → exit
```

---

## 2. Example topology — multi-host GPU serving with gateway routing

A production-style layout: one GPU host serves several models (one engine
instance per model), and an application host runs the gradatum services plus a
routing gateway that prefers the GPU host and falls back to a local CPU path.
All addresses below are illustrative.

```
                 consumers (apps, agents, MCP clients)
                                  │
                                  ▼
 ┌───────────────────────── app-host (Linux, 10.0.0.10) ─────────────────────────┐
 │                                                                                │
 │  gradatum-server ──┐                       ┌─────────────────────────────┐     │
 │  gradatum-worker ──┴─────────────────────▶ │ gradatum-gateway  :8436     │     │
 │                                            │ (router + circuit-breaker)  │     │
 │                                            └───┬─────────────────┬───────┘     │
 │                                       primary  │                 │ fallback    │
 │                                                │                 ▼             │
 │                                                │     ┌────────────────────┐    │
 │                                                │     │ local CPU fallback │    │
 │                                                │     │ engine + legacy    │    │
 │                                                │     │ embedder           │    │
 │                                                │     └────────────────────┘    │
 └────────────────────────────────────────────────┼───────────────────────────────┘
                                                   │ LAN
 ┌───────────────────────── gpu-host (Linux, 10.0.0.20) ─────────────────────────┐
 │  gradatum-engine — one supervisor binary, one instance per model:             │
 │                                                                               │
 │   ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌──────────┐              │
 │   │ chat    │ │ embed   │ │ small   │ │ reason  │ │ vision   │              │
 │   │ :8083   │ │ :8432   │ │ :8082   │ │ :8081   │ │ :8080 +mm│              │
 │   └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘ └────┬─────┘              │
 │        │ each instance supervises one llama-server child (loopback child_port)│
 │        └────────── bind: gpu-host LAN IP · /metrics on loopback ─────────────┘
 │   GGUF models bind-mounted read-only under /opt/gradatum/models/               │
 └───────────────────────────────────────────────────────────────────────────────┘
```

**How it maps to config:**
- Each model on `gpu-host` is one `conf.d/70-engine-<name>.toml` (see §4): `bind_addr` =
  the gpu-host LAN IP (never `0.0.0.0`), a unique `port`, a loopback `child_port`, and a
  loopback `metrics_port`. Vision adds `mmproj_path`.
- `gradatum-gateway` (`gateway.toml`) defines one alias per role. Each alias has a
  `provider` (primary → an engine instance on `gpu-host`) and a `fallback_provider`
  (→ a local CPU engine / legacy embedder on `app-host`). The circuit-breaker opens after
  repeated primary failures and returns after the cooldown.
- `gradatum-server` and `gradatum-worker` point their chat and embedding endpoints at the
  gateway (`http://localhost:8436`), so all routing/failover is centralized.

**Sizing note:** put the largest model (e.g. a vision model with a long context) last when
bringing instances up, and keep enough headroom on the GPU — bring one instance up at a
time and verify VRAM is released before starting the next when migrating.

---

## 3. Prerequisites

| Requirement | Detail |
|---|---|
| **OS** | Linux (x86_64 or aarch64). Kernel 5.4+ recommended. |
| **llama-server** | Pre-built binary placed in `/usr/local/bin/llama-server` or `/opt/gradatum/bin/llama-server`. Build from [ggerganov/llama.cpp](https://github.com/ggerganov/llama.cpp). |
| **Models** | GGUF files placed under `/opt/gradatum/models/`. The engine validates this prefix on startup; any path outside it is rejected. |
| **gradatum-engine** | Binary at `/usr/local/bin/gradatum-engine` (or adjust `ExecStart` in the systemd unit). |
| **GPU (optional)** | CUDA (NVIDIA) or ROCm/Vulkan (AMD) runtime installed on the host. GPU layers are activated via `gpu_layers` in the config. |

---

## 4. Configuration reference

Each instance is configured via a TOML file, typically placed at
`/etc/gradatum/conf.d/70-engine-<name>.toml`. The file uses a single `[engine]` section.

### Full field reference

```toml
[engine]
# ── Required ──────────────────────────────────────────────────────────────────
# Path to the GGUF model file. Must be under /opt/gradatum/models/.
model_path = "/opt/gradatum/models/<model-name>.gguf"

# Model type: "chat" (text generation) or "embed" (embeddings).
model_kind = "chat"

# Port the supervisor listens on (your API endpoint).
port = 11435

# ── Child process ─────────────────────────────────────────────────────────────
# Port the llama-server child listens on (loopback only, not exposed).
# Must be > 1024 and different from `port`. Default: 11436.
child_port = 11436

# Number of parallel inference slots passed as --parallel to llama-server.
# Controls concurrent request handling. Default: 4.
parallel = 4

# Path to the llama-server binary. Must be under /usr/local/bin/ or /opt/gradatum/bin/.
# Default: /usr/local/bin/llama-server
llama_server_bin = "/usr/local/bin/llama-server"

# ── Inference parameters ──────────────────────────────────────────────────────
# Number of model layers to offload to GPU. 0 = CPU only.
# Set to a large number (e.g. 99) to offload all layers.
gpu_layers = 0

# Number of CPU threads for inference. Default: 8.
n_threads = 8

# KV cache context length in tokens. Default: 32768.
context_len = 32768

# Maximum tokens generated per chat request. Default: 512.
max_tokens = 512

# ── Startup and resilience ────────────────────────────────────────────────────
# Time in seconds to wait for the child /health to return 200. Default: 60.
startup_timeout_secs = 60

# Total restart budget (not a rate-limit per window). Decremented on each crash.
# Reset to max only if the child was stable for >= min_stable_uptime_secs before crash.
# After exhaustion, engine marks itself unhealthy. Default: 3.
child_restart_max = 3

# Minimum uptime in seconds for a crash to be considered "stable" (not flapping).
# Below this threshold, the restart budget is consumed without reset. Default: 30.
min_stable_uptime_secs = 30

# ── Network ───────────────────────────────────────────────────────────────────
# Bind address for the supervisor's listening port.
# Omit or set to 127.0.0.1 for loopback-only (default).
# Set to a specific LAN unicast IP to expose on the network.
# 0.0.0.0 and :: are REJECTED (fail-closed).
bind_addr = "127.0.0.1"

# Optional explicit port for the /metrics endpoint (Prometheus).
# Always bound on 127.0.0.1 regardless of bind_addr.
# Default: port + 1.
# metrics_port = 11436

# Request timeout in seconds. Exceeded => 504 to the caller. Default: 120.
timeout_secs = 120

# Maximum request body size in bytes. Default: 32 MiB (covers vision base64 images).
# Hard cap: 256 MiB.
body_limit_bytes = 33554432

# ── gradatum server ──────────────────────────────────────────────────────────
# URL of the gradatum server for event logging and JWT exchange.
# Must be loopback. Default: http://127.0.0.1:19090
gradatum_url = "http://127.0.0.1:19090"

# ── Vision (optional) ────────────────────────────────────────────────────────
# Path to the multimodal projector GGUF file (vision models only).
# Must be under /opt/gradatum/models/. Set only for vision-capable models.
# mmproj_path = "/opt/gradatum/models/<mmproj-name>.gguf"

# ── Extra flags (allow-list) ─────────────────────────────────────────────────
# Additional flags passed verbatim to llama-server, after all authoritative flags.
# Only flags in the allow-list are accepted; anything else causes a startup error.
# See Section 6 for the complete allow-list.
extra_args = []
```

Environment variable overrides are supported with the prefix `GRADATUM_ENGINE_`:

```bash
GRADATUM_ENGINE_GPU_LAYERS=99 GRADATUM_ENGINE_N_THREADS=16 gradatum-engine --config /etc/gradatum/conf.d/70-engine-chat.toml
```

---

## 5. Config examples

### 4.1 Chat instance (CPU)

```toml
[engine]
model_path    = "/opt/gradatum/models/<chat-model-name>.gguf"
model_kind    = "chat"
port          = 11435
child_port    = 11436
parallel      = 4
gpu_layers    = 0
n_threads     = 8
context_len   = 32768
max_tokens    = 512
bind_addr     = "127.0.0.1"
```

### 4.2 Embed instance (CPU)

```toml
[engine]
model_path    = "/opt/gradatum/models/<embed-model-name>.gguf"
model_kind    = "embed"
port          = 11437
child_port    = 11438
parallel      = 8
gpu_layers    = 0
n_threads     = 8
context_len   = 8192
bind_addr     = "127.0.0.1"
```

`gradatum-engine` adds `--embedding` automatically when `model_kind = "embed"`.

### 4.3 Chat instance (GPU)

```toml
[engine]
model_path    = "/opt/gradatum/models/<chat-model-name>.gguf"
model_kind    = "chat"
port          = 11435
child_port    = 11436
parallel      = 4
gpu_layers    = 99      # offload all layers; llama-server clamps to actual layer count
n_threads     = 4
context_len   = 32768
bind_addr     = "127.0.0.1"
extra_args    = ["--flash-attn"]
```

### 4.4 Vision instance (GPU)

```toml
[engine]
model_path    = "/opt/gradatum/models/<vision-model-name>.gguf"
model_kind    = "chat"
port          = 11439
child_port    = 11440
parallel      = 2
gpu_layers    = 99
context_len   = 8192
mmproj_path   = "/opt/gradatum/models/<mmproj-name>.gguf"
body_limit_bytes = 67108864    # 64 MiB — images are large
bind_addr     = "127.0.0.1"
```

The multimodal projector is passed to `llama-server` via `--mmproj`. It cannot be
specified in `extra_args` (blocked by the allow-list to prevent path injection).

---

## 6. extra_args allow-list

The allow-list is enforced at startup. Any flag not in this list causes the engine to
refuse to start with a clear error. The intent is to prevent accidental exposure of flags
that control network binding, authentication, model URLs, or arbitrary file paths.

**Allowed flags:**

| Category | Flags |
|---|---|
| Attention | `--flash-attn`, `-fa` |
| Memory | `--no-mmap`, `--mlock`, `--no-kv-offload`, `-nkvo` |
| Batching | `--cont-batching`, `--no-cont-batching`, `--batch-size`/`-b`, `--ubatch-size`/`-ub` |
| HTTP threads | `--threads-http` |
| Context / KV cache | `--keep`, `--defrag-thold`, `--cache-type-k`/`-ctk`, `--cache-type-v`/`-ctv` |
| NUMA | `--numa` |
| Logging | `--log-disable`, `--log-prefix`, `--log-timestamps` |
| RoPE / YaRN | `--rope-scaling`, `--rope-scale`, `--rope-freq-base`, `--rope-freq-scale`, `--yarn-orig-ctx`, `--yarn-ext-factor`, `--yarn-attn-factor`, `--yarn-beta-slow`, `--yarn-beta-fast` |
| Reproducibility | `--seed`/`-s` |
| Performance | `--poll` |
| SWA / cache reuse | `--swa-full`, `--cache-reuse` |
| Unified KV cache | `--kv-unified` |
| Prefix-cache slot routing | `--slot-prompt-similarity` |
| Reasoning | `--reasoning`, `--reasoning-format`, `--reasoning-budget` |
| Sampling | `--temp`/`--temperature`, `--top-k`, `--top-p`, `--min-p`, `--presence-penalty`, `--repeat-penalty`, `--n-predict`/`-n`, `--backend-sampling` |

**Always rejected (security boundary):**

`--host`, `--port`, `--api-key-file`, `--model-url`, `--rpc`, `--ssl-key-file`,
`--ssl-cert-file`, `--path`, `--mmproj`, `--n-gpu-layers`/`-ngl`/`--gpu-layers`,
`-m`/`--model`, and any flag not explicitly listed above.

`--n-gpu-layers` is controlled by the `gpu_layers` config field. Passing it in
`extra_args` would create a duplicate argument warning in `llama-server` and could
silently override the configured value.

---

## 7. Security properties

> **Security note**: the default ACL is **fail-closed**. With no preset file at `[acl] preset_path` (default `/var/lib/gradatum/config/bearer.toml`), the engine falls back to DENY-ALL and every locus is denied — an identity with no matching `[[consumer]]` block is refused. Configure an ACL preset (shipped preset or your own) to grant access. Multi-tenant isolation (`multi_tenant.enabled`) is opt-in and off by default; note that per-key **write scopes** are only enforced when it is on — see `SECURITY.md` for that caveat.

### 6.1 Bind address (fail-closed)

`bind_addr` defaults to `127.0.0.1` (loopback). The engine rejects the following at
startup:

- `0.0.0.0` (IPv4 wildcard)
- `::` (IPv6 wildcard)
- `::ffff:0.0.0.0` (IPv4-mapped wildcard — equivalent to 0.0.0.0 on Linux)
- Broadcast addresses (`255.255.255.255`)
- Multicast addresses

To expose the engine on a LAN, set `bind_addr` to the specific unicast IP of the
relevant network interface. Never use `0.0.0.0`.

### 6.2 /metrics endpoint

`/metrics` (Prometheus) is always bound to `127.0.0.1` regardless of `bind_addr`. It
is not accessible from the network, even when the engine API is exposed on a LAN IP.

### 6.3 Model path guard

`model_path` and `mmproj_path` are validated at startup:

- The path must exist and be accessible (`canonicalize` must succeed).
- The resolved canonical path must start with `/opt/gradatum/models/`.

Any attempt to load a model from outside this prefix (including path traversal like
`/opt/gradatum/models/../../etc/passwd`) is rejected.

### 6.4 Binary allow-list

`llama_server_bin` must canonicalize to a path starting with `/usr/local/bin/` or
`/opt/gradatum/bin/`. Arbitrary binary paths are rejected.

### 6.5 Environment isolation

The child process is spawned with `env_clear()`. Only the following environment
variables are re-injected from the supervisor's environment:

- `PATH`, `HOME`, `LD_LIBRARY_PATH` (by exact name)
- GPU detection variables by prefix: `VK_*`, `MESA_*`, `RADV_*`, `GGML_*`, `HIP_*`,
  `ROCR_*`, `ROCM_*`, `HSA_*`, `CUDA_*`, `NVIDIA_*`

This prevents `llama-server` from reading variables like `LLAMA_ARG_HOST`,
`LLAMA_API_KEY`, `HF_TOKEN`, or `LLAMA_ARG_MODEL_URL` from the supervisor's environment,
which could otherwise silently override positional flags.

### 6.6 Orphan prevention

- The child is placed in its own process group (`process_group(0)`).
- `kill_on_drop(true)` sends SIGKILL to the child if the supervisor is dropped
  unexpectedly (covers panics with unwinding).
- The systemd unit must set `KillMode=control-group` and `Delegate=yes` so that
  systemd cleans the entire cgroup (including any orphaned child from a hard crash)
  before restarting the supervisor.

---

## 8. systemd unit

### Single instance

Save as `/etc/systemd/system/gradatum-engine-chat.service`:

```ini
[Unit]
Description=Gradatum Engine — chat (<model-name>)
After=network.target
Wants=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/gradatum-engine --config /etc/gradatum/conf.d/70-engine-chat.toml
Restart=on-failure
RestartSec=10
StartLimitBurst=5
StartLimitIntervalSec=300

# Process group management — ensures llama-server child is cleaned up on stop/restart.
# KillMode=control-group kills all processes in the cgroup (supervisor + child).
# Delegate=yes keeps the cgroup clean before re-ExecStart (no double GPU load).
KillMode=control-group
Delegate=yes
TimeoutStopSec=30

# For large models, protect the OOM killer from killing the engine before the child.
OOMScoreAdjust=-100

User=gradatum
Group=gradatum
WorkingDirectory=/opt/gradatum

[Install]
WantedBy=multi-user.target
```

Enable and start:

```bash
systemctl daemon-reload
systemctl enable --now gradatum-engine-chat
```

Check status and logs:

```bash
systemctl status gradatum-engine-chat
journalctl -u gradatum-engine-chat -f
```

### Multiple instances

Each instance needs its own unit file and config file. Example for two instances
(one chat, one embed):

```bash
# /etc/gradatum/conf.d/70-engine-chat.toml   → port 11435, child_port 11436
# /etc/gradatum/conf.d/70-engine-embed.toml  → port 11437, child_port 11438

# /etc/systemd/system/gradatum-engine-chat.service
# /etc/systemd/system/gradatum-engine-embed.service
```

The two instances are completely independent: separate processes, separate ports,
separate restart budgets.

---

## 9. Verifying the deployment

### Health check

```bash
# Check supervisor health (HTTP 200 = ready, 503 = unhealthy/starting)
curl -s http://127.0.0.1:11435/health | python3 -m json.tool

# Expected output when ready:
# {
#   "status": "ok",
#   "model_kind": "chat",
#   "version": "..."
# }
```

### Chat smoke test

```bash
curl -s http://127.0.0.1:11435/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "curator",
    "messages": [{"role": "user", "content": "Hello"}],
    "max_tokens": 32
  }' | python3 -m json.tool
```

### Embed smoke test

```bash
curl -s http://127.0.0.1:11437/v1/embeddings \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "embed",
    "input": "Hello world"
  }' | python3 -m json.tool

# Verify dimensions match your model (e.g. 384, 768, or 1024).
```

---

## 10. Multi-instance wiring with gradatum-gateway

`gradatum-gateway` routes requests to engine instances using an alias-to-provider
mapping. Each engine instance registers as a named provider. The gateway handles:

- **Primary + fallback routing**: if the primary engine is unhealthy, the gateway
  automatically falls back to the configured fallback provider.
- **Circuit-breaker**: consecutive failures open the circuit and route around the
  unhealthy instance.

Example gateway config fragment (generic):

```toml
[providers.engine-chat]
base_url = "http://127.0.0.1:11435"
timeout_secs = 120

[providers.engine-embed]
base_url = "http://127.0.0.1:11437"
timeout_secs = 30

[providers.fallback-embed]
base_url = "http://127.0.0.1:8431"   # standalone embeddings service as fallback (adapt to your deployment)
timeout_secs = 60

[[aliases]]
name = "curator"
primary = "engine-chat"
# fallback omitted: returns 503 if primary unhealthy

[[aliases]]
name = "embed"
primary = "engine-embed"
fallback = "fallback-embed"
```

When an engine becomes unhealthy (restart budget exhausted), the gateway's health probe
detects the `503` from `/health` and routes to the fallback until the engine recovers
(systemd restart).

---

## 11. Upgrade procedure

1. Build or download the new `gradatum-engine` binary.
2. Stop the instance: `systemctl stop gradatum-engine-chat`.
3. Replace the binary at `/usr/local/bin/gradatum-engine`.
4. Start the instance: `systemctl start gradatum-engine-chat`.
5. Verify: `curl http://127.0.0.1:11435/health`.

For zero-downtime upgrades with the gateway in place, bring up the new instance on a
different port, verify it is healthy, then update the gateway config to point to the
new port, and stop the old instance.

---

## 12. App-host upgrade order: `gradatum-server` before `gradatum-worker`

`gradatum-server` and `gradatum-worker` are **not independently versionable**. The worker
drives the vault through internal HTTP routes served by `gradatum-server`, and releases add
routes to that surface. A worker newer than its server therefore calls endpoints the server
does not expose yet.

> **Rule** — upgrade both binaries in the same operation. Stop the worker first, start the
> server first. Never upgrade the worker alone.

```
stop  gradatum-worker  →  stop  gradatum-server
install both binaries
start gradatum-server  →  start gradatum-worker
```

Stopping the worker first lets it drain against a server that is still up; starting the
server first means the worker never runs a single job against an older server. The repo's
`scripts/deploy-gradatum-local.sh` already applies this order — the constraint is written
here for manual, partial, or rolling upgrades, which is where it gets broken.

### Why the order matters

The rule is not a convention: skipping it silently forfeits the fix a release ships.

Concrete case, v1.0.0. When the forget job meets a note already forgotten, it skips it —
re-running the full forget would re-stamp `forgotten_at`/`forgotten_by` at call time and
destroy the audit trail of the *first* forget. But the skip alone used to leave the index
mark unrepaired: the index still said "live" while the frontmatter said "forgotten", so the
note stayed **searchable** although the vault declared it forgotten. To close that, the skip
branch now calls a route only the new server exposes:

```
POST /internal/v1/note/{ulid}/forget-resync?vault_id=<vault_id>
```

Against an **older** server the route does not exist, so the server answers `404`. The
worker's HTTP client treats every 4xx as terminal (no retry), maps `404` to a client-side
`NotFound` error, logs a WARN, and **carries on with the batch**.

That is a clean degradation, and deliberately so: the note *is* forgotten in the vault, so
failing the batch would be the worse outcome. Nothing crashes, nothing is lost, no job is
stuck. What does not happen is the repair — the index stays out of sync, which is exactly
the defect the release closes. **Clean is not the same as harmless**, and because it is
silent you will not see it unless you read the logs.

### Observable symptom when the order is inverted

In the worker journal (`journalctl -u gradatum-worker`):

```
forget: index resync failed — note left desynchronised (searchable), batch continues
```

Functionally: notes that were forgotten **remain findable through search**, indefinitely —
the WARN is emitted once per affected note per forget run, not retried in the background.

> The `404` is ambiguous read on its own: the *new* server also returns `404` when no index
> row exists for that (ULID, `vault_id`). Do not infer a version skew from the WARN alone —
> confirm it against `/health`, below.

### Verify the deployed commit, not just the version

`scripts/deploy-gradatum-local.sh` **does not build** unless `--build` is passed: it copies
`target/release/{gradatum-server,gradatum-worker}` as they are. If the last build in that
tree ran under the dev profile, `target/release` still holds an earlier build, and the
deploy reports success while installing a stale binary — the same silent-skew class as the
one above.

The semantic version does not catch this: it is identical across dozens of consecutive
commits. The build commit does.

```bash
# Server: /health carries both fields
curl -s http://127.0.0.1:19090/health | jq -r '.version, .build_sha'

# Expected reference
git rev-parse --short HEAD
```

`build_sha` must equal `HEAD`. A value of `unknown` means the binary was built outside a git
checkout — that is an absence of proof, not a match.

The worker has **no `/health` endpoint of its own**; check it through its version string,
which both binaries print in the same stable format:

```bash
gradatum-server --version   # gradatum-server <semver> (build_sha <sha>)
gradatum-worker --version   # gradatum-worker <semver> (build_sha <sha>)
```

Both must report the same `build_sha` — that is the check that catches a half-applied
upgrade, and it is the one worth running after any manual deploy.

---

## 13. Troubleshooting

| Symptom | Likely cause | Action |
|---|---|---|
| `EngineError::ModelLoad: llama_server_bin canonicalize failed` | Binary not found at configured path | Install `llama-server` to `/usr/local/bin/` |
| `EngineError::ModelLoad: ... is outside the allowed prefix` | Binary path outside allowed prefix | Place binary in `/usr/local/bin/` or `/opt/gradatum/bin/` |
| `EngineError::ModelLoad: model_path: path must be under /opt/gradatum/models/` | Model file outside allowed prefix | Move GGUF to `/opt/gradatum/models/` |
| `EngineError::BadRequest: extra_args: flag '...' is not allowed` | Flag not in allow-list | Remove the flag or check Section 6 |
| `/health` returns `503` immediately | Child startup failed or timed out | Check `journalctl -u gradatum-engine-chat` for child stderr |
| `/health` returns `503` after running fine | Restart budget exhausted (flapping child) | Check model compatibility with your `llama-server` version |
| GPU not used despite `gpu_layers > 0` | Missing GPU runtime or env vars not injected | Verify the GPU runtime is installed; env vars prefixed `VK_*`/`CUDA_*` etc. are auto-injected |
| Engine restarts in a loop | Child crashes immediately (model incompatible, OOM) | Check `journalctl` for child output; reduce `context_len` or `gpu_layers` |
| `bind_addr '0.0.0.0' interdit` | Wildcard bind rejected | Set a specific unicast IP or use `127.0.0.1` |
