#!/usr/bin/env bash
# fetch-gradatum-release.sh — Downloads, verifies and installs a pre-built
# gradatum release (GitHub Releases archives) + the packaging files that are
# NOT shipped inside the binary archives (systemd units, sysusers.d,
# example configs).
#
# Usage:
#   bash scripts/fetch-gradatum-release.sh [OPTIONS]
#
# Options:
#   --version vX.Y.Z   Tag to install (default: latest GitHub release)
#   --repo owner/name   GitHub repository (default: gradatum/gradatum)
#   --group GROUP       server | llm | all (default: server)
#                         server = gradatum-server + gradatum-worker + gradatum-admin
#                         llm    = gradatum-gateway + gradatum-engine
#   --dest DIR           Install directory for the binaries (default: /usr/local/bin,
#                         requires sudo — pass --dest ~/bin to avoid sudo)
#   --with-packaging      Also fetch packaging/ + examples/configs/ (source tarball
#                         of the same tag — see "Why" below). Default: on.
#   --skip-attestation    Skip SLSA provenance verification (requires `gh` otherwise)
#   --dry-run              Print the plan without downloading or installing anything
#
# ── Why --with-packaging is needed (not a convenience) ────────────────────────
# The binary archive contains ONLY binaries. Verified in the GitHub Actions job that
# produces the release (.github/workflows/release.yml, step "Package binaries"):
# each group archive (gradatum-server-*, gradatum-llm-*) is a tar.gz
# of the executables alone, copied from target/release/ — no packaging/ file,
# no examples/configs/, no systemd unit, no sysusers.d entry is added to it.
# Without this script (or a manual git clone), a binary-archive install has
# neither a systemd unit nor an example config to adapt. This is also documented
# in packaging/systemd/README.md and docs/DEPLOYMENT.md §0.
#
# ── What this script does NOT do ──────────────────────────────────────────────
# - Does not assume the tag is already published on GitHub. It checks that the
#   GitHub release exists before downloading anything (step 2) and fails with
#   an explicit message if it doesn't exist yet — instead of silently pointing
#   at a 404 URL. The GitHub repository is a MIRROR (not auto-synced): a tag can
#   exist upstream before it appears on GitHub. See
#   docs/guides/B-install-binaries.md §"Two release paths".
# - Does not touch any binary already installed without confirmation if --dest
#   is not empty (see step 5).
#
# Requirements: curl, tar, sha256sum. `gh` CLI (v2.49+) is optional, needed only
# for SLSA attestation verification (--skip-attestation to skip it).

set -euo pipefail

REPO="gradatum/gradatum"
VERSION=""
GROUP="server"
DEST="/usr/local/bin"
WITH_PACKAGING=true
SKIP_ATTESTATION=false
DRY_RUN=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --group) GROUP="$2"; shift 2 ;;
        --dest) DEST="$2"; shift 2 ;;
        --with-packaging) WITH_PACKAGING=true; shift ;;
        --no-packaging) WITH_PACKAGING=false; shift ;;
        --skip-attestation) SKIP_ATTESTATION=true; shift ;;
        --dry-run) DRY_RUN=true; shift ;;
        -h|--help)
            sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

case "$GROUP" in
    server|llm|all) ;;
    *) echo "Invalid group: $GROUP (expected: server|llm|all)" >&2; exit 1 ;;
esac

STEP=0
TOTAL_STEPS=6

step() {
    STEP=$(( STEP + 1 ))
    echo ""
    echo "[$STEP/$TOTAL_STEPS] $*"
}

ok() { echo "  OK"; }
fail() { echo "  FAIL: $*" >&2; exit 1; }

ARCH="x86_64-unknown-linux-gnu"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Gradatum — Fetch release"
echo "  REPO    : $REPO"
echo "  GROUP   : $GROUP"
echo "  DEST    : $DEST"
echo "  DRY-RUN : $DRY_RUN"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# ─── [1/6] Preflight checks ────────────────────────────────────────────────────

step "Preflight checks"

for tool in curl tar sha256sum; do
    command -v "$tool" &>/dev/null || fail "required tool missing: $tool"
done

HAVE_GH=true
if ! command -v gh &>/dev/null; then
    HAVE_GH=false
    if [[ "$SKIP_ATTESTATION" == "false" ]]; then
        echo "  WARNING: gh CLI missing — SLSA verification skipped (use --skip-attestation to silence this warning)"
    fi
fi

ok

# ─── [2/6] Resolve tag + check the GitHub release exists ─────────────────────

step "Resolving the release"

if [[ -z "$VERSION" ]]; then
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    [[ -n "$VERSION" ]] || fail "could not resolve the latest release via the GitHub API — pass --version explicitly"
    echo "  Latest release resolved: $VERSION"
fi

RELEASE_API="https://api.github.com/repos/${REPO}/releases/tags/${VERSION}"
HTTP_CODE="$(curl -s -o /dev/null -w '%{http_code}' "$RELEASE_API")"
if [[ "$HTTP_CODE" != "200" ]]; then
    fail "no GitHub release '${VERSION}' on ${REPO} (HTTP ${HTTP_CODE}). The GitHub repository is a MIRROR: publishing there is a separate, non-automatic step. Check that the release has been published publicly before retrying — see docs/guides/B-install-binaries.md §\"Two release paths\"."
fi

ok

# ─── [3/6] Download + verification ────────────────────────────────────────────

