#!/usr/bin/env bash
# quickstart-docker.sh — Starts the gradatum stack via docker-compose.yml
# (server + worker + local inference + init), reveals the pre-provisioned
# `main-agent` API key (init already minted it — the script never creates a key)
# and checks the health check.
#
# Usage:
#   bash scripts/quickstart-docker.sh [OPTIONS]
#
# Options:
#   --no-build       Skip `docker compose build` (image already built)
#   --token TOKEN    Reuse an existing GRADATUM_INTERNAL_TOKEN instead of
#                     generating a new one (useful to restart without recreating everything)
#   --yes            Non-interactive mode: skip the confirmation prompt
#
# What this script does, in order:
#   1. Preflight checks (docker, docker compose plugin, compose file present)
#   1b. Ensures the local inference weights are on disk (embed + curator). The
#       default stack serves both (llama-embed :8436, llama-chat :8000) and the
#       read-only bind mounts require the .gguf files BEFORE `up`. Missing +
#       --yes -> auto-fetch via scripts/fetch-models.sh; missing interactively
#       -> prompt; declined -> stop with the exact manual command (no surprise
#       multi-GB download).
#   2. Generates GRADATUM_INTERNAL_TOKEN if absent (docker-compose.yml requires it:
#      `${GRADATUM_INTERNAL_TOKEN:?...}` on the gradatum-worker service) and PERSISTS
#      it to `.env` (0600) so `docker compose` keeps working in a fresh shell
#   3. `docker compose build` (the image is built locally — `build: .` —
#      there is NO `docker pull`: the ghcr.io/gradatum/gradatum image is
#      private as of today, see docs/guides/A-docker-quickstart.md)
#   4. `docker compose --profile init up gradatum-init` (one-shot vault init)
#   5. `docker compose up -d gradatum-server gradatum-worker llama-embed llama-chat`
#   6. Poll `/health` until available (60s timeout)
#   7. Reveals the PATH of the pre-provisioned `main-agent` API key file that
#      `init` recorded at step 4 (0600) — never prints the secret itself
#   8. Prints the health URL, how to read that key, and the next step (MCP wiring).
#      The Studio UI (/ui/) is NOT bundled in this image, so it is not advertised as
#      running — see docs/guides/D-mcp-and-studio.md.
#
# Worker↔server internal API — resolved, kept for the record:
#   `gradatum-admin init` writes an `[internal_api]` section into server.toml
#   unconditionally (crates/gradatum-admin/src/init.rs,
#   `generate_server_toml_template`) — this section used to be missing and
#   is not anymore. `docker-compose.yml` doesn't even rely on that
#   auto-generated value: GRADATUM_INTERNAL_API__TOKEN (server) and
#   GRADATUM_INTERNAL_TOKEN (worker) are both bound to the same value this
#   script generates below, overriding whatever `init` wrote (env beats
#   TOML). See docs/guides/A-docker-quickstart.md
#   §"Worker deployment — resolved caveats" for the full record — all three
#   points once listed there are resolved.
#   This compose flow has been run end to end against a live Docker daemon
#   (VPS CA-1, 2026-08-12, repo @ 482fcaa4) in two commands — gradatum-init is
#   behind the `init` profile, so a plain `up` never starts it: first
#   `docker compose --profile init up gradatum-init` (exited 0), then the default
#   stack (server + worker + llama-embed + llama-chat, no profile) which all came
#   up healthy; a note written via the API then round-tripped through the LLM
#   curation path. See docker-compose.yml header for the exact claim. The
#   `gateway` and `engine` profiles were not part of that run and remain unverified.
#
# Prerequisites:
#   - docker + the `docker compose` plugin (v2, a subcommand — not the separate
#     `docker-compose` v1 binary)
#   - Run from the root of the Gradatum workspace (docker-compose.yml present)

set -euo pipefail

DO_BUILD=true
DO_YES=false
TOKEN=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build) DO_BUILD=false; shift ;;
        --token) TOKEN="$2"; shift 2 ;;
        --yes) DO_YES=true; shift ;;
        -h|--help)
            sed -n '2,55p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

STEP=0
TOTAL_STEPS=8

step() {
    STEP=$(( STEP + 1 ))
    echo ""
    echo "[$STEP/$TOTAL_STEPS] $*"
}

ok() {
    echo "  OK"
}

fail() {
    echo "  FAIL: $*" >&2
    exit 1
}

