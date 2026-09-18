#!/usr/bin/env bash
#
# fetch-upstream.sh — acquire the two third-party inputs of the privacy sidecar
# at pinned revisions, outside the repository.
#
#   bash tools/privacy-sidecar/fetch-upstream.sh [BUILD_DIR]
#
# Fetched, never vendored:
#   privacy-filter.cpp   the GGML engine behind the C API the sidecar calls,
#                        cloned with its ggml submodule at a pinned revision
#   nlohmann/json        single-header JSON used on the wire, pinned by content
#
# BUILD_DIR defaults to $PF_SIDECAR_BUILD_DIR, or $HOME/.cache/gradatum-privacy-sidecar.
# Layout under it:
#   third_party/privacy-filter.cpp/       clone at the pinned revision
#   third_party/include/nlohmann/json.hpp pinned single header
#   build/                            CMake output (created by the caller)
#
# The script verifies every revision and every digest it pins and exits non-zero
# on any divergence. It never writes inside the repository.
#
# Exit codes: 0 all inputs present and verified; 1 a pinned revision or digest
# diverged, or a fetch failed; 2 usage/precondition error (no git, bad path).
set -euo pipefail

# ── Pins ───────────────────────────────────────────────────────────────────
# Changing any of these is a deliberate act: the digests reported by the sidecar
# and the spans it emits are tied to the revisions below.
UPSTREAM_URL="https://github.com/localai-org/privacy-filter.cpp"
UPSTREAM_SHA="735a6c28607ee82afc3a670383f41b55266a3b9a"
GGML_SHA="3af5f5760e19a96427f5f7a93b79cbdf3d4b265b"
JSON_VERSION="v3.11.3"
JSON_URL="https://github.com/nlohmann/json/releases/download/${JSON_VERSION}/json.hpp"
JSON_SHA256="9bea4c8066ef4a1c206b2be5a36302f8926f7fdc6087af5d20b417d0cf103ea6"

usage() {
    sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

BUILD_DIR="${PF_SIDECAR_BUILD_DIR:-$HOME/.cache/gradatum-privacy-sidecar}"
while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        -*) echo "fetch-upstream.sh: unknown option: $1" >&2; usage >&2; exit 2 ;;
        *) BUILD_DIR="$1"; shift ;;
    esac
done

command -v git >/dev/null 2>&1 || { echo "ERREUR : git est requis." >&2; exit 2; }
case "$BUILD_DIR" in
    /*) ;;
    *) BUILD_DIR="$PWD/$BUILD_DIR" ;;
esac

REPO_DIR="$BUILD_DIR/third_party/privacy-filter.cpp"
JSON_DIR="$BUILD_DIR/third_party/include"
mkdir -p "$BUILD_DIR/third_party" "$JSON_DIR/nlohmann"

# ── 1. Engine checkout, pinned revision + pinned submodule ────────────────
if [ -d "$REPO_DIR/.git" ]; then
    echo "==> Mise à jour du clone existant : $REPO_DIR"
    git -C "$REPO_DIR" fetch --quiet origin
else
    echo "==> Clone : $UPSTREAM_URL"
    git clone --quiet "$UPSTREAM_URL" "$REPO_DIR"
fi
git -C "$REPO_DIR" checkout --quiet "$UPSTREAM_SHA"
git -C "$REPO_DIR" submodule update --init --recursive --quiet

have_upstream="$(git -C "$REPO_DIR" rev-parse HEAD)"
if [ "$have_upstream" != "$UPSTREAM_SHA" ]; then
    echo "ERREUR : révision divergente — épinglée $UPSTREAM_SHA, obtenue $have_upstream" >&2
    exit 1
fi
if [ ! -d "$REPO_DIR/ggml/.git" ] && [ ! -f "$REPO_DIR/ggml/.git" ]; then
    echo "ERREUR : le sous-module ggml est absent de $REPO_DIR" >&2
    exit 1
fi
have_ggml="$(git -C "$REPO_DIR/ggml" rev-parse HEAD)"
if [ "$have_ggml" != "$GGML_SHA" ]; then
    echo "ERREUR : sous-module ggml divergent — épinglé $GGML_SHA, obtenu $have_ggml" >&2
    exit 1
fi
echo "    privacy-filter.cpp @ $have_upstream"
echo "    ggml               @ $have_ggml"

# ── 2. JSON header, pinned by content ─────────────────────────────────────
# Verified by digest, not by URL: a tag can be moved, a digest cannot.
JSON_PATH="$JSON_DIR/nlohmann/json.hpp"
json_ok=0
if [ -f "$JSON_PATH" ]; then
    have_json="$(sha256sum "$JSON_PATH" | cut -d' ' -f1)"
    [ "$have_json" = "$JSON_SHA256" ] && json_ok=1
fi
if [ "$json_ok" -eq 1 ]; then
    echo "==> En-tête JSON déjà présent et conforme : $JSON_PATH"
else
    echo "==> Téléchargement : nlohmann/json $JSON_VERSION"
    tmp="$(mktemp "$JSON_DIR/nlohmann/.json.hpp.XXXXXX")"
    trap 'rm -f "$tmp"' EXIT
    curl --fail --silent --show-error --location --max-time 300 -o "$tmp" "$JSON_URL"
    have_json="$(sha256sum "$tmp" | cut -d' ' -f1)"
    if [ "$have_json" != "$JSON_SHA256" ]; then
        echo "ERREUR : empreinte divergente — épinglée $JSON_SHA256, obtenue $have_json" >&2
        exit 1
    fi
    mv "$tmp" "$JSON_PATH"
    trap - EXIT
    # mktemp creates 0600; the header is a build input, not a secret.
    chmod 0644 "$JSON_PATH"
fi
echo "    json.hpp           @ sha256:$JSON_SHA256"

# ── 3. Hand over the two paths CMake needs ────────────────────────────────
cat <<EOF

Entrées vérifiées. Configuration :

  cmake -S tools/privacy-sidecar -B "$BUILD_DIR/build" \\
        -DPF_UPSTREAM_DIR="$REPO_DIR" \\
        -DPF_JSON_INCLUDE_DIR="$JSON_DIR"
  cmake --build "$BUILD_DIR/build" --target pf-sidecar pf-ref-cli
EOF
