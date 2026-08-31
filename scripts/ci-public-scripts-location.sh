#!/usr/bin/env bash
#
# ci-public-scripts-location.sh — l'appartenance d'un script au dépôt public est
# une propriété de son EMPLACEMENT, et ce gate la vérifie.
#
#   scripts/            -> outillage produit, PUBLIÉ
#   scripts/internal/   -> outillage opérateur, INTERNE (jamais dans le snapshot public)
#
# POURQUOI CE GATE EXISTE (F-172, 2026-08-18)
#   Avant, l'appartenance était portée par une liste de noms tenue à la main, recopiée en
#   quatre exemplaires sans lien mécanique entre eux. Une liste tenue à la main se tient
#   jusqu'au jour où on l'oublie, et l'oubli était SILENCIEUX : ajouter un script prescrit
#   par la doc publique sans l'inscrire dans la liste ne déclenchait aucune alerte, et la
#   doc publique se mettait à promettre un fichier absent du dépôt publié.
#   Mesuré le 2026-08-18 : la table « Published scripts » d'ARCHITECTURE.md avait déjà
#   divergé — un script supprimé y figurait, trois scripts publiés y manquaient.
#
# LES DEUX SENS, parce qu'un seul laisserait la moitié du trou ouverte :
#   [A] Un script PRESCRIT par une surface publique doit vivre dans l'emplacement publié.
#       C'est le sens que F-172 visait : sans lui, l'oubli reste muet.
#   [B] Un script prescrit UNIQUEMENT par des surfaces internes ne doit pas vivre dans
#       l'emplacement publié. C'est la contrepartie du défaut inversé : depuis F-172 un
#       fichier déposé dans `scripts/` part en public sans que personne l'ait demandé.
#
# CE QUI COMPTE COMME RÉFÉRENCE — et pourquoi ce n'est pas « toute occurrence du chemin ».
#   Un gate qui traiterait toute occurrence de `scripts/x.sh` comme une promesse n'aurait
#   AUCUN pouvoir discriminant : le dépôt est plein de mentions en prose qui ne prescrivent
#   rien (entrées de CHANGELOG, commentaires rustdoc, littéraux de test, et jusqu'à des
#   commentaires de CI qui disent explicitement « absent du snapshot public »). Mesuré le
#   2026-08-18 : 6 chemins `scripts/…` cités depuis une surface publique désignent des
#   fichiers qui n'existent pas — tous en prose ou en littéral de test, aucun cassé pour un
#   utilisateur. Les compter ferait 6 faux positifs et rendrait le gate inutilisable.
#   Une référence n'est donc retenue que si elle est en POSITION D'EXÉCUTION : le chemin
#   est immédiatement précédé de `bash`/`sh`/`source`, ou écrit `./scripts/…`. C'est
#   exactement la forme qu'un lecteur COPIE ET COLLE — donc exactement ce que le dépôt
#   promet de faire résoudre.
#
# UN `scripts/` N'EST PAS L'AUTRE. Le dépôt contient un second répertoire de scripts,
#   `crates/v1-parity-tests/scripts/`, et son README y renvoie par un chemin RELATIF, de la
#   forme `scripts/regenerate-snapshot.sh`. Lu comme un chemin depuis la racine, il désigne
#   un fichier inexistant : première version de ce gate, 1 violation rendue, entièrement
#   fabriquée par l'instrument. Une référence est donc d'abord résolue RELATIVEMENT au
#   répertoire du fichier qui la porte ; si elle y résout, elle vise un autre répertoire et
#   sort du périmètre. Ce n'est qu'à défaut qu'elle est lue depuis la racine.
#
# UN SCRIPT NE SE PRESCRIT PAS LUI-MÊME. Presque tous les scripts impriment leur propre
#   ligne d'usage, du genre « Usage : sudo bash » suivi de son propre chemin. Comme le
#   script vit dans l'emplacement publié, cette auto-citation le faisait compter comme
#   « prescrit publiquement » quoi qu'il arrive : mesuré le 2026-08-18, les 9 scripts
#   publiés étaient à in_pub=1, dont 4 UNIQUEMENT par eux-mêmes. Le contrôle [B] ne
#   pouvait alors échouer pour aucun fichier — vert par construction, pouvoir
#   discriminant nul. Une référence d'un fichier VERS LUI-MÊME est donc ignorée.
#
# DEUX MODES, explicites — jamais un défaut silencieux.
#   Le périmètre interne est lu dans `.forgejo/leak-scan-exclude.paths`. Ce fichier est
#   lui-même interne : dans un clone PUBLIC il est absent. Son absence n'est pas traitée
#   comme « aucune exclusion » par défaut mou, mais bascule le gate en mode CLONE PUBLIC,
#   où TOUT fichier suivi est une surface publique — c'est-à-dire le mode le PLUS STRICT,
#   celui qui convient à un arbre déjà assaini. L'élargissement est annoncé, jamais subi.
#
# Sortie : 0 = conforme, 1 = violation, 2 = gate inutilisable (refus de rendre un vert creux).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PUBLIC_DIR="scripts"
INTERNAL_DIR="scripts/internal"
EXCLUDE_FILE=".forgejo/leak-scan-exclude.paths"

