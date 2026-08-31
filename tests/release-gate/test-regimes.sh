#!/usr/bin/env bash
# Regime correctif : toute rupture bloque. Regime mineur : inscrite passe, non inscrite echoue.
# Couvre spec §4 (regime mineur) et §5 (determinisme). L'end-to-end sur vraie sortie d'outil
# est dans test-e2e-gate.sh (spec §5.3/§5.4).
set -uo pipefail
M=scripts/internal/check-deviation-match.py
R=scripts/internal/resolve-release-rank.sh
fail=0
t() { # $1=libelle $2=rc-attendu $3..=cmd
  local lib="$1" att="$2"; shift 2
  "$@" >/dev/null 2>&1; echo "$?" > /tmp/regime-rc.txt
  got=$(cat /tmp/regime-rc.txt)
  if [ "$got" = "$att" ]; then echo "OK   $lib"; else echo "FAIL $lib -> rc=$got attendu $att"; fail=1; fi
}
t "mineur/inscrite passe"      0 python3 "$M" gradatum-core enum_variant_missing "variant KindKind::Chore"
t "mineur/non inscrite echoue" 1 python3 "$M" gradatum-core enum_variant_missing "variant KindKind::Fantome"
t "appariement par symbole"    1 python3 "$M" gradatum-queue enum_variant_missing "variant KindKind::Chore"
t "rang correctif derive"      0 env FAKE_WS_VERSION=2.0.9 FAKE_PUBLISHED=2.0.0 FAKE_EXPECTED_RANK=patch FAKE_TRIGGER=branch FAKE_INTERNAL=internal/2.0.8 bash "$R"
# DETERMINISME : deux executions consecutives, meme sortie ET meme rc
a=$(python3 "$M" gradatum-core enum_variant_missing "variant KindKind::Fantome" 2>&1; echo "rc=$?")
b=$(python3 "$M" gradatum-core enum_variant_missing "variant KindKind::Fantome" 2>&1; echo "rc=$?")
[ "$a" = "$b" ] && echo "OK   determinisme" || { echo "FAIL determinisme"; fail=1; }
exit $fail
