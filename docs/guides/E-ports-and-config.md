# Guide E — Ports & configuration reference

> Moved from `PORTS.md` (repo root) — see that file for a one-line pointer back here.
> This is the canonical port matrix; every other document (`ARCHITECTURE.md`, `README.md`,
> `docs/DEPLOYMENT.md`) links here rather than restating it.

> Range `19090–19100` reserved. Each port is `19090 + role offset`.

---

## Convention

Gradatum follows the **`19090 + offset`** pattern to:

- Stay outside the canonical Prometheus range (`9090–9099`) — operators routinely use `9090–9099` for `prometheus`, `alertmanager`, `pushgateway`, and node exporters. Gradatum components must not collide.
- Cluster all Gradatum services in a single 11-port range (`19090–19100`), making firewall rules, reverse-proxy configs and observability dashboards trivial to scope.
- Leave room above the core range for project-local extensions (custom adapters, third-party integrations) without risking conflict. The exact boundaries are stated once, in "Range allocation policy" below — this section does not restate them.

---

## Default port matrix

| Service | Default port | Offset | Notes |
|---|---|---|---|
| `gradatum-server` (HTTP/MCP) | **19090** | 0 | Main entrypoint. SSE, JSON, MCP-over-HTTP. |
| `gradatum-server` metrics | 19091 | 1 | Prometheus `/metrics`, separate listener bound to loopback by default (security caveat C7 — metrics binding must stay loopback, see [SECURITY.md § Hardening defaults](../../SECURITY.md#hardening-defaults)). |
| `gradatum-server` internal API | 19092 | 2 | `/internal/v1/*` (worker ↔ server). Spawned **only** when `[internal_api] token` is configured, in its own background task. Loopback-enforced — but that check runs *inside* the background task: a non-loopback `[internal_api] bind` makes the internal listener exit with an error that is only **logged**, not propagated. **The main server keeps running and serving `/health` normally; the internal API simply never comes up.** This does not abort the boot. |
| `gradatum-gateway` (LLM gateway, optional) | **8436** — `19093` reserved, unused | 3 | OpenAI-compatible + Anthropic mappers. **This service has no default in code**: `[server] listen` is mandatory, so the value below is a convention, not a fallback. `19093` was reserved here under the `19090 + offset` scheme, but every shipped example and deployment binds **8436** (`crates/gradatum-gateway/examples/spike-engine-routing.toml`, `ARCHITECTURE.md`, `README.md`, `docs/DEPLOYMENT.md`). The `19093` reservation is recorded here for completeness, this table being the sole canonical port matrix. **Caveat:** `19093` would collide with Alertmanager's default `9093` only if an operator reused that numeric port elsewhere; the `19090+` prefix avoids it. |
| `gradatum-vault` HTTP (read-only API, optional) | 19094 | 4 | Reserved for vault-only deployments without `gradatum-server`. |
| Reserved | 19095 | 5 | — |
| `gradatum-studio` (admin UI) | 19096 | 6 | Port reserved for a future standalone Studio process. In `1.0.0`, Studio is served by `gradatum-server` at `/ui/*` (port 19090, no separate process). |
| Reserved | 19097–19099 | 7–9 | Future use. |
| `gradatum-worker` healthcheck | 19100 | 10 | Worker has no public listener; this port exposes `/health` only. |

**Override:** every default can be overridden via TOML, env var, or CLI flag — see "Override matrix" below.

---

## Override matrix

Listeners are configured as full **socket addresses** (`ip:port`), not as bare port numbers.
Precedence: **env > TOML > defaults** (figment layering).

`gradatum-server` takes exactly one CLI flag — `--config <path>`. It exposes no per-port flag.
Environment overrides use the `GRADATUM_` prefix with a **double underscore** as the section
separator (`Env::prefixed("GRADATUM_").split("__")`), so the variable name mirrors the TOML path.

| Setting | TOML key (`server.toml`) | Environment variable |
|---|---|---|
| Server listener | `[server] bind = "127.0.0.1:19090"` | `GRADATUM_SERVER__BIND` |
| Server metrics listener | `[server] metrics_bind = "127.0.0.1:19091"` | `GRADATUM_SERVER__METRICS_BIND` |
| Storage root | `[storage] root = "/var/lib/gradatum"` | `GRADATUM_STORAGE__ROOT` |
| Log format | `[log] format = "json"` | `GRADATUM_LOG__FORMAT` |

`gradatum-gateway` and `gradatum-engine` are separate binaries with their own config files;
`gradatum-engine` reads its config path as a **positional** argument
(`gradatum-engine <config-path>`) and takes **no** environment override — its TOML file is
its only configuration source (F-190). The `GRADATUM_ENGINE_` prefix names one thing only,
the event-log credential `GRADATUM_ENGINE_API_KEY`.
Full `[engine]` field reference: [docs/DEPLOYMENT.md §4](../DEPLOYMENT.md#4-configuration-reference).

**Bind safety (`gradatum-server`):** two fail-closed guards, both detailed in
[SECURITY.md § Hardening defaults](../../SECURITY.md#hardening-defaults) (referred to elsewhere
in this project as caveats C3 and C7):

- **C3 — main listener.** `[server] bind` on a non-loopback address **without** `[server.tls]`
  → the server refuses to boot.
- **C7 — metrics listener.** `[server] metrics_bind` must be loopback **unconditionally** — TLS
  does not lift this; a non-loopback metrics address aborts startup.
- The other listeners in the matrix above are not covered by these two guards. In particular,
  the internal-API listener (see table above) enforces its own loopback check, but a failure
  there does not abort the server's boot — see the caveat in that row.

---

## Known collisions to watch

| External service | Default port | Mitigation |
|---|---|---|
| Prometheus server | 9090 | Out of range — no collision. |
| Alertmanager | 9093 | Out of range — no collision. |
| Pushgateway | 9091 | Out of range — no collision. |
| Anything in `19090–19100` | — | Operator's responsibility to coordinate; Gradatum logs the bind error and exits non-zero on conflict. |

---

## Range allocation policy

- `19090–19100`: reserved for Gradatum core services. Any new Gradatum service must claim an offset here.
- `19101–19199`: free for operator-local extensions (custom adapters, ad-hoc proxies).
- `>= 19200`: out of Gradatum scope.

Adding a new Gradatum service requires updating the table above as part of the project-map
feature card that introduces it (see [`../../GOVERNANCE.md`](../../GOVERNANCE.md) § Structural
change tracking).

---

## `server.toml` — fields set by `gradatum-admin init`

`gradatum-admin init` (source: `crates/gradatum-admin/src/init.rs`, `generate_server_toml_template`)
writes these sections, unconditionally, `[internal_api]` included:

```toml
[server]
bind = "<--bind value, default 127.0.0.1:19090>"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "<--root value>"
vault_index_path = "<root>/vault/.gradatum/index.db"

[auth]
jwt_ttl_human_secs = 3600
jwt_ttl_service_secs = 86400
revocation_store = "sqlite"
revocation_db_path = "<root>/db/revocation.sqlite"
api_keys_db_path = "<root>/db/api_keys.sqlite"

[acl]
preset_path = "<root>/config/bearer.toml"

[log]
format = "json"

[embed]
enabled = true
endpoint = "http://localhost:8436/v1/embeddings"
model = "bge-m3-Q8_0"
dim = 1024
timeout_ms = 5000

[internal_api]
bind = "127.0.0.1:19092"
token = "<256-bit CSPRNG secret, hex-encoded>"
admin_token = "<256-bit CSPRNG secret, hex-encoded>"
```

`[internal_api]` is **not** opt-in: `init` writes it on every run, `bind` is always the loopback
default (`127.0.0.1:19092`, independent of `--bind`), and `InitArgs` exposes no flag to skip it.
`token` and `admin_token` are two independent 256-bit CSPRNG secrets minted fresh each `init` (or
`init --force`). `token` is additionally copied to `config/internal-worker.token.txt` (mode
0600), specifically so the operator does not have to parse `server.toml` to retrieve it.

**Operator gesture:** to run `gradatum-worker` against this server, read
`config/internal-worker.token.txt` and export its contents as `GRADATUM_INTERNAL_TOKEN` — do
not mint your own value (e.g. `openssl rand -hex 32`) independently of `init`; a self-generated
token will not match what `gradatum-server` reads from `[internal_api].token`, unless something
else overrides it (see "Override matrix" above — env beats TOML).

Verified empirically: `gradatum-admin init --root <tmp-dir> --non-interactive`, then inspecting
the resulting `server.toml` and `config/internal-worker.token.txt`.

---

## `[queue]` — periodic purge of terminal job payloads

Written by `gradatum-admin init` like the sections above (same source:
`crates/gradatum-admin/src/init.rs`), and read by `gradatum-worker` from the same shared
`server.toml` — `gradatum-server` ignores it. This is a **retention** section, and it exists
because the payload of a terminal job *is* the note body it carried: without the task, those
bodies stay in `db/queue.sqlite` indefinitely.

```toml
[queue]
purge_after_secs = 86400      # 24 h — age beyond which a terminal job's payload is erased; MUST be >= 1
purge_interval_secs = 3600    # 1 h — delay between two purges, floored at 60 s
```

- The values above are the **compiled** defaults written out: a worker that finds no `[queue]`
  section applies exactly the same 24 h / 1 h. The section is emitted so a retention nobody can
  read does not become a retention nobody reviews.
- `purge_after_secs = 0` is **refused at parse** — or more exactly, a `[queue]` section that
  carries it is refused whole: the worker logs the rejection, applies the 24 h / 1 h defaults, and
  publishes the fallback as `gradatum_config_degraded{section="queue",cause="parse_failed"}` on
  `GET /metrics`. It is never clamped, because a cutoff of "now" would erase every terminal
  payload outside the keep window on the next tick, irreversibly.
- The **N most recent terminal rows are never touched** (see `TERMINAL_PAYLOAD_KEEP` in
  `gradatum-core`). N is deliberately absent here: it keeps a purge from racing a reader still
  consuming a job's output, and widening it is a code change with a review, never a knob.
- The purge **rewrites** rows — a purged job keeps its status, timestamps and result note; only
  its note body is gone, replaced by the `PayloadPurged` sentinel. `job_status` keeps answering
  what it answered before.
- Run it by hand, or ask what it would take, with
  `gradatum-admin queue purge --older-than 12h` and `gradatum-admin queue purge --dry-run`.
- Running it by hand opens a **second writer connection** on `db/queue.sqlite` — safe while the
  worker is up (WAL, a 5 s `busy_timeout`, and an idempotent purge: a row already carrying
  `PayloadPurged` is neither rewritten nor counted), but it can meet the scheduled purge in a write
  transaction and come back with a transient `SQLITE_BUSY` that asks to be run again.

---

## `[privacy]` — personal-data marking (F-160)

Optional, and **not** written by `gradatum-admin init`. With `[privacy]` absent — or present
with `categories = []` — the write path mounts a no-op detector and scans nothing; the block is
opt-in, added by hand.

```toml
[privacy]
mode = "report_only"          # "report_only" (default) marks only; "mask" rewrites the matched passages in place and is refused at load unless a category is calibrated
categories = []               # enabled categories; empty by default — nothing is scanned until an operator opts in
max_input_bytes = 262144      # 256 KiB — largest body a single detector call may be handed
window_overlap_bytes = 512    # overlap between consecutive windows of a larger body
rescan_enabled = false        # the deferred rescan task is always spawned; this gates its work, never the spawn
rescan_interval_secs = 300    # interval of the deferred rescan pass
# [privacy.thresholds]        # per-category score thresholds, integer thousandths
# [privacy.exceptions]        # literal exception patterns, per tenant, applied after detection
# [privacy.sidecar]           # the out-of-process detector; absent by default
# socket_path = "/run/gradatum-privacy/privacy-sidecar.sock"
# binary_digest = "…"         # 64 lowercase hex characters, required
# model_digest = "…"          # 64 lowercase hex characters, required
# score_mode = "mean_token_prob"
# timeout_ms_sync_per_kib = 800
# breaker_n = 3
# breaker_cooldown_s = 30
```

- `categories` is the **activation list**: a category it does not name is still scanned, and
  what the detector reports about it is still recorded in the note's block — that record is
  what a campaign measures its calibration on. What the absence costs is the marker, and
  nothing else.
- `[privacy.thresholds]` takes integer **thousandths** (`0..=1000`), the same unit a note
  records, so no float ever crosses the comparison. `default_m` applies to any activated
  category that names no entry of its own; a key is a canonical category label (`email = 900`).
  A span marks only when its category is activated **and** its score meets that threshold.
  `default_m = 0` means **not calibrated** — the category is scanned and counted and cannot
  mark; it is not a floor that passes everything. A label outside the taxonomy, a value above
  `1000`, and a key for a category `categories` does not list are each **refused at load**.
- `[privacy.exceptions]` is keyed by tenant and holds literal patterns (`main = ["alice@example.com"]`).
  It is applied **last** — after detection and after the thresholds — and to the spans
  themselves: what an entry covers is declared not personal data, so it marks nothing, is
  dropped from the block and is not counted. An entry whose literal does not occur covers
  nothing and changes nothing. Each firing is counted per **digest** of the pattern, never per
  pattern, so the metrics channel cannot read back a value the configuration withholds.
- `mode = "mask"` replaces every passage the activation **marks** with `[redacted]` and is
  **irreversible**: no copy of the replaced text is kept, in the note, in its history, or in
  the block. It is an operator's decision, never a default, and it is admitted only over an
  activation that can perform a rewrite — at least one activated category with a threshold
  **above zero**. A `mask` over categories that all sit at the observation point is refused at
  load, because it would read as a running redaction while rewriting nothing, and the two are
  indistinguishable afterwards on a corpus where nothing matched.
  Once running, the mode is announced at every boot: a `WARN` line and one audit event
  (`event = "privacy_mode"`) naming the mode and the mounted detector. A deployment in
  `report_only` writes neither.
  What the mode destroys is bounded by the same rule as the marker, span by span: a span below
  its category's threshold is left in place. What it does *not* reach is unchanged — an
  exception-covered span is spared (it never reaches the rewrite), `[privacy]` never scans the
  `identity` section or the `code-*` vaults, and a note whose pass did not complete is
  **refused** rather than written unmasked.
  Under `mask`, the note's block keeps its `counts` and drops its `spans`: the offsets would
  otherwise point a reader at text that no longer exists. The version snapshot (`F-40`) is
  suppressed for a writing that masked anything, since a snapshot would copy the replaced text
  into the one directory the mode does not rewrite. Entering mask mode does **not** purge
  history written before it: notes masked earlier in `report_only` keep their clear copies in
  `.history/`.
- `[privacy.sidecar]` mounts the **out-of-process** detector in place of the in-process
  one. The block is a **2.5.0** addition and is absent from 2.4.0, which has no `sidecar` field.
  It speaks over a Unix socket at mode `0660`, owned by the sidecar account and the server's group,
  under `/run/gradatum-privacy/` — a runtime directory the sidecar's own unit owns, so that
  declaring it takes nothing from the server's `/run/gradatum`. A filesystem socket, **not
  a TCP port**, so it claims no offset in the port matrix above. Its systemd hardening and
  peer-authentication properties are described in
  [SECURITY.md § Personal-data marking (F-160)](../../SECURITY.md#personal-data-marking-f-160).
  - **When it is mounted**: the block must be present **and** `categories` non-empty. A block over
    an empty activation is admitted and mounts nothing — it names a detector no write would ask
    for, and nothing is sent to it. Without the block, a non-empty activation mounts the
    in-process detector, exactly as it did before this key existed.
  - **The three required keys** are `socket_path`, `binary_digest` and `model_digest`. The digests
    are **64 lowercase hexadecimal characters**, bare — no `sha256:` tag, because they are
    compared to what the handshake reports by exact string equality. A missing key, a digest of
    another length, or a digest with an uppercase or non-hexadecimal character is **refused at
    load**, and the refusal names the field, never the value. An empty `socket_path` is refused
    the same way.
  - **Startup refuses a sidecar that does not answer.** Mounting opens the socket and takes the
    handshake; if the sidecar is unreachable, times out, or announces a binary, a model or a
    score mode other than the pinned one, the server **fails to start** with an error naming the
    cause. There is deliberately **no fallback** to the in-process detector: a deployment that
    wrote the block down asked for an engine that runs outside the server, and a silent
    substitution would leave it scanning with the engine it did not choose. The repair is to
    start the sidecar, or to remove the block.
  - **`max_input_bytes` and `window_overlap_bytes` are not repeated here.** They stay in
    `[privacy]` and are handed to the sidecar client by the server: two keys for one ceiling
    would be two authorities on how a body is cut, and the two would only have to disagree once
    for a body to be scanned in windows neither value describes.
  - **`score_mode`, `timeout_ms_sync_per_kib`, `breaker_n` and `breaker_cooldown_s`** default to
    the values the client itself defines — `mean_token_prob`, `800`, `3` and `30`. They are the
    sidecar's own calibration, not a second one: the server asks the client for its default
    rather than restating it. `timeout_ms_sync_per_kib` is a starting calibration, not a measured
    one, and is meant to be set from a measurement of the deployed model.
  - **`[privacy.thresholds]` and `[privacy.exceptions]` are unchanged** by this block, and are
    applied in the same place as before: the mounted vault applies the policy it is handed —
    regex floor first, then the model, then the threshold on the **fused** score, then the
    exceptions last. Nothing about the activation moves into the sidecar, and the server does
    not re-apply any of it after the call.
  - **One sidecar per deployment**, shared by every vault of the process: the socket, the
    announced handshake and the circuit breaker live in the mounted detector, and a per-vault
    rebuild would hand each vault its own connection to a process the deployment started once.
