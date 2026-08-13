#!/usr/bin/env bash
# fetch-models.sh — Downloads and verifies the local inference model weights the
# default Docker stack needs (embedding + curator), from Hugging Face.
#
# Usage:
#   bash scripts/fetch-models.sh [OPTIONS]
#
# Options:
#   --dest DIR     Root under which ./embed and ./chat are written (default: ./models)
#   --only NAME    Fetch a single model: embed | chat (default: both)
#   --verify       Re-hash files already on disk (default: trust a size match, skip)
#   --dry-run      Print the plan (repo, revision, sha256, size) without downloading
#   -h, --help     This help
#
# ── Why this script exists ────────────────────────────────────────────────────
# docker-compose.yml bind-mounts ./models/embed and ./models/chat read-only and
# names specific .gguf files. The default config ships `[embed] enabled = true`
# pointing at the local embedder (:8436), so the weights MUST be on disk before
# `docker compose up` — an absent mount makes llama-server fail to load and its
# healthcheck never turns green, which (by design) blocks the worker.
#
# ── Provenance policy (deliberate) ────────────────────────────────────────────
# We reference the weights on Hugging Face and NEVER rehost them: the licences do
# not have to be re-assumed, and reproducibility comes from the PINNED REVISION
# (a commit hash, not a moving branch) plus the SHA256 verified LOCALLY after
# download — not from trusting the host. The authoritative sha256 below is the
# git-LFS oid published by the repo for that exact revision.
#
# Licences (verified 2026-08-11, must be re-checked if a revision is bumped):
#   embed : ggml-org/bge-m3-Q8_0-GGUF        MIT        (base BAAI/bge-m3, MIT)
#   chat  : unsloth/Qwen3-4B-Instruct-2507-GGUF  Apache-2.0
#           (base Qwen/Qwen3-4B-Instruct-2507, Apache-2.0)
# Both permit redistribution; pointing to HF is well within them.
#
# Requirements: curl, sha256sum.

set -euo pipefail

DEST="./models"
ONLY=""
VERIFY=false
DRY_RUN=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dest) DEST="$2"; shift 2 ;;
        --only) ONLY="$2"; shift 2 ;;
        --verify) VERIFY=true; shift ;;
        --dry-run) DRY_RUN=true; shift ;;
        -h|--help)
            sed -n '2,38p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

case "$ONLY" in
    ""|embed|chat) ;;
    *) echo "Invalid --only: $ONLY (expected: embed | chat)" >&2; exit 1 ;;
esac

# ── Model manifest (source of truth: pinned revision + authoritative sha256) ──
# One record per line, whitespace-separated:
#   name  subdir  repo  revision  filename  sha256  size_bytes
# `revision` is a HF commit hash (immutable), NOT "main". Bumping a model means
# updating revision + sha256 TOGETHER here — and re-checking the licence.
read -r -d '' MODELS <<'MANIFEST' || true
embed embed ggml-org/bge-m3-Q8_0-GGUF 9eba04c5d75ba5a1595e45de734d36bef4e5cb98 bge-m3-q8_0.gguf aa473d51f451a22f0fcf39ba3330c14bed38a385712b1113440f69df4047a173 634553760
chat chat unsloth/Qwen3-4B-Instruct-2507-GGUF a06e946bb6b655725eafa393f4a9745d460374c9 Qwen3-4B-Instruct-2507-UD-Q4_K_XL.gguf 4bbe1f2f8ebe69fad3be8e15d69f220b06448a9dd26f82d7d81cce88ebfc39fd 2546340960
MANIFEST

STEP=0
step() { STEP=$(( STEP + 1 )); echo ""; echo "[$STEP] $*"; }
ok()   { echo "  OK"; }
fail() { echo "  FAIL: $*" >&2; exit 1; }

command -v curl       &>/dev/null || fail "required tool missing: curl"
command -v sha256sum  &>/dev/null || fail "required tool missing: sha256sum"