confirm() {
    local msg="$1"
    if [[ "$DO_YES" == "true" ]]; then
        echo "  (--yes) auto-confirming: $msg"
        return 0
    fi
    read -r -p "  $msg [y/N] " reply
    if [[ ! "$reply" =~ ^[yY]$ ]]; then
        echo "  Cancelled."
        exit 0
    fi
}

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Gradatum — Docker quickstart"
echo "  BUILD: $DO_BUILD"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# ─── [1/8] Preflight checks ────────────────────────────────────────────────────

step "Preflight checks"

if [[ ! -f "docker-compose.yml" ]]; then
    fail "docker-compose.yml not found — run this script from the root of the Gradatum workspace"
fi

if ! command -v docker &>/dev/null; then
    fail "docker not found — see https://docs.docker.com/engine/install/"
fi

if ! docker compose version &>/dev/null; then
    fail "'docker compose' plugin (v2) missing — 'docker-compose' (v1, separate binary) is not supported here"
fi

if ! command -v curl &>/dev/null; then
    fail "curl is required for the healthcheck"
fi

ok

# ─── [1b] Local inference weights (embed + curator) ─────────────────────────────

# The default stack serves both inference services; their read-only bind mounts
# (./models/embed, ./models/chat) need the .gguf files present before `up`.
# scripts/fetch-models.sh owns provenance (pinned HF revision + sha256) — this
# step only decides whether to invoke it, and never downloads by surprise.
EMBED_MODEL="./models/embed/bge-m3-q8_0.gguf"
CHAT_MODEL="./models/chat/Qwen3-4B-Instruct-2507-UD-Q4_K_XL.gguf"

step "Local inference weights (embed + curator)"
if [[ -f "$EMBED_MODEL" && -f "$CHAT_MODEL" ]]; then
    echo "  Both model files present — skipping fetch."
    ok
else
    missing=()
    [[ -f "$EMBED_MODEL" ]] || missing+=("embed  ($EMBED_MODEL)")
    [[ -f "$CHAT_MODEL"  ]] || missing+=("curator ($CHAT_MODEL)")
    echo "  Missing weights:"
    for m in "${missing[@]}"; do echo "    - $m"; done
    echo "  scripts/fetch-models.sh will download them from Hugging Face"
    echo "  (~0.6 GB embed + ~2.4 GB curator, pinned revision + sha256 verified)."
    if [[ ! -f "scripts/fetch-models.sh" ]]; then
        fail "scripts/fetch-models.sh not found — run from the Gradatum workspace root"
    fi
    if [[ "$DO_YES" == "true" ]]; then
        echo "  (--yes) fetching now."
    else
        read -r -p "  Download the missing model weights now? [y/N] " reply
        if [[ ! "$reply" =~ ^[yY]$ ]]; then
            fail "model weights required before 'up' — fetch them manually with:
    bash scripts/fetch-models.sh
  then re-run this script. (Or run server+worker only, pointing [embed]/[curator.llm]
  at your own endpoints: docker compose up -d --no-deps gradatum-server gradatum-worker)"
        fi
    fi
    bash scripts/fetch-models.sh || fail "model fetch failed — see the output above"
    ok
fi

# ─── [2/8] Worker↔server internal token (generated + persisted to .env) ────────

# The compose file requires this token — `${GRADATUM_INTERNAL_TOKEN:?...}` on both
# gradatum-server and gradatum-worker. Exporting it only covers THIS shell; every
# later `docker compose` in a fresh shell (ps / logs / exec / down) would then fail
# to interpolate and become unusable. We therefore PERSIST it to `.env` in the
# compose/project directory, which docker compose loads automatically. `.env` holds
# a secret, so it is written 0600 and is already covered by .gitignore (**/.env).

ENV_FILE=".env"
ENV_KEY="GRADATUM_INTERNAL_TOKEN"

step "$ENV_KEY (persisted to $ENV_FILE)"

# Value already recorded in .env — set by the operator or by a previous run. The
# first matching assignment wins, mirroring docker compose's own .env parsing.
existing_env_token() {
    [[ -f "$ENV_FILE" ]] || return 1
    local line
    line="$(grep -m1 -E "^[[:space:]]*(export[[:space:]]+)?${ENV_KEY}=" "$ENV_FILE" 2>/dev/null)" || return 1
    line="${line#*=}"                    # drop everything up to the first '='
    line="${line%\"}"; line="${line#\"}" # strip optional surrounding double quotes
    line="${line%\'}"; line="${line#\'}" # strip optional surrounding single quotes
    printf '%s' "$line"
}
PERSISTED_TOKEN="$(existing_env_token || true)"

