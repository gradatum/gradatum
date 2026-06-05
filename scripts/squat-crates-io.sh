#!/usr/bin/env bash
#
# squat-crates-io.sh — Defensive crates.io name reservation for Gradatum.
#
# Generates 26 placeholder crates at v0.0.0 and (optionally) publishes them
# to crates.io to reserve the names defensively against typosquatting.
#
# Uses an isolated CARGO_HOME so any host-level cargo registry proxy replace-with
# (~/.cargo/config.toml) is bypassed transparently.
#
# Usage:
#   bash scripts/squat-crates-io.sh
#
# The script will:
#   1. Prompt for your crates.io API token (stdin, invisible, never argv)
#   2. Set up a temp CARGO_HOME with no registry override
#   3. Generate 26 placeholder crates in /tmp/squat-*/
#   4. Dry-run validate all 26 against the real crates.io
#   5. If all dry-runs OK, ask you to type "PUBLISH" to confirm
#   6. Publish all 26 sequentially (with rate-limit delay)
#   7. Clean up the temp CARGO_HOME on exit
#
# Idempotent: re-running re-creates /tmp/squat-* dirs from scratch.
# Token never written to disk in $HOME/.cargo/* (isolated CARGO_HOME).
#
# To get an API token:
#   https://crates.io/me → "API Tokens" → "New Token" → scope "publish-new"

set -euo pipefail

# ---------------------------------------------------------------- config ---

NAMES=(
    # 21 workspace members (current Phase 0bis target)
    "gradatum"
    "gradatum-acl-auth"
    "gradatum-acl-policy"
    "gradatum-admin"
    "gradatum-auth"
    "gradatum-cache"
    "gradatum-chat"
    "gradatum-cli"
    "gradatum-core"
    "gradatum-curator"
    "gradatum-embed"
    "gradatum-index"
    "gradatum-markdown"
    "gradatum-mcp-stub"
    "gradatum-queue"
    "gradatum-sdk-rs"
    "gradatum-search"
    "gradatum-server"
    "gradatum-storage"
    "gradatum-vault"
    "gradatum-worker"

    # 5 future-reserved names (defensive)
    # gradatum-engine:   Phase 1+ — all local LLM runtime impls (Chat + Embedder via candle/llama.cpp)
    #                    chat/embed traits stay light (HTTP remote backends only),
    #                    engine implements both traits with shared local stack (Option 2 hexagonal)
    # gradatum-protocol: defensive — D9 removed but name confusion risk
    # gradatum-studio:   Phase 2+ — admin/visualization web UI
    # gradatum-mcp:      Phase 2+ — full MCP impl (vs current mcp-stub proxy)
    # gradatum-distill:  Phase v1.x+ — k-anonymity distillation (PII removal on exports)
    "gradatum-engine"
    "gradatum-protocol"
    "gradatum-studio"
    "gradatum-mcp"
    "gradatum-distill"
)

readonly TMP_ROOT="/tmp"
readonly AUTHOR='Gradatum Maintainers <maintainer@gradatum.org>'
readonly LICENSE="Apache-2.0"
readonly REPO_URL="https://github.com/gradatum/gradatum"
readonly DESCRIPTION_TPL="Reserved name for future Gradatum crate. See ${REPO_URL}."
readonly RATE_LIMIT_SLEEP_S=5

# -------------------------------------------------------- preflight check ---

command -v cargo >/dev/null 2>&1 || {
    echo "ERROR: cargo not found in PATH" >&2
    exit 1
}

echo "==> Defensive crates.io name reservation for Gradatum"
echo "    ${#NAMES[@]} names to validate (21 workspace + 5 future-reserved)."
echo

# ----------------------------------------------- token prompt (invisible) ---

# Accept token from env var (set in current shell to avoid re-prompt on relaunch),
# otherwise prompt via stdin (invisible).
if [[ -n "${CRATES_IO_TOKEN:-}" ]]; then
    TOKEN="$CRATES_IO_TOKEN"
    echo "==> Using \$CRATES_IO_TOKEN from environment (skip prompt)."
else
    read -rsp "Paste your crates.io API token (input hidden), then Enter: " TOKEN
    echo
fi

if [[ -z "${TOKEN:-}" ]]; then
    echo "ERROR: empty token, aborting." >&2
    exit 1
fi

# Sanity: crates.io tokens look like "ciovXXXXXXXXXXXXX" (~32-40 chars)
if [[ ! "$TOKEN" =~ ^cio ]]; then
    echo "WARNING: token does not start with 'cio' — is this really a crates.io token?"
    read -rp "Continue anyway? (yes/no) " confirm
    [[ "$confirm" == "yes" ]] || { echo "Aborted."; exit 1; }
fi

# ---------------------------------------------- isolated CARGO_HOME setup ---

ORIG_CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
ISOLATED_CARGO_HOME="$(mktemp -d -t squat-cargo-XXXXXX)"
export CARGO_HOME="$ISOLATED_CARGO_HOME"