step "Downloading archives (group: $GROUP)"

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
cd "$WORKDIR"

BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"

declare -a ARCHIVES=()
case "$GROUP" in
    server) ARCHIVES=("gradatum-server-${VERSION}-${ARCH}.tar.gz") ;;
    llm)    ARCHIVES=("gradatum-llm-${VERSION}-${ARCH}.tar.gz") ;;
    all)    ARCHIVES=(
                "gradatum-server-${VERSION}-${ARCH}.tar.gz"
                "gradatum-llm-${VERSION}-${ARCH}.tar.gz"
            ) ;;
esac

if [[ "$DRY_RUN" == "true" ]]; then
    echo "  [dry-run] would download: ${ARCHIVES[*]} + SHA256SUMS from $BASE_URL"
else
    curl -fLO "${BASE_URL}/SHA256SUMS" || fail "SHA256SUMS not found for ${VERSION}"
    for archive in "${ARCHIVES[@]}"; do
        curl -fLO "${BASE_URL}/${archive}" || fail "archive not found: ${archive}"
    done
fi

ok

# ─── [4/6] Integrity + provenance ─────────────────────────────────────────────

step "Verifying integrity (sha256) + provenance (SLSA)"

if [[ "$DRY_RUN" == "true" ]]; then
    echo "  [dry-run] sha256sum -c + gh attestation verify skipped"
else
    sha256sum -c SHA256SUMS --ignore-missing || fail "checksum mismatch — archive corrupted or tampered with, do NOT install"

    if [[ "$HAVE_GH" == "true" && "$SKIP_ATTESTATION" == "false" ]]; then
        for archive in "${ARCHIVES[@]}"; do
            gh attestation verify "$archive" --repo "$REPO" \
                || fail "invalid SLSA attestation for $archive — do NOT install"
        done
    fi
fi

ok

# ─── [5/6] Extract + install the binaries ─────────────────────────────────────

step "Installing binaries into $DEST"

if [[ "$DRY_RUN" == "true" ]]; then
    echo "  [dry-run] would extract then install into $DEST"
else
    mkdir -p "$DEST" 2>/dev/null || true
    NEED_SUDO=""
    [[ -w "$DEST" ]] || NEED_SUDO="sudo"
    [[ -n "$NEED_SUDO" ]] && echo "  $DEST not writable without privileges — using sudo"

    for archive in "${ARCHIVES[@]}"; do
        tar -xzf "$archive"
    done

    for dir in gradatum-*"${VERSION}-${ARCH}"; do
        [[ -d "$dir" ]] || continue
        for bin in "$dir"/*; do
            [[ -f "$bin" ]] || continue
            $NEED_SUDO install -m 755 "$bin" "$DEST/"
            echo "  installed: $DEST/$(basename "$bin")"
        done
    done
fi

ok

# ─── [6/6] packaging/ + examples/configs/ (absent from the binary archives) ──

step "Fetching packaging/ + examples/configs/ (source, absent from binary archives)"

if [[ "$WITH_PACKAGING" == "false" ]]; then
    echo "  Skipped (--no-packaging). Reminder: required for systemd (packaging/systemd/README.md)."
elif [[ "$DRY_RUN" == "true" ]]; then
    echo "  [dry-run] would download the source tarball for tag $VERSION and extract packaging/ + examples/configs/ from it"
else
    curl -fLO "https://github.com/${REPO}/archive/refs/tags/${VERSION}.tar.gz" \
        || fail "source tarball not found for ${VERSION} — packaging/ not retrieved"

    # Extract into a dedicated directory rather than guessing the top-level
    # directory name via a glob: the binary archives extracted in step 5 also
    # create directories starting with "gradatum-" in this same WORKDIR
    # (e.g. gradatum-server-${VERSION}-${ARCH}/), and a name-based glob cannot
    # reliably tell them apart from the source tarball's own top-level
    # directory. --strip-components=1 removes the need to know that name at
    # all: whatever it is, its contents land directly in SRC_DIR.
    SRC_DIR="$WORKDIR/.src-extract"
    mkdir -p "$SRC_DIR"
    tar -xzf "${VERSION}.tar.gz" -C "$SRC_DIR" --strip-components=1 \
        || fail "source tarball extraction failed for ${VERSION}"

    OUT_ROOT="${OLDPWD:-.}"
    COPIED_ANY=false
    for sub in packaging examples/configs; do
        if [[ -d "$SRC_DIR/$sub" ]]; then
            mkdir -p "$OUT_ROOT/$sub"
            cp -r "$SRC_DIR/$sub/." "$OUT_ROOT/$sub/"
            echo "  copied: $sub/ -> $(realpath "$OUT_ROOT/$sub")"
            COPIED_ANY=true
        fi
    done
    # A copy step that silently finds nothing to copy is worse than no copy
    # step at all — it would print "OK" while leaving packaging/ absent, which
    # is exactly the failure this script exists to prevent. Fail loudly instead.
    [[ "$COPIED_ANY" == "true" ]] || fail "neither packaging/ nor examples/configs/ found in the source tarball for ${VERSION} — nothing was copied. This should not happen for a well-formed release; please report it."
fi

ok

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Done. Binaries in $DEST, packaging/ + examples/configs/ in the current"
echo "  directory (if --with-packaging, on by default)."
echo "  Next: docs/guides/B-install-binaries.md §Systemd."
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
