#!/usr/bin/env bash
# END-TO-END specimen (blocking). REAL cargo-semver-checks 0.50 output (captured fixtures)
# goes through the REAL classification path of the semver job, then is classified per rank.
# Covers spec §5.3 and §5.4.
#
# What it proves: the triplet extraction (`Failed in:` block) + the strip of the volatile
# <path>:<line> suffix + the (crate, lint, rendered) match yields the RIGHT verdict on the
# tool's REAL output -- not on a re-injected inventory string.
#
# The classification is DELEGATED to the single artefact
# scripts/internal/classify-semver-output.sh -- the source also consumed by the semver job
# of .forgejo/workflows/ci.yml and by the G8 gate of release-readiness-scan.sh. This test no
# longer has a copy of its own: it exercises the code the CI actually runs. The
# non-divergence specimen (test-non-divergence.sh) proves it by breaking the artefact and
# asserting that this test turns RED.
#
# INVENTORY: this specimen brings ITS OWN, fixtures/inventaire-nomme.yaml, and points the
# classifier at it through SEMVER_INVENTORY_PATH. It never reads the release inventory
# (RELEASE-MANIFEST.yaml): an entry kept there for a test would make the shipped inventory
# depend on the harness, and the harness on the shipped inventory, each hiding the other's
# degradation.
#
# Fixtures (tests/release-gate/fixtures/), real output vs internal/2.0.8, 2026-08-23:
#   semver-inscrite.out     : KindKind::Chore + ::Spike RENAMED -> 2 enum_variant_missing
#                             breaks, both NAMED by the fixture inventory (positions
#                             preserved: no discriminant shift).
#   semver-non-inscrite.out : CONSTRUCTED FIXTURE (its header carries the detail). Chore +
#                             Spike REMOVED -> 2 enum_variant_missing breaks NAMED by the
#                             fixture inventory, PLUS a third
#                             enum_no_repr_variant_discriminant_changed break carrying a
#                             SYNTHETIC symbol, `__FixtureNeverInventoried::Synthetic`,
#                             which belongs to no crate and can therefore never be
#                             registered: under a minor rank, one unregistered break
#                             blocks. It used to cite `variant KindKind::Task 5 -> 3` until
#                             a release lot legitimately registered it in the manifest --
#                             the break then matched, the verdict turned PASS, and the
#                             specimen went QUIET. The guard below closes that recurrence.
#   semver-majeur.out       : real output under `--release-type major` (2026-08-26) -- the
#                             tool compares nothing: `0 checks: 0 pass, N skip`. Under
#                             major, that N==0 is EXPECTED (REPORT, not a failure); under
#                             patch/minor it is a vacuity (NOT EXECUTED, failure).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"
CLASSIFY="${CLASSIFY_SEMVER_OUTPUT:-$ROOT/scripts/internal/classify-semver-output.sh}"
[ -r "$CLASSIFY" ] || { echo "FAIL artefact introuvable : $CLASSIFY"; exit 2; }
# The inventory interrogated by the delegated classifier (and by the matcher it spawns) is
# this specimen's own fixture, exported so the whole chain reads it and nothing else.
INVENTAIRE="$HERE/fixtures/inventaire-nomme.yaml"
[ -r "$INVENTAIRE" ] || { echo "FAIL fixture inventory unreadable: $INVENTAIRE"; exit 2; }
export SEMVER_INVENTORY_PATH="$INVENTAIRE"
fail=0

# Classify one `cargo semver-checks check-release` output for a given rank, DELEGATING to the
# single artefact. Returns 0 if the gate PASSES, non-zero if it BLOCKS or is NOT EXECUTED --
# the same rules as the body of the semver job (ci.yml).
classer() { # $1=crate $2=rang $3=fichier-sortie
  bash "$CLASSIFY" "$1" "$2" "$3"; return $?
}

t() { # $1=libelle $2=attendu(PASS|BLOCK) $3=crate $4=rang $5=fichier
  classer "$3" "$4" "$5"; local rc=$? got=BLOCK
  [ "$rc" = "0" ] && got=PASS
  if [ "$got" = "$2" ]; then echo "OK   $1 -> $got"; else echo "FAIL $1 -> $got attendu $2"; fail=1; fi
}

INS="$HERE/fixtures/semver-inscrite.out"
NON="$HERE/fixtures/semver-non-inscrite.out"
MAJ="$HERE/fixtures/semver-majeur.out"
[ -f "$INS" ] && [ -f "$NON" ] && [ -f "$MAJ" ] || { echo "FAIL fixtures absentes ($INS / $NON / $MAJ)"; exit 2; }

# ── MECHANICAL GUARD — the MINOR-rank evidence cannot go quiet unnoticed ─────
# The assertion "minor + UNREGISTERED break -> BLOCK" holds only if the fixture carries a
# break NOBODY can register. It already stopped proving anything once: the fixture cited
# `variant KindKind::Task 5 -> 3`, a REAL break, which a release lot legitimately registered
# in the manifest. It then matched, the verdict turned PASS, and NOTHING BROKE VISIBLY. The
# fixture's own invariant comment prevented nothing: nobody edits the manifest while reading
# the fixtures. Hence a guard, biting in BOTH directions of degradation:
#   (a) the synthetic symbol was replaced by a real one  -> the marker disappears;
#   (b) the synthetic symbol was registered in the inventory -> it matches.
# The guard reads the SAME inventory the classifier reads (SEMVER_INVENTORY_PATH, set
# above), never the shipped one: a guard checking a different surface than the one being
# classified would measure something other than what runs.
# The `rendered` is DERIVED from the fixture with the same strip as the classifier, never
# rewritten by hand: same reason.
MARQUEUR='__FixtureNeverInventoried'
SYNTH=$(sed -nE "s/^  (.*${MARQUEUR}.*) in +\/[^ ]*:[0-9]+$/\1/p" "$NON")
if [ -z "$SYNTH" ]; then
  echo "FAIL garde : $NON ne porte plus de rupture marquee '$MARQUEUR'."
  echo "     Le regime mineur exige un symbole SYNTHETIQUE, inscriptible par personne."
  echo "     Toute rupture REELLE finit inscrite a l'inventaire et rend l'epreuve muette."
  fail=1
elif grep -qF -- "$SYNTH" "$INVENTAIRE"; then
  echo "FAIL garde : la rupture synthetique de la fixture est INSCRITE a l'inventaire."
  echo "     rendered = $SYNTH"
  echo "     Elle s'apparie donc, le classificateur rend PASS, et l'assertion du regime"
  echo "     mineur ne prouve plus rien. Retirer cette entree de $INVENTAIRE."
  fail=1
else
  echo "OK   garde : rupture de fixture synthetique et absente de l'inventaire"
fi

t "correctif + rupture -> bloque"            BLOCK gradatum-core patch "$INS"
t "mineur + ruptures INSCRITES -> passe"     PASS  gradatum-core minor "$INS"
t "mineur + rupture NON INSCRITE -> bloque"  BLOCK gradatum-core minor "$NON"
t "majeur + N==0 -> RAPPORT, pas echec"      PASS  gradatum-core major "$MAJ"
t "patch + N==0 -> NON EXECUTE (echoue)"     BLOCK gradatum-core patch "$MAJ"
t "minor + N==0 -> NON EXECUTE (echoue)"     BLOCK gradatum-core minor "$MAJ"

exit $fail