# Cleanup on exit (success or failure)
cleanup() {
    rm -rf "$ISOLATED_CARGO_HOME"
    export CARGO_HOME="$ORIG_CARGO_HOME"
    unset TOKEN
}
trap cleanup EXIT INT TERM

# Empty config.toml = no replace-with override, default crates.io registry
: > "$CARGO_HOME/config.toml"

# Token in credentials.toml under [registry] = default crates-io
{
    echo "[registry]"
    echo "token = \"${TOKEN}\""
} > "$CARGO_HOME/credentials.toml"
chmod 600 "$CARGO_HOME/credentials.toml"
unset TOKEN

echo "==> Isolated CARGO_HOME: $CARGO_HOME (any host registry proxy bypassed)"
echo

# ------------------------------------------------------ generate + check ---

ok_count=0
fail_count=0
fail_names=()

echo "==> Generating placeholder crates + dry-run validating (this checks name availability)"
echo

for name in "${NAMES[@]}"; do
    dir="${TMP_ROOT}/squat-${name}"
    rm -rf "${dir}"
    mkdir -p "${dir}/src"

    cat > "${dir}/Cargo.toml" <<EOF
[package]
name = "${name}"
version = "0.0.0"
edition = "2021"
authors = ["${AUTHOR}"]
license = "${LICENSE}"
description = "${DESCRIPTION_TPL}"
repository = "${REPO_URL}"
readme = "README.md"

[lib]
path = "src/lib.rs"
EOF

    cat > "${dir}/src/lib.rs" <<EOF
//! Reserved. See ${REPO_URL}.
EOF

    cat > "${dir}/README.md" <<EOF
# ${name}

Reserved crate name for the [Gradatum](${REPO_URL}) project.

This v0.0.0 placeholder exists to prevent name-squatting on crates.io.
Real implementation will follow when the corresponding component reaches
public release. See the main repository for status.
EOF

    printf "  [%-25s] " "${name}"
    if (cd "${dir}" && cargo publish --dry-run --allow-dirty --quiet 2>/dev/null); then
        printf "OK\n"
        ok_count=$((ok_count + 1))
    else
        printf "FAIL\n"
        fail_count=$((fail_count + 1))
        fail_names+=("${name}")
    fi
done

# --------------------------------------------------------------- summary ---

echo
echo "==> Dry-run results: ${ok_count} OK / ${fail_count} FAIL out of ${#NAMES[@]}"

if [[ ${fail_count} -gt 0 ]]; then
    echo
    echo "Failed names (likely already taken on crates.io or other validation issue):"
    for n in "${fail_names[@]}"; do
        echo "  - ${n}"
        echo "    Verbose error: (cd ${TMP_ROOT}/squat-${n} && cargo publish --dry-run --allow-dirty 2>&1 | tail -10)"
    done
    echo
    echo "If a name is taken, document the conflict in PROJECT-CONTEXT.md and rename"
    echo "before proceeding. Do NOT attempt to publish over a conflict."
    exit 1
fi

# ---------------------------------------------- confirmation gate publish ---

cat <<'EOF'

==> All 26 names available. Ready to PUBLISH for real.

⚠️  WARNING: cargo publish is IRREVERSIBLE.
    Once published, the names are reserved on crates.io forever.
    Versions can only be 'yanked' (disabled), not deleted.
    The name reservation persists even after yank.

EOF

read -rp "Type exactly 'PUBLISH' to proceed (anything else aborts): " confirm
if [[ "$confirm" != "PUBLISH" ]]; then
    echo "Aborted by user. No publish performed."
    exit 0
fi

# ----------------------------------------------------- publish sequential ---

echo
echo "==> Publishing 26 placeholders (${RATE_LIMIT_SLEEP_S}s between to respect rate limit)..."
echo

publish_ok=0
publish_fail=0
publish_fail_names=()

for name in "${NAMES[@]}"; do
    dir="${TMP_ROOT}/squat-${name}"
    printf "  [%-25s] " "${name}"
    if (cd "${dir}" && cargo publish --allow-dirty 2>&1 | tail -3); then
        publish_ok=$((publish_ok + 1))
    else
        publish_fail=$((publish_fail + 1))
        publish_fail_names+=("${name}")
    fi
    sleep "$RATE_LIMIT_SLEEP_S"
done

echo
echo "==> Publish results: ${publish_ok} OK / ${publish_fail} FAIL out of ${#NAMES[@]}"

if [[ ${publish_fail} -gt 0 ]]; then
    echo
    echo "Failed publishes:"
    for n in "${publish_fail_names[@]}"; do
        echo "  - ${n}"
    done
    exit 1
fi

cat <<EOF

==> All 26 names successfully reserved on crates.io.

Verify on:
  https://crates.io/users/<your-username>
  https://crates.io/search?q=gradatum

Cleanup:
  rm -rf /tmp/squat-gradatum*

(The isolated CARGO_HOME is auto-cleaned by the trap on exit.)
EOF
