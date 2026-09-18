#!/usr/bin/env bash
# Corrective rank: any break blocks. Minor rank: a registered break passes, an unregistered
# one fails. Covers spec §4 (minor rank) and §5 (determinism). The end-to-end run over real
# tool output lives in test-e2e-gate.sh (spec §5.3/§5.4).
#
# The inventory this specimen interrogates is ITS OWN, carried by
# fixtures/inventaire-nomme.yaml and pointed at below: it never reads the release inventory
# (RELEASE-MANIFEST.yaml). That decoupling is the whole point -- while this specimen read
# the release inventory, two entries had to be kept there for its sake alone, so the
# shipped inventory carried test data and the test carried a dependency on release data.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"
M=scripts/internal/check-deviation-match.py
R=scripts/internal/resolve-release-rank.sh
INVENTAIRE="$HERE/fixtures/inventaire-nomme.yaml"
[ -r "$INVENTAIRE" ] || { echo "FAIL fixture inventory unreadable: $INVENTAIRE"; exit 2; }
export SEMVER_INVENTORY_PATH="$INVENTAIRE"
INSCRITE_RENDERED="variant KindKind::Chore"
fail=0

# ── ANTI-VACUITY GUARD ───────────────────────────────────────────────────────
# The "minor + registered break -> passes" assertion below only proves something as long as
# the fixture inventory really NAMES the break it matches. Emptying that inventory does turn
# the specimen red (rc 1) -- but it reads as an unexpected return code, and that is precisely
# how this assertion once went quiet: it kept running while it had ceased to measure. The
# guard names the cause instead, and refuses a fixture that no longer carries the entry the
# assertion is built on.
if ! grep -qF -- "$INSCRITE_RENDERED" "$INVENTAIRE"; then
  echo "FAIL guard: $INVENTAIRE no longer names '$INSCRITE_RENDERED'."
  echo "     The minor-rank assertion below matches that exact rendered break; without the"
  echo "     entry the inventory authorizes nothing and the assertion stops proving anything."
  fail=1
else
  echo "OK   guard : l'inventaire de fixture nomme la rupture mesuree"
fi

t() { # $1=label $2=expected-rc $3..=command
  local lib="$1" att="$2"; shift 2
  "$@" >/dev/null 2>&1; echo "$?" > /tmp/regime-rc.txt
  got=$(cat /tmp/regime-rc.txt)
  if [ "$got" = "$att" ]; then echo "OK   $lib"; else echo "FAIL $lib -> rc=$got attendu $att"; fail=1; fi
}
t "mineur/inscrite passe"      0 python3 "$M" gradatum-core enum_variant_missing "variant KindKind::Chore"
t "mineur/non inscrite echoue" 1 python3 "$M" gradatum-core enum_variant_missing "variant KindKind::Fantome"
t "appariement par symbole"    1 python3 "$M" gradatum-queue enum_variant_missing "variant KindKind::Chore"
t "rang correctif derive"      0 env FAKE_WS_VERSION=2.0.9 FAKE_PUBLISHED=2.0.0 FAKE_EXPECTED_RANK=patch FAKE_TRIGGER=branch FAKE_INTERNAL=internal/2.0.8 bash "$R"
# DETERMINISM: two consecutive runs, same output AND same rc
a=$(python3 "$M" gradatum-core enum_variant_missing "variant KindKind::Fantome" 2>&1; echo "rc=$?")
b=$(python3 "$M" gradatum-core enum_variant_missing "variant KindKind::Fantome" 2>&1; echo "rc=$?")
[ "$a" = "$b" ] && echo "OK   determinisme" || { echo "FAIL determinisme"; fail=1; }
exit $fail
