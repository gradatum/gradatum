#!/usr/bin/env bash
# Smoke test alpha.7 — re-run alpha-6 + check absence WARN deprecated db_path.
# Pré-requis : server.toml MAJ avec vault_index_path = "..." (au lieu de db_path).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "=== alpha.7 — Phase 1 : re-run alpha.6 ==="
sudo bash "$SCRIPT_DIR/smoke-alpha-6.sh"

echo "=== alpha.7 — Phase 2 : check absence WARN deprecated db_path ==="
WARN=$(sudo journalctl -u gradatum-server --since "5 min ago" --no-pager 2>/dev/null | grep -c "db_path is deprecated" || true)
if [ "$WARN" = "0" ]; then
    echo "PASS — pas de WARN deprecated (config server.toml utilise vault_index_path)"
else
    echo "FAIL — $WARN WARN trouvés. server.toml utilise encore db_path. À migrer."
    exit 1
fi

echo "=== alpha.7 — Phase 3 : check version handler /health ==="
HEALTH=$(curl -sS http://localhost:19090/health --max-time 2)
VERSION=$(echo "$HEALTH" | jq -r '.version // "unknown"')
echo "Health version: $VERSION"
echo "$VERSION" | grep -qE "^0\.1\.0-alpha\.7" && echo "PASS version alpha.7" || echo "WARN version mismatch (got $VERSION)"

echo "=== smoke-alpha-7.sh : DONE ==="
