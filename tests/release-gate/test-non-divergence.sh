#!/usr/bin/env bash
# F-258 — éprouvette de non-divergence. Un test qui reste vert quoi qu'on change a la
# source ne prouve rien : c'est exactement le defaut que F-258 corrige (le test e2e
# éprouvait SA PROPRE COPIE du triplet et resterait vert si les deux autres divergeaient).
#
# Procede : on fabrique une COPIE CASSEE de l'artefact unique
# scripts/internal/classify-semver-output.sh — le sed de normalisation du `rendered`
# est NEUTRALISE (no-op) — et on verifie que test-e2e-gate.sh ROUGIT dessus. Puis on
# verifie qu'il reverdit sur la source versionnee. L'artefact versionne n'est JAMAIS
# modifie : la copie est jetable et son sha256 est verifie avant/après.
#
# Pourquoi le e2e rouge : la fixture semver-inscrite.out porte des items dont le suffixe
# volatil `<chemin>:<ligne>` doit etre retire par la normalisation. Neutralisee, la
# normalisation laisse ce suffixe dans le rendered, et l'appariement minor contre
# l'inventaire ne peut plus passer.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
ARTIFACT="$ROOT/scripts/internal/classify-semver-output.sh"
[ -r "$ARTIFACT" ] || { echo "FAIL artefact introuvable : $ARTIFACT"; exit 2; }
TMPD="$(mktemp -d)"
trap 'rm -rf "$TMPD"' EXIT
BROKEN="$TMPD/classify-broken.sh"
SHA_BEFORE=$(sha256sum "$ARTIFACT" | cut -d' ' -f1)

# La copie cassee embarque AUSSI check-deviation-match.py (son `$0` pointe vers $TMPD) :
# le seul ecart avec la source est le sed — l'echec du e2e est alors attribuable au
# contrat casser, pas a un helper introuvable.
cp "$ROOT/scripts/internal/check-deviation-match.py" "$TMPD/check-deviation-match.py"

# Copie CASSEE : le programme sed de normalisation est remplace par un no-op. Le motif
# ne nomme aucune forme de suffixe : il vise le programme sed lui-meme, quelle que soit
# sa teneur — l'eprouvette s'adapte a la source sans la copier.
python3 - "$ARTIFACT" > "$BROKEN" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
m = re.search(r"(sed -E ')([^']*)(')", src)
assert m, "sed de normalisation introuvable dans l'artefact — adapter l'eprouvette"
sys.stdout.write(src[:m.start(2)] + "s/^$//" + src[m.end(2):])
PY

# 1) Source CASSEE -> le test e2e doit ROUGIR.
CLASSIFY_SEMVER_OUTPUT="$BROKEN" bash "$HERE/test-e2e-gate.sh" > "$TMPD/e2e-broken.out" 2>&1
echo "$?" > "$TMPD/e2e-broken.rc"
if [ "$(cat "$TMPD/e2e-broken.rc")" = "0" ]; then
  echo "FAIL : le test e2e est reste vert sur une source cassee — il ne consomme pas la source unique"
  cat "$TMPD/e2e-broken.out"
  exit 1
fi
echo "OK   : source CASSEE -> e2e ROUGE (rc=$(cat "$TMPD/e2e-broken.rc"))"
echo "--- sortie du e2e sur source cassee ---"
cat "$TMPD/e2e-broken.out"

# 2) Source VERSIONNEE intacte -> le test e2e doit reverdir.
bash "$HERE/test-e2e-gate.sh" > "$TMPD/e2e-fixed.out" 2>&1
echo "$?" > "$TMPD/e2e-fixed.rc"
if [ "$(cat "$TMPD/e2e-fixed.rc")" != "0" ]; then
  echo "FAIL : source intacte mais e2e rouge (rc=$(cat "$TMPD/e2e-fixed.rc"))"
  cat "$TMPD/e2e-fixed.out"
  exit 1
fi
echo "OK   : source intacte -> e2e VERT (rc=$(cat "$TMPD/e2e-fixed.rc"))"
echo "--- sortie du e2e sur source intacte ---"
cat "$TMPD/e2e-fixed.out"

# 3) Garde finale : la source versionnee est byte-identique (jamais modifiee).
SHA_AFTER=$(sha256sum "$ARTIFACT" | cut -d' ' -f1)
[ "$SHA_BEFORE" = "$SHA_AFTER" ] || { echo "FAIL : l'artefact versionne a ete modifie par l'eprouvette"; exit 1; }
echo "OK   : sha256 de l'artefact inchange (${SHA_BEFORE:0:12}...)"
echo "OK   : non-divergence prouvee — le e2e éprouve la source unique"
exit 0
