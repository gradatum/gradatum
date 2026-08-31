#!/usr/bin/env bash
# F-257 — éprouvette symétrique (garde F-250) : N == 0 sous patch ET minor doit
# TOUJOURS échouer. La correction F-257 ne doit pas rouvrir le trou que F-250 ferme :
# seul le régime MAJEUR a droit au N==0 (rien à comparer).
#
# Même fixture que le régime majeur (0 checks), mais le rang passé est patch puis
# minor -> la garde doit rendre NON EXECUTE (code 2), jamais un pass.
#
# Preuve exigee : codes de sortie capturés DANS DES FICHIERS, jamais à travers un tube.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
CLASSIFY="${CLASSIFY_SEMVER_OUTPUT:-$ROOT/scripts/internal/classify-semver-output.sh}"
[ -r "$CLASSIFY" ] || { echo "FAIL artefact introuvable : $CLASSIFY"; exit 2; }
FIX="$HERE/fixtures/semver-majeur.out"
[ -f "$FIX" ] || { echo "FAIL fixture absente : $FIX"; exit 2; }

fail=0
for rank in patch minor; do
  bash "$CLASSIFY" gradatum-core "$rank" "$FIX" > "/tmp/vacuite-$rank.out" 2>&1
  echo "$?" > "/tmp/vacuite-$rank.rc"
  RC=$(cat "/tmp/vacuite-$rank.rc")
  if [ "$RC" != "2" ]; then
    echo "FAIL : N==0 sous rang $rank a rendu rc=$RC, attendu 2 (NON EXECUTE, jamais un pass)"
    cat "/tmp/vacuite-$rank.out"
    fail=1
  else
    echo "OK   : N==0 sous rang $rank -> NON EXECUTE (rc=$(cat "/tmp/vacuite-$rank.rc"))"
  fi
done

if [ "$fail" = "0" ]; then
  echo "OK   : le trou F-250 reste ferme — N==0 sous patch/minor échoue toujours"
fi
exit $fail