command -v git >/dev/null 2>&1 || { echo "ERREUR: git introuvable — gate inutilisable."; exit 2; }
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || { echo "ERREUR: pas un dépôt git — gate inutilisable."; exit 2; }

mapfile -t TRACKED < <(git ls-files)
[ "${#TRACKED[@]}" -gt 0 ] || { echo "ERREUR: aucun fichier suivi — gate inutilisable."; exit 2; }

# ── Périmètre interne ────────────────────────────────────────────────────────
INTERNAL_PREFIXES=()
if [ -r "$EXCLUDE_FILE" ]; then
  mapfile -t INTERNAL_PREFIXES < <(grep -vE '^[[:space:]]*(#|$)' "$EXCLUDE_FILE" || true)
  [ "${#INTERNAL_PREFIXES[@]}" -gt 0 ] || {
    echo "ERREUR: $EXCLUDE_FILE existe mais ne déclare aucun préfixe."
    echo "        Un périmètre vide rendrait ce gate vert par vacuité — refus."
    exit 2
  }
  MODE="dépôt interne"
  echo "==> Mode : $MODE (${#INTERNAL_PREFIXES[@]} préfixes internes lus dans $EXCLUDE_FILE)"
else
  MODE="clone public"
  echo "==> Mode : $MODE — $EXCLUDE_FILE absent."
  echo "    Tout fichier suivi est donc traité comme surface publique (mode le plus strict)."
fi

is_internal_surface() {   # $1 = chemin repo-relatif
  local f="$1" p
  for p in ${INTERNAL_PREFIXES[@]+"${INTERNAL_PREFIXES[@]}"}; do
    [ "$f" = "$p" ] && return 0
    [ "${f#"$p"/}" != "$f" ] && return 0
  done
  return 1
}

# ── Collecte des prescriptions exécutables ───────────────────────────────────
# Motif : `bash|sh|source` + espaces + (affectations d'env)* + chemin, ou `./chemin`.
EXEC_RX='(^|[^A-Za-z0-9._/~-])((bash|sh|source)[[:space:]]+([A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*[[:space:]]+)*|\./)(scripts/[A-Za-z0-9._+/-]+\.(sh|py))'

PUB_REFS=$(mktemp); INT_REFS=$(mktemp)
trap 'rm -f "$PUB_REFS" "$INT_REFS"' EXIT

