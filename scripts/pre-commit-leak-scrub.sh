#!/usr/bin/env bash
#
# pre-commit-leak-scrub.sh — Detect homelab/personal patterns in staged files.
#
# Usage:
#   bash scripts/pre-commit-leak-scrub.sh                  # scan whole repo
#   bash scripts/pre-commit-leak-scrub.sh --staged-only    # scan staged files only (pre-commit hook mode)
#
# Patterns are kept in sync with .forgejo/workflows/ci.yml leak-detector job.
# If you need to legitimately allow a pattern, document the exception in
# CONTRIBUTING.md and update both this script and the CI job.
#
# Allow-list (not in PATTERNS — legitimate public references):
#   obsidian-memory-vaults: predecessor project name (public)

set -euo pipefail

# Synced with ci.yml leak-detector PATTERNS — 2026-06-05 OSS-flip round 2: added vault-mem/Monarch/nexus
PATTERNS='\bst[ée]phane\b|\bmotreffs?\b|\bmymomot\b|\blxc-[0-9]{3,4}\b|\bLXC[ -]?[0-9]{2,4}\b|\bmaintainer-org\b|\bJarvis\b|\bBigBrother\b|\bHubMQ\b|\bkellnr(-proxy)?\b|\bforgejo\.lab\.|\bGMKTEC\b|\bKnox\b|\bAuditeur\b|\bLyra\b|\bSecurityAuditor\b|\bArchiviste\b|192\.168\.|10\.77\.|\bllmcore\b|\bProxmox\b|\bTrueNAS\b|\bWazuh\b|\bAuthentik\b|\bAdGuard\b|\bMinisForum\b|Evo X-2|\bRadeon\b|\bZeroClaw\b|vault-mem|\bMonarch\b|\bnexus\b'

mode="${1:-full}"

if [[ "$mode" == "--staged-only" ]]; then
    files=$(git diff --cached --name-only --diff-filter=ACM | grep -E '\.(md|rs|toml|yml|yaml|sh|sql|txt)$' || true)
    [[ -z "$files" ]] && { echo "✓ No staged files to scan"; exit 0; }
    # Exclude this script itself and the CI job from the scan
    files=$(echo "$files" | grep -vE '^scripts/pre-commit-leak-scrub\.sh$|^\.forgejo/workflows/ci\.yml$' || true)
    [[ -z "$files" ]] && { echo "✓ No staged files to scan (after exclusions)"; exit 0; }
    if echo "$files" | xargs -r grep -EnH "$PATTERNS" 2>/dev/null | grep -vi 'bob martin'; then
        echo "✗ Leak detected in staged files — refusing commit"
        exit 1
    fi
else
    # SYNC: --include list must match .forgejo/workflows/ci.yml leak-detector job exactly.
    # Round 3 additions 2026-06-03: added *.sql and *.txt (gap: 0001_phase1.sql + curator-classifier-v1.txt).
    if grep -rEn "$PATTERNS" \
        --include="*.md" --include="*.rs" --include="*.toml" \
        --include="*.yml" --include="*.yaml" --include="*.sh" \
        --include="*.sql" --include="*.txt" \
        --exclude-dir=target --exclude-dir=.git --exclude-dir=node_modules \
        --exclude-dir=.worktrees \
        --exclude="pre-commit-leak-scrub.sh" \
        --exclude="ci.yml" \
        . | grep -vi 'bob martin'; then
        echo "✗ Leak detected — see grep output above"
        exit 1
    fi
fi

echo "✓ No homelab/personal leaks detected"