human_size() { # bytes -> human, integer-only (no bc dependency)
    local b="$1"
    if   (( b >= 1073741824 )); then echo "$(( b / 1073741824 )).$(( (b % 1073741824) * 10 / 1073741824 )) GiB"
    elif (( b >= 1048576   )); then echo "$(( b / 1048576 )) MiB"
    else echo "${b} B"; fi
}

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Gradatum — Fetch local inference models"
echo "  DEST    : $DEST"
echo "  ONLY    : ${ONLY:-both}"
echo "  DRY-RUN : $DRY_RUN"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

fetched_any=false

while read -r name subdir repo revision filename sha256 size; do
    [[ -z "${name:-}" ]] && continue
    [[ -n "$ONLY" && "$ONLY" != "$name" ]] && continue

    dir="$DEST/$subdir"
    out="$dir/$filename"
    url="https://huggingface.co/${repo}/resolve/${revision}/${filename}"

    step "$name — $repo@${revision:0:12} ($(human_size "$size"))"
    echo "  file  : $out"
    echo "  sha256: $sha256"

    if [[ "$DRY_RUN" == "true" ]]; then
        echo "  [dry-run] would download $url and verify sha256"
        continue
    fi

    # Idempotency: a present file with the expected size is trusted (a full
    # re-hash of a multi-GB file on every run is wasteful). --verify forces the
    # hash check on the existing file instead of trusting the size.
    if [[ -f "$out" ]]; then
        cur_size="$(stat -c '%s' "$out" 2>/dev/null || echo 0)"
        if [[ "$VERIFY" == "true" ]]; then
            echo "  present — verifying sha256 (--verify)…"
            if echo "$sha256  $out" | sha256sum -c --status; then
                echo "  present and verified — skipping."
                continue
            fi
            echo "  present but sha256 MISMATCH — re-downloading."
        elif [[ "$cur_size" == "$size" ]]; then
            echo "  present with expected size — skipping (pass --verify to re-hash)."
            continue
        else
            echo "  present but size $cur_size != $size — re-downloading."
        fi
    fi

    mkdir -p "$dir" || fail "cannot create $dir — check permissions"

    tmp="$out.part"
    # Bounded: never blocks forever. --connect-timeout caps the handshake;
    # --max-time caps the whole transfer (1 h is generous even for ~2.4 GB on a
    # slow link); --retry rides out transient blips. --fail turns an HTTP 404 /
    # 401 into a non-zero exit instead of saving an HTML error page as "weights".
    if ! curl --fail --location --show-error \
              --connect-timeout 20 --max-time 3600 \
              --retry 3 --retry-delay 5 \
              -o "$tmp" "$url"; then
        rm -f "$tmp"
        fail "download failed for $name.
    URL       : $url
    Mechanism : curl could not retrieve the pinned file (network down, the
                revision was removed, or the repo turned private/gated).
    Action    : check the URL opens in a browser; if the model is now gated,
                accept its terms on Hugging Face and retry with an authenticated
                mirror. If the revision is gone, update the manifest in this
                script (revision + sha256 together) after re-checking the licence."
    fi

    # Verify BEFORE moving into place: a corrupted or tampered blob must never
    # land at the path the read-only mount serves. Same discipline as
    # fetch-gradatum-release.sh ("do NOT install" on a checksum mismatch).
    if ! echo "$sha256  $tmp" | sha256sum -c --status; then
        got="$(sha256sum "$tmp" | cut -d' ' -f1)"
        rm -f "$tmp"
        fail "sha256 MISMATCH for $name — the download does NOT match the pinned hash.
    expected  : $sha256
    got       : $got
    Mechanism : the bytes served differ from the revision this script pins
                (corruption in transit, or the file changed under the same name).
    Action    : retry once (transient corruption). If it recurs, do NOT use the
                file — verify the revision/sha256 in the manifest against the HF
                repo before proceeding."
    fi

    mv -f "$tmp" "$out"
    echo "  downloaded + verified."
    fetched_any=true
done <<< "$MODELS"

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if [[ "$DRY_RUN" == "true" ]]; then
    echo "  Dry-run complete — nothing downloaded."
elif [[ "$fetched_any" == "true" ]]; then
    echo "  Models ready under $DEST/. Next: bash scripts/quickstart-docker.sh"
else
    echo "  Nothing to do — all requested models already present under $DEST/."
fi
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
