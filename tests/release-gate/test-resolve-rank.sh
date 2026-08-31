#!/usr/bin/env bash
# TDD -- resolve-release-rank.sh
# Deux valeurs sur stdout : ligne 1 = rang (patch|minor|major), ligne 2 = argument de
# baseline (--baseline-rev <tag> sur poussee de branche, VIDE sur tag public v*).
# RC : 0 = OK ; 1 = assertion du manifeste != rang derive ; 2 = rang/baseline non derivable.
# Sources autoritaires reelles remplacables par des FAKE_* pour la table de cas.
set -uo pipefail
S="$(cd "$(dirname "$0")/../.." && pwd)/scripts/internal/resolve-release-rank.sh"
fail=0

rank_of() { # $1=ws $2=published $3=expected -> imprime rang, ecrit rc dans /tmp/rank-rc.txt
  local out
  out=$(FAKE_WS_VERSION="$1" FAKE_PUBLISHED="$2" FAKE_EXPECTED_RANK="$3" FAKE_TRIGGER=branch FAKE_INTERNAL="internal/$2" "$S" 2>/dev/null)
  echo "$?" > /tmp/rank-rc.txt
  printf '%s\n' "$out" | sed -n 1p
}
check_rank() { # $1=ws $2=pub $3=attendu (aussi passe comme expected -> accord)
  local got; got=$(rank_of "$1" "$2" "$3")
  if [ "$got" != "$3" ]; then echo "FAIL rang $1 vs $2 -> '$got' attendu '$3'"; fail=1
  else echo "OK   rang $1 vs $2 -> $3"; fi
}
check_rank 2.0.9  2.0.0 patch
check_rank 2.1.0  2.0.0 minor
check_rank 3.0.0  2.0.0 major
check_rank 2.0.1  2.0.0 patch
check_rank 2.10.0 2.9.3 minor

# Desaccord assertion vs derive -> exit 1
FAKE_WS_VERSION=2.0.9 FAKE_PUBLISHED=2.0.0 FAKE_EXPECTED_RANK=minor FAKE_TRIGGER=branch FAKE_INTERNAL=internal/2.0.0 "$S" >/dev/null 2>&1
echo "$?" > /tmp/rank-rc.txt
if [ "$(cat /tmp/rank-rc.txt)" = "1" ]; then echo "OK   desaccord -> exit 1"; else echo "FAIL desaccord non detecte (rc=$(cat /tmp/rank-rc.txt))"; fail=1; fi

# Baseline contextuelle -- poussee de branche -> dernier internal/*
b_branch=$(FAKE_WS_VERSION=2.0.9 FAKE_PUBLISHED=2.0.0 FAKE_EXPECTED_RANK=patch FAKE_TRIGGER=branch FAKE_INTERNAL=internal/2.0.8 "$S" 2>/dev/null | sed -n 2p)
if [ "$b_branch" = "--baseline-rev internal/2.0.8" ]; then echo "OK   baseline branche -> $b_branch"; else echo "FAIL baseline branche -> '$b_branch'"; fail=1; fi

# Baseline contextuelle -- poussee tag v* -> VIDE (registre publie resolu par l'outil)
b_tag=$(FAKE_WS_VERSION=2.1.0 FAKE_PUBLISHED=2.0.0 FAKE_EXPECTED_RANK=minor FAKE_TRIGGER=tag "$S" 2>/dev/null | sed -n 2p)
if [ -z "$b_tag" ]; then echo "OK   baseline tag public -> (vide)"; else echo "FAIL baseline tag public -> '$b_tag' (devrait etre vide)"; fail=1; fi

# Baseline non derivable en branche (aucun internal/*) -> exit 2
FAKE_WS_VERSION=2.0.9 FAKE_PUBLISHED=2.0.0 FAKE_EXPECTED_RANK=patch FAKE_TRIGGER=branch FAKE_INTERNAL="" FAKE_NO_INTERNAL=1 "$S" >/dev/null 2>&1
echo "$?" > /tmp/rank-rc.txt
if [ "$(cat /tmp/rank-rc.txt)" = "2" ]; then echo "OK   baseline branche absente -> exit 2"; else echo "FAIL internal/* absent (rc=$(cat /tmp/rank-rc.txt))"; fail=1; fi

exit $fail