# Precedence for the effective token: explicit --token > value already in .env >
# freshly generated. Adopting .env on a plain re-run makes the script idempotent
# (no regeneration, no split-brain with the running containers).
if [[ -n "$TOKEN" ]]; then
    echo "  Using the token passed via --token."
elif [[ -n "$PERSISTED_TOKEN" ]]; then
    TOKEN="$PERSISTED_TOKEN"
    echo "  Reusing the token already recorded in $ENV_FILE."
else
    if command -v openssl &>/dev/null; then
        TOKEN="$(openssl rand -hex 32)"
    else
        TOKEN="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
    fi
    echo "  Generated (32 hex bytes)."
fi

# Persist the effective token to .env, idempotently and without leaking the secret.
persist_token() {
    # Same value already on disk → nothing to write (no duplicate line, no churn).
    if [[ "$PERSISTED_TOKEN" == "$TOKEN" ]]; then
        echo "  $ENV_FILE already records this token — left untouched."
        return 0
    fi
    # A secret must land in a writable directory. If it is not, degrade loudly:
    # the export below keeps THIS run working, but say plainly that a fresh shell
    # will not — and how to fix it — rather than half-failing in silence.
    if [[ ! -w "." ]]; then
        echo "  WARNING: '$PWD' is not writable — cannot persist $ENV_KEY to $ENV_FILE." >&2
        echo "  The stack runs for THIS shell (token exported below), but in a fresh shell" >&2
        echo "  'docker compose' (ps / logs / exec / down) will fail to interpolate the token." >&2
        echo "  Fix: run from a writable directory (or 'chmod u+w .'), then re-run this script." >&2
        return 0
    fi
    if [[ -f "$ENV_FILE" ]]; then
        # Rewrite atomically via a temp file: strip any prior assignment (so we
        # never append a second one), keep every other line, then add ours. This
        # only needs a writable directory, and never truncates .env in place.
        local tmp
        tmp="$(mktemp "./.env.XXXXXX")"
        chmod 600 "$tmp"
        grep -vE "^[[:space:]]*(export[[:space:]]+)?${ENV_KEY}=" "$ENV_FILE" > "$tmp" || true
        printf '%s=%s\n' "$ENV_KEY" "$TOKEN" >> "$tmp"
        mv "$tmp" "$ENV_FILE"
        chmod 600 "$ENV_FILE"
        if [[ -n "$PERSISTED_TOKEN" ]]; then
            echo "  Replaced $ENV_KEY in $ENV_FILE (previous value overwritten)."
        else
            echo "  Added $ENV_KEY to existing $ENV_FILE (other lines preserved)."
        fi
    else
        # Fresh file: umask 077 so the secret is never group/other-readable, even
        # for the instant between create and chmod.
        ( umask 077; printf '%s=%s\n' "$ENV_KEY" "$TOKEN" > "$ENV_FILE" )
        chmod 600 "$ENV_FILE"
        echo "  Wrote $ENV_FILE (0600)."
    fi
    # Defence in depth: flag an operator-supplied .env left group/other-readable.
    local perms
    perms="$(stat -c '%a' "$ENV_FILE" 2>/dev/null || echo '?')"
    [[ "$perms" == "600" || "$perms" == "?" ]] || \
        echo "  NOTE: $ENV_FILE is mode $perms and holds a secret — consider 'chmod 600 $ENV_FILE'." >&2
}
persist_token

export GRADATUM_INTERNAL_TOKEN="$TOKEN"

ok

# ─── [3/8] Build ────────────────────────────────────────────────────────────────

if [[ "$DO_BUILD" == "true" ]]; then
    step "docker compose build (local image — no pull, ghcr.io/gradatum/gradatum is private)"
    docker compose build
    ok
else
    step "Build skipped (--no-build)"
    ok
fi

# ─── [4/8] One-shot init ─────────────────────────────────────────────────────────

step "Vault init ('init' profile, one-shot)"
docker compose --profile init up gradatum-init
ok

# ─── [5/8] Start the default stack (server + worker + local inference) ──────────

