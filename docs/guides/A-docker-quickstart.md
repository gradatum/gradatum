# Guide A — Docker quickstart

The fastest way to get a gradatum server + worker running locally. Uses the
`docker-compose.yml` at the repo root.

---

## Prerequisites

- Docker Engine + the `docker compose` plugin (v2 — a subcommand of `docker`, not the separate
  `docker-compose` v1 binary).
- This repository checked out locally (the image is built from source — see
  [Docker image](#docker-image) below).

---

## Quick path

```bash
git clone https://github.com/gradatum/gradatum.git
cd gradatum
bash scripts/quickstart-docker.sh
```

The script builds the image, generates a `GRADATUM_INTERNAL_TOKEN`, runs the one-shot vault
init, starts `gradatum-server` + `gradatum-worker` (plus the `llama-embed` / `llama-chat`
inference services), waits for `/health`, and reveals the path to the pre-provisioned
`main-agent` API key — `gradatum-admin init` already minted it; the script never creates a key.
Full option list: `bash scripts/quickstart-docker.sh --help`.

The worker-deployment issues this guide once flagged here are resolved — see
[Worker deployment — resolved caveats](#worker-deployment--resolved-caveats-kept-for-the-record)
below for the historical record, kept for reference.

---

## Manual path (what the script does, step by step)

```bash
# 1. A worker↔server internal-API token is required by the compose file
export GRADATUM_INTERNAL_TOKEN="$(openssl rand -hex 32)"

# 2. Build locally — see "Docker image" below for why this isn't `docker pull`
docker compose build

# 3. One-shot vault init (writes only under --root: /var/lib/gradatum; NOT /etc/gradatum)
docker compose --profile init up gradatum-init

# 4. Start the core services
docker compose up -d gradatum-server gradatum-worker

# 5. Verify
curl -fsS http://127.0.0.1:19090/health

# 6. Create an API key (see Guide D for scopes and Studio login)
docker compose exec gradatum-server \
  gradatum-admin api-key create --root /var/lib/gradatum --owner admin --scopes admin --tenant main
```

---

## Docker image

`docker-compose.yml` builds every service from the local `Dockerfile` (`build: .`) — there is
**no `docker pull`** in this flow. `ghcr.io/gradatum/gradatum` exists, but as a **private**
image (org GitHub `gradatum`, `.forgejo/workflows/release.yml` job `build-docker`, currently
gated `if: false` pending a Docker-capable runner). Until that image is made public, building
locally is the only path — plan for the first `docker compose build` to compile the image's five
`--bin` targets (below), which takes several minutes.

**The image does not ship `gradatum-cli`.** The `Dockerfile`'s builder stage runs
`cargo build --release --workspace --bin gradatum-server --bin gradatum-worker --bin
gradatum-admin --bin gradatum-gateway --bin gradatum-engine` — five named targets, not the
whole workspace — and the runtime stage `COPY`s exactly those five binaries to
`/usr/local/bin/`. `gradatum-cli` is never built and never copied; a shell inside the container
confirms `/usr/local/bin/` holds only the five. Same outcome as [Guide B](B-install-binaries.md#archives)'s
pre-built binary archives, where `gradatum-cli` is likewise excluded — for a different reason
there (the archive assembly step drops it), but the end state is identical: no `gradatum-cli`
in either distribution.

---

## Windows

Windows is supported **through this Docker path only** — there is no native Windows build or
binary. Docker Desktop on Windows runs Linux containers via its WSL2 backend: the image built by
`docker compose build` is `linux/amd64`, the same target this project's CI builds and tests
(self-hosted `linux, x64` runners). A Windows host running that container is running the same
target CI validated, not a separate Windows build.

This has not been tested on an actual Windows machine — the paragraph above states which target
gets built and run, not that someone has run it there. The [Docker image](#docker-image) section
above applies unchanged on Windows: there is no public image (`ghcr.io/gradatum/gradatum` is
private, `build: .` in `docker-compose.yml`), so `docker compose build` compiles the image's five
`--bin` targets (not the full 31-member workspace — see [Docker image](#docker-image) above)
**inside the WSL2 VM**.

Two different numbers matter here, and they are not the same budget: a `cargo test --workspace`
run (debug profile, every crate, all dev-dependencies) is far heavier than the release build this
image actually does. Measured on a Linux host for the release build alone: `target/` lands
around **1.3 GB** after `cargo build --workspace --release`, plus roughly **3.2 GB** for the Rust
toolchain itself (`~/.rustup` + `~/.cargo`) if it is not already present in the WSL2 VM — roughly
**4.5 GB** combined, not the workspace test-suite footprint. Budget several minutes for the build
itself.

macOS is out of scope for this project entirely — Apple Silicon is `arm64` but a different
target triple from the Linux `arm64` this project builds, never compiled or tested here, and
there is no Docker-based path for it. Platform support in general:
[docs/DEPLOYMENT.md § Platform support](../DEPLOYMENT.md#platform-support).

---

## Optional profiles

| Profile | Service(s) | What it adds |
|---|---|---|
| `init` | `gradatum-init` | One-shot vault initialization (`gradatum-admin init`). Run once. |
| `gateway` | `gradatum-gateway` | LLM router (alias → provider, circuit-breaker). Needs `/etc/gradatum/gateway.toml` mounted — see [docs/DEPLOYMENT.md §10](../DEPLOYMENT.md#10-multi-instance-wiring-with-gradatum-gateway). |
| `engine` | `gradatum-engine` | `llama-server` supervisor. Needs `/etc/gradatum/engine.toml` — see [docs/DEPLOYMENT.md §4](../DEPLOYMENT.md#4-configuration-reference). In a container context, prefer the default `llama-embed` / `llama-chat` services over embedding `llama-server` inside the engine container. |
Enable a profile with `docker compose --profile <name> up -d <service>`.

> **`llama-embed` and `llama-chat` are not a profile.** They are default services (no `profiles:`
> key in `docker-compose.yml`), part of the operational minimum and started by a plain
> `docker compose up`; the worker `depends_on` both. They are CPU-only `llama.cpp` server
> containers (`ghcr.io/ggml-org/llama.cpp:server`), pre-wired for the `[embed]` and `[curator.llm]`
> config sections, and require GGUF models mounted under `./models/embed` and `./models/chat`.
> Note bodies are sent to these endpoints for embedding/classification — see
> [SECURITY.md § Privacy posture](../../SECURITY.md#privacy-posture).

---

## Worker deployment — resolved caveats, kept for the record

This section used to list three blockers found by reading `docker-compose.yml`. **All three are
resolved in the current file** — confirmed at the time of writing by reading `docker-compose.yml`
directly (`grep -n` on the relevant service blocks), not by re-running the stack; since then the
default stack (which includes `gradatum-init`, `gradatum-server` and `gradatum-worker`) has been
run end to end against a live daemon (see the note below the three points). Kept below as a
historical record, not as an active warning: nothing here should stop you from running the
worker.

### 1. Loopback bind — resolved

`gradatum-init` is invoked with `--bind 127.0.0.1:19090` (`docker-compose.yml`, `gradatum-init`
service, line 101), not a non-loopback address, so `gradatum-server`'s fail-closed check
(`crates/gradatum-server/src/config.rs`, `validate_bind_tls`) does not trigger. The compose
file's own header (top of `docker-compose.yml`) explains the design: `network_mode: host` on
`gradatum-server` and `gradatum-worker` keeps the loopback-bound server reachable at the host's
own `127.0.0.1:19090` without a bridge-published port, which cannot forward to a loopback-bound
process.

### 2. Config path — resolved

`gradatum-init` writes `server.toml` under `<--root>/config/server.toml`; the compose file
passes `--root /var/lib/gradatum` to `gradatum-init` and `--config
/var/lib/gradatum/config/server.toml` to both `gradatum-server` (line 61) and `gradatum-worker`
(line 86) — the exact same path, on the same shared `gradatum-state` volume. No `/etc/gradatum`
mismatch.

### 3. `[internal_api]` — resolved

**As of the current release, this is no longer a gap.** `gradatum-admin init` unconditionally
writes an `[internal_api]` section into the generated `server.toml`
(`crates/gradatum-admin/src/init.rs`, `generate_server_toml_template`) — `bind =
"127.0.0.1:19092"` plus two independent 256-bit CSPRNG secrets (`token`, `admin_token`). There is
no flag to opt out; `InitArgs` exposes none. The worker token is additionally written to
`config/internal-worker.token.txt` (mode 0600). Verified empirically:
`gradatum-admin init --root <tmp-dir> --non-interactive`, then inspecting the resulting
`server.toml` — see
[Guide E — Ports & configuration](E-ports-and-config.md#servertoml--fields-set-by-gradatum-admin-init)
for the exact fields.

The compose flow does not depend on that auto-generated token: `GRADATUM_INTERNAL_API__TOKEN`
(server) and `GRADATUM_INTERNAL_TOKEN` (worker) are both bound to the same
`${GRADATUM_INTERNAL_TOKEN}` exported in step 1 of the manual path above — the environment layer
overrides whatever `init` put in `server.toml` (env beats TOML, see
[Guide E — Override matrix](E-ports-and-config.md#override-matrix)). **Do not hand-edit
`server.toml` to add `[internal_api]`** — it is already there, and this compose file's env
wiring already supersedes it.

---

None of the three points above blocks the worker — confirmed both by source reading of the
current `docker-compose.yml` and, since 2026-08-12, by a live run on a real Docker daemon
(VPS CA-1, repo @ `482fcaa4`) in two commands — `gradatum-init` sits behind the `init` profile,
so a plain `up` never starts it: `docker compose --profile init up gradatum-init` ran the one-shot
init to exit 0, then the default stack (every service without a profile flag) was brought up end to
end — every service reached healthy, and a note written through the API round-tripped
through `gradatum-worker`'s LLM curation path. See the header of `docker-compose.yml` for the
exact claim. That run did not exercise the `gateway` or `engine` profiles, load, or a daemon
without pre-pulled images — treat those as still unverified. No workaround is needed — the
compose file already handles the loopback bind, the config path, and the internal-API token on
its own; there is no reason to drop `gradatum-worker` from this setup.

---

## Uninstall

Everything gradatum owns in this setup is in Docker itself — containers, and three named
volumes (`gradatum-config`, `gradatum-state`, `gradatum-audit`; `docker-compose.yml` names them
under `name: gradatum`, so Compose prefixes them `gradatum_gradatum-config` etc. — check
`docker volume ls | grep gradatum` for the exact names on your host).

**Your vault is `gradatum-state`** (mounted at `/var/lib/gradatum` in every container: the
SQLite DBs, the ACL preset, the JWT signing key). Back it up before anything destructive:

```bash
docker run --rm -v gradatum_gradatum-state:/data -v "$PWD":/backup debian:13-slim \
  tar -czf /backup/gradatum-vault-backup.tar.gz -C /data .
```

**1. Stop and remove the containers** (does not touch volumes):

```bash
docker compose down
```

`down` tears down every container Compose created for this project regardless of which
`--profile` brought it up — you don't need to repeat the profile flags you used at `up` time.
This step alone gets you back to "nothing running" while every volume, and the vault inside it,
is intact.

**2. Remove the built images** (does not touch data — a later `docker compose build` rebuilds
them). Every service here uses `build: .` with no explicit `image:` key, so Compose tags one
image per service as `<project>-<service>` (project name is `gradatum`, set by `name: gradatum`
at the top of `docker-compose.yml`) — list what's actually on your host rather than guessing the
full set:

```bash
docker images | grep '^gradatum-'
docker image rm $(docker images -q --filter reference='gradatum-*')
```

**3. Remove the volumes — destructive, only after your backup from above is confirmed good**:

```bash
docker compose down -v
# or, targeting them by hand if `down -v` doesn't reach a profile-gated volume:
docker volume rm gradatum_gradatum-state gradatum_gradatum-config gradatum_gradatum-audit
```

Step 1 (and 2, if you also want the image gone) gets you back to "gradatum was never brought up
here" while keeping every note in the vault. Step 3 is the only one that discards data — do it
last, and only once you're sure.

---

## Next steps

- [Guide D — MCP & Studio](D-mcp-and-studio.md) — connect an MCP client, create scoped keys, log
  into the Studio UI.
- [Guide E — Ports & configuration](E-ports-and-config.md) — full port matrix and config field
  reference.
- Multi-host GPU topology (separate app-host and gpu-host, beyond this single-host compose
  setup): [docs/DEPLOYMENT.md](../DEPLOYMENT.md).
