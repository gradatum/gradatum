#!/usr/bin/env bash
# Tache 5 bis -- eprouvette END-TO-END (bloquante). De VRAIES sorties de
# cargo-semver-checks 0.50 (fixtures capturees) passent par le VRAI chemin de classement
# du job semver, puis sont classees par regime. Couvre spec §5.3 et §5.4.
#
# Ce qui est prouve : le triplet extraction (bloc `Failed in:`) + strip du suffixe volatil
# <chemin>:<ligne> + appariement (crate,lint,rendered) rend le BON verdict sur la sortie
# REELLE de l'outil -- pas sur une chaine d'inventaire reinjectee. C'est la faille P0-1.
#
# F-258 : le classement est DELEGUE a l'artefact UNIQUE
# scripts/internal/classify-semver-output.sh — la source consommee aussi par le job
# semver de .forgejo/workflows/ci.yml et par la gate G8 de release-readiness-scan.sh.
# Ce test n'a plus SA PROPRE COPIE : il eprouve le code que la CI execute reellement.
# L'eprouvette de non-divergence (test-non-divergence.sh) le prouve en cassant l'artefact
# et en constatant que ce test ROUGIT.
#
# Fixtures (tests/release-gate/fixtures/), sorties reelles vs internal/2.0.8, 2026-08-23 :
#   semver-inscrite.out     : KindKind::Chore + ::Spike RENOMMES -> 2 ruptures
#                             enum_variant_missing, toutes deux INSCRITES a l'inventaire
#                             (positions preservees : aucun decalage de discriminant).
#   semver-non-inscrite.out : FIXTURE CONSTRUITE (son en-tete porte le detail). Chore +
#                             Spike RETIRES -> 2 ruptures enum_variant_missing INSCRITES,
#                             PLUS une 3e rupture enum_no_repr_variant_discriminant_changed
#                             portant un symbole SYNTHETIQUE
#                             `__FixtureNeverInventoried::Synthetic`, qui n'appartient a
#                             aucune crate et ne peut donc jamais etre inscrit : sous un
#                             mineur, une seule non-inscrite bloque. Elle citait
#                             `variant KindKind::Task 5 -> 3` jusqu'a ce que le lot 2.1.0
#                             l'inscrive legitimement au manifeste (F-220) — l'appariement
#                             a alors reussi, le verdict est passe a PASS et l'epreuve
#                             s'est videe EN SILENCE. La garde ci-dessous ferme la recidive.
#   semver-majeur.out       : sortie reelle sous `--release-type major` (2026-08-26) —
#                             l'outil ne compare rien : `0 checks: 0 pass, N skip`.
#                             F-257 : sous major ce N==0 est ATTENDU (RAPPORT, pas echec) ;
#                             sous patch/minor c'est une vacuite (NON EXECUTE, echec).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"
CLASSIFY="${CLASSIFY_SEMVER_OUTPUT:-$ROOT/scripts/internal/classify-semver-output.sh}"
[ -r "$CLASSIFY" ] || { echo "FAIL artefact introuvable : $CLASSIFY"; exit 2; }
fail=0

# Classe la sortie d'un `cargo semver-checks check-release` pour un rang donne, EN
# DELEGUANT a l'artefact unique. Rend 0 si le gate PASSE, non-zero s'il BLOQUE ou est
# NON EXECUTE — les memes regles que le corps du job semver (ci.yml).
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

# ── GARDE MECANIQUE — l'epreuve du regime MINEUR ne peut pas se vider en silence ──
# L'assertion « mineur + rupture NON INSCRITE -> BLOCK » ne vaut que si la fixture porte
# une rupture que PERSONNE ne peut inscrire a l'inventaire. Elle a deja cesse de prouver
# quoi que ce soit une fois : la fixture citait `variant KindKind::Task 5 -> 3`, une
# rupture REELLE, que le lot 2.1.0 a legitimement inscrite au manifeste (F-220). Elle
# s'est alors appariee, le verdict est devenu PASS, et RIEN N'A CASSE VISIBLEMENT.
# Le commentaire d'invariant de la fixture n'a rien empeche : on n'edite pas le manifeste
# en lisant les fixtures. D'ou une garde, qui mord dans les DEUX sens de la degradation :
#   (a) le symbole synthetique a ete remplace par un symbole reel -> le marqueur disparait ;
#   (b) le symbole synthetique a ete inscrit au manifeste       -> il s'apparie.
# Le `rendered` est DERIVE de la fixture avec le meme strip que le classificateur, jamais
# reecrit en dur : une garde qui verifie une chaine differente de celle qui est classee
# mesurerait autre chose que ce qui tourne.
MARQUEUR='__FixtureNeverInventoried'
SYNTH=$(sed -nE "s/^  (.*${MARQUEUR}.*) in +\/[^ ]*:[0-9]+$/\1/p" "$NON")
if [ -z "$SYNTH" ]; then
  echo "FAIL garde : $NON ne porte plus de rupture marquee '$MARQUEUR'."
  echo "     Le regime mineur exige un symbole SYNTHETIQUE, inscriptible par personne."
  echo "     Toute rupture REELLE finit inscrite au manifeste et rend l'epreuve muette."
  fail=1
elif grep -qF -- "$SYNTH" RELEASE-MANIFEST.yaml; then
  echo "FAIL garde : la rupture synthetique de la fixture est INSCRITE au manifeste."
  echo "     rendered = $SYNTH"
  echo "     Elle s'apparie donc, le classificateur rend PASS, et l'assertion du regime"
  echo "     mineur ne prouve plus rien. Retirer cette entree de RELEASE-MANIFEST.yaml."
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
