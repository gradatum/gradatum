#!/bin/bash
# B10 — Binary size baseline via cargo build --release + cargo-bloat (P2)
#
# Produit docs/bench/b10_binary_size.txt avec :
# - Taille du répertoire target/release/ (artefacts workspace)
# - Décomposition par crate via cargo-bloat (si installé)
#
# Usage :
#   ./b10_binary_size.sh
#
# Installation cargo-bloat (si absent) :
#   cargo install cargo-bloat

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

OUTPUT_DIR="${PROJECT_ROOT}/docs/bench"
OUTPUT_FILE="${OUTPUT_DIR}/b10_binary_size.txt"

mkdir -p "${OUTPUT_DIR}"

cd "${PROJECT_ROOT}"

echo "B10 — Binary size baseline ($(date -u +%Y-%m-%dT%H:%M:%SZ))"
echo "Projet : ${PROJECT_ROOT}"
echo "Output : ${OUTPUT_FILE}"

{
    echo "# B10 — Binary size baseline"
    echo ""
    echo "Generated : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "Git commit : $(git rev-parse --short HEAD 2>/dev/null || echo 'unknown')"
    echo ""
    echo "## Build release workspace"
    echo ""
    echo "Command : cargo build --release --workspace"
    echo ""

    # Build release — capture stderr (progress) séparément.
    if cargo build --release --workspace 2>&1; then
        echo "Build : SUCCESS"
    else
        echo "Build : FAILED"
        exit 1
    fi

    echo ""
    echo "## Taille target/release/"
    echo ""
    du -sh target/release/ 2>/dev/null || echo "(target/release/ absent)"

    echo ""
    echo "## Binaires principaux"
    echo ""
    for bin in target/release/gradatum-server target/release/gradatum-worker target/release/gradatum-cli target/release/gradatum-admin; do
        if [[ -f "${bin}" ]]; then
            size=$(stat -c '%s' "${bin}" 2>/dev/null || echo "?")
            size_human=$(du -sh "${bin}" 2>/dev/null | cut -f1 || echo "?")
            echo "  $(basename "${bin}") : ${size_human} (${size} bytes)"
        fi
    done

    echo ""
    echo "## cargo-bloat (top 30 crates par contribution)"
    echo ""
    if command -v cargo-bloat &>/dev/null || cargo bloat --version &>/dev/null 2>&1; then
        cargo bloat --release --crates -n 30 2>&1 | head -70 || \
            echo "(cargo-bloat disponible mais échec — vérifier les binaires)"
    else
        echo "cargo-bloat non installé."
        echo "Pour installer : cargo install cargo-bloat"
        echo "Pour ré-exécuter : ./crates/gradatum-bench/scripts/b10_binary_size.sh"
    fi

} | tee "${OUTPUT_FILE}"

echo ""
echo "Résultats écrits dans : ${OUTPUT_FILE}"
