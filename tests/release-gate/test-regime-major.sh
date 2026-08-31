#!/usr/bin/env bash
# F-257 — éprouvette régime MAJEUR. La chaîne de la garde (anti-vacuite + escalade
# par rang) est exécutée sur une VRAIE sortie de cargo-semver-checks 0.50 sous
# `--release-type major` (fixture capturee) : l'outil ne compare RIEN et rend
# `0 checks: 0 pass, N skip`, rc 0. La garde doit distinguer cet etat de la vacuite,
# ATTEINDRE le `case major` et rendre un RAPPORT — pas un echec.
#
# Avant F-257, ce cas partait dans la branche `gate NOT EXECUTED` (rc_final=1) : un
# jalon majeur peignait le job en rouge permanent. Cette eprouvette jouee en CI
# (job semver-regimes de .forgejo/workflows/ci.yml) empeche la regression de
# redevenir invisible jusqu'a la majeure.
#
# Preuve exigee : code de sortie capturé dans un FICHIER, jamais a travers un tube.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
CLASSIFY="${CLASSIFY_SEMVER_OUTPUT:-$ROOT/scripts/internal/classify-semver-output.sh}"
[ -r "$CLASSIFY" ] || { echo "FAIL artefact introuvable : $CLASSIFY"; exit 2; }
FIX="$HERE/fixtures/semver-majeur.out"
[ -f "$FIX" ] || { echo "FAIL fixture absente : $FIX"; exit 2; }

bash "$CLASSIFY" gradatum-core major "$FIX" > /tmp/majeur.out 2>&1
echo "$?" > /tmp/majeur.rc
RC=$(cat /tmp/majeur.rc)

if [ "$RC" != "0" ]; then
  echo "FAIL : regime majeur N==0 a rendu rc=$RC, attendu 0 (rapport, pas echec)"
  cat /tmp/majeur.out
  exit 1
fi

# La preuve que le `case major` a ete ATTEINT : la ligne RAPPORT est emise. Sans elle,
# le flux serait retombe dans la branche NOT EXECUTED (le defaut F-257).
grep -q '^RAPPORT ' /tmp/majeur.out || {
  echo "FAIL : le case major n'a pas ete atteint — aucune ligne RAPPORT dans la sortie :"
  cat /tmp/majeur.out
  exit 1
}

echo "OK   : regime majeur N==0 -> case major ATTEINT (rc=$(cat /tmp/majeur.rc), RAPPORT rendu, pas d'echec)"
cat /tmp/majeur.out
exit 0