# Names all four services explicitly. The worker depends_on BOTH llama-embed AND
# llama-chat (service_healthy): a freshly-init'd server.toml sets [curator]
# backend = "openai_compat" with [curator.llm] base_url = "http://localhost:8000",
# so the worker's curator calls the chat model at :8000 — see the worker's
# depends_on in docker-compose.yml and init.rs generate_server_toml_template.
# A plain `up` of the worker would pull both llama services in via depends_on;
# naming them here just makes the started set self-evident.
# To run WITHOUT the bundled inference, use instead:
#   docker compose up -d --no-deps gradatum-server gradatum-worker
step "Starting gradatum-server + gradatum-worker + llama-embed + llama-chat"
docker compose up -d gradatum-server gradatum-worker llama-embed llama-chat
ok

# ─── [6/8] Healthcheck ───────────────────────────────────────────────────────────

step "Healthcheck /health (60s timeout)"
DEADLINE=$(( $(date +%s) + 60 ))
until curl -fsS http://127.0.0.1:19090/health >/dev/null 2>&1; do
    if [[ "$(date +%s)" -ge "$DEADLINE" ]]; then
        echo "  gradatum-server logs (last lines):"
        docker compose logs --tail=40 gradatum-server || true
        fail "healthcheck never returned 200 within 60s — see the gradatum-server logs above, and docs/guides/A-docker-quickstart.md for the compose flow"
    fi
    sleep 2
done
ok

# ─── [7/8] Reveal the pre-provisioned API key (path only, never the secret) ─────

# The quickstart does NOT create a key. `gradatum-admin init` (step 4) already
# minted the mandatory `main-agent` bootstrap key and RECORDED its secret, 0600,
# under the --root it wrote (/var/lib/gradatum): config/main-agent.apikey.txt
# (crates/gradatum-admin/src/init.rs, apikey_txt_path). That root is the
# `gradatum-state` volume, mounted at the SAME path by gradatum-server — so the
# file the init container wrote is the file the running server reads. Verified
# empirically 2026-08-10: `init --root <R>` writes <R>/config/main-agent.apikey.txt
# mode 0600 holding an ak_ token.
#
# Why reveal instead of create: `main-agent` is declared by the default
# `hierarchical` preset, so its key authenticates. An ad-hoc `--owner
# quickstart-docker` is declared by NO preset; `api-key create` rightly refuses
# it ("undeclared identity"), because such a key would authenticate then be
# refused everywhere — indiscernible from an outage. We reveal the PATH, never
# the secret: a printed secret lands in the terminal history and session logs.
# This mirrors init's own 0600-file discipline.
KEY_PATH="/var/lib/gradatum/config/main-agent.apikey.txt"
step "Locating the pre-provisioned main-agent API key"
if docker compose exec -T gradatum-server test -f "$KEY_PATH"; then
    echo "  API key file (inside the gradatum-server container): $KEY_PATH"
    echo "  Not printed here — reading it is your call. To read it:"
    echo "    docker compose exec gradatum-server cat $KEY_PATH"
    echo "  The file lives on the 'gradatum-state' Docker volume, owned by the"
    echo "  in-container 'gradatum' user — not directly readable from the host."
    echo "  Use the exec above, not a host path."
    ok
else
    KEY_MISSING=1
    echo "  WARNING: $KEY_PATH not found in the gradatum-server container." >&2
    echo "  init mints + records it only on a FRESH vault; on a pre-existing" >&2
    echo "  volume whose key file was removed the secret is unrecoverable and" >&2
    echo "  must be rotated (find the prefix, then rotate):" >&2
    echo "    docker compose exec gradatum-server gradatum-admin api-key list   --root /var/lib/gradatum" >&2
    echo "    docker compose exec gradatum-server gradatum-admin api-key rotate --root /var/lib/gradatum --prefix <ak_...>" >&2
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Gradatum is running — health OK at http://127.0.0.1:19090/health"
echo "  (The Studio UI at /ui/ is NOT bundled in this image and returns 404 until its"
echo "   static bundle is built and placed on disk — see docs/guides/D-mcp-and-studio.md.)"
if [[ "${KEY_MISSING:-0}" == "1" ]]; then
    echo "  API key: NOT found — see the rotate hint above to mint a fresh one."
else
    echo "  API key (main-agent): read it with"
    echo "    docker compose exec gradatum-server cat $KEY_PATH"
fi
echo ""
echo "  Next: wire an MCP client (Claude Code / Studio) to this key —"
echo "  docs/guides/D-mcp-and-studio.md"
echo ""
echo "  Note: /health above confirms the server is up, not that the worker's"
echo "  curator/embedding jobs run end to end over the internal API (:19092)."
echo "  This script does not exercise that path."
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