for f in "${TRACKED[@]}"; do
  hits=$(grep -hoE "$EXEC_RX" -- "$f" 2>/dev/null | grep -oE 'scripts/[A-Za-z0-9._+/-]+\.(sh|py)' || true)
  [ -n "$hits" ] || continue
  fdir=$(dirname "$f")
  dest="$PUB_REFS"; is_internal_surface "$f" && dest="$INT_REFS"
  while IFS= read -r h; do
    [ -n "$h" ] || continue
    # Résolution relative d'abord : si elle aboutit, la référence vise un AUTRE répertoire
    # de scripts (cf. crates/v1-parity-tests/scripts/) et ne relève pas de ce gate.
    [ "$fdir" != "." ] && [ -f "$fdir/$h" ] && continue
    # Auto-citation (ligne d'usage) : ne prescrit rien à personne.
    [ "$h" = "$f" ] && continue
    printf '%s\t%s\n' "$f" "$h" >> "$dest"
  done <<< "$hits"
done

fail=0

# ── [A] Prescrit publiquement -> doit être dans l'emplacement publié ─────────
echo "==> [A] Chaque script prescrit par une surface publique résout-il dans l'emplacement publié ?"
nA=0
while IFS=$'\t' read -r src path; do
  [ -n "${path:-}" ] || continue
  nA=$((nA+1))
  if [ "${path#"$INTERNAL_DIR"/}" != "$path" ]; then
    echo "  VIOLATION: $src prescrit '$path', qui vit dans l'emplacement INTERNE."
    echo "             Un lecteur du dépôt public ne trouvera pas ce fichier."
    echo "             Geste : déplacer le script sous $PUBLIC_DIR/, ou cesser de le prescrire publiquement."
    fail=1
  elif [ ! -f "$path" ]; then
    echo "  VIOLATION: $src prescrit '$path', qui n'existe pas."
    fail=1
  fi
done < <(sort -u "$PUB_REFS")
echo "    $nA prescription(s) publique(s) examinée(s)."
[ "$nA" -gt 0 ] || { echo "ERREUR: aucune prescription publique détectée — le contrôle [A] serait vide."; echo "        Motif de détection cassé ou dépôt inattendu : refus de rendre un vert creux."; exit 2; }

# ── [B] Prescrit seulement en interne -> ne doit pas être dans l'emplacement publié ──
echo "==> [B] Un script de l'emplacement publié n'est-il prescrit que par des surfaces internes ?"
mapfile -t PUBLISHED < <(git ls-files -- "$PUBLIC_DIR" | grep -vE "^$INTERNAL_DIR/" || true)
# Anti-vacuité : un emplacement publié vide ferait passer [B] pour un succès sans rien tester.
[ "${#PUBLISHED[@]}" -gt 0 ] || { echo "ERREUR: aucun fichier suivi directement sous $PUBLIC_DIR/ — [B] serait vide."; exit 2; }
for s in ${PUBLISHED[@]+"${PUBLISHED[@]}"}; do
  case "$s" in *.sh|*.py) ;; *) continue ;; esac
  in_pub=0; in_int=0
  cut -f2 "$PUB_REFS" 2>/dev/null | grep -qxF "$s" && in_pub=1
  cut -f2 "$INT_REFS" 2>/dev/null | grep -qxF "$s" && in_int=1
  if [ "$in_pub" -eq 0 ] && [ "$in_int" -eq 1 ]; then
    echo "  VIOLATION: '$s' vit dans l'emplacement publié mais n'est prescrit que par des surfaces internes."
    echo "             Il partirait en public sans que rien ne le demande. Geste : déplacer sous $INTERNAL_DIR/."
    fail=1
  elif [ "$in_pub" -eq 0 ] && [ "$in_int" -eq 0 ]; then
    # Indécidable par la mesure, et volontairement NON bloquant : un script sans prescription
    # peut être un outil qu'on invoque à la main. Trancher au jugé produirait des faux positifs.
    echo "  INFO: '$s' n'est prescrit par aucune surface — appartenance non décidable mécaniquement."
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "==> ÉCHEC — l'emplacement et les prescriptions ne concordent pas."
  exit 1
fi
echo "==> OK — emplacement et prescriptions concordent dans les deux sens."
