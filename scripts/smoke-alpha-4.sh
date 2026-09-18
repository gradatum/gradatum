#!/usr/bin/env bash
# Deprecated name — kept as a one-screen shim.
#
# This smoke test was renamed to `scripts/smoke-post-deploy-1.sh`. The old name survives
# here so that operator runbooks, CI call sites and installed copies keep resolving; the
# shim only warns and then execs the new script, forwarding every argument and inheriting
# the environment (SERVER, GRADATUM_API_KEY, API_KEY_FILE, ...).
#
# It is a self-contained script rather than a symlink on purpose: a symlink whose target
# is filtered out of a release snapshot leaves a dangling link behind, silently.
#
# Remove this shim once no reference to the old name remains.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

echo "WARNING: scripts/smoke-alpha-4.sh is deprecated — use scripts/smoke-post-deploy-1.sh" >&2

exec bash "$SCRIPT_DIR/smoke-post-deploy-1.sh" "$@"
