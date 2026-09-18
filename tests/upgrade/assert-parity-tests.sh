#!/usr/bin/env bash
#
# assert-parity-tests.sh — self-test of `assert-parity.sh`'s export key (e).
#
# HERMETIC BY CONSTRUCTION: the two relevés are assembled here, in a temporary
# directory, from literal content. No binary, no port, no server, no state
# produced by `gen-fixture.sh` — so this file is replayable anywhere, and it
# measures `assert-parity.sh` alone.
#
# The comparison of two relevés is exercised through its real entry point (the
# script's command line, its exit code, and its printed report). The report is
# read as text, exactly as an operator reads it: a case passes when the exit code
# AND the named tokens are both what the rule promises.
#
# Why the rule under test is not "byte for byte":
#
#   The export is a REPORT. Between 2.4.0 and 2.5.0 its dialect gains keys — that
#   is the additive evolution the product chose, and a byte comparison would call
#   it a divergence on any populated state while calling two empty exports equal.
#   The rule kept is therefore:
#
#     AVANT → MONTÉ   every entry of the BEFORE export equals the MOUNTED entry
#                     projected onto the BEFORE dialect (same card order, same
#                     count); keys NEW on the MOUNTED side are reported by name
#                     (`cles_ajoutees=…`) and must be documented in the upgrade
#                     guide; a BEFORE key that DISAPPEARS on the MOUNTED side, or
#                     whose value CHANGES, is a divergence.
#     AVANT → RETOUR  byte for byte (the rollback runs the 2.4.0 binary again:
#                     nothing may move at all).
#
# Codes de retour :
#   0  tous les cas sont conformes
#   1  au moins un cas est en écart (le cas est nommé)
#   2  NON EXÉCUTÉ — outil requis absent (jamais un vert)

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
readonly PARITY="$SCRIPT_DIR/assert-parity.sh"

for tool in jq sha256sum cmp; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'assert-parity-tests: NON EXÉCUTÉ — outil requis absent : %s\n' "$tool" >&2
    exit 2
  }
done
[[ -f "$PARITY" ]] || { printf 'assert-parity-tests: NON EXÉCUTÉ — %s absent\n' "$PARITY" >&2; exit 2; }

WORK="$(mktemp -d)"
readonly WORK
trap 'rm -rf -- "$WORK"' EXIT

PASSED=0
FAILED=0

# ── Le relevé minimal ────────────────────────────────────────────────────────
#
# Toutes les clés (a)–(g) sont présentes pour que `assert-parity.sh` compare au
# lieu de rendre `2` (un relevé absent est une NON-EXÉCUTION). Elles sont
# IDENTIQUES des deux côtés : seul l'export est varié, et il ne doit y avoir
# qu'un écart à la fois.
make_releve() {  # $1 = répertoire, $2 = fichier export-features.json
  local dir="$1" export="$2"
  mkdir -p -- "$dir"
  {
    printf 'version\tdescription\tinstalled_on\tchecksum\tsuccess\n'
    printf '1\tinit\t2026-01-01 00:00:00\tdeadbeef\t1\n'
  } >"$dir/sqlx_migrations.tsv"
  printf '1\n' >"$dir/schema_migrations.tsv"
  printf 'decisions/01AAA\ndecisions/01BBB\n' >"$dir/vault_list.paths.txt"
  printf 'decisions/01AAA\ndecisions/01BBB\n' >"$dir/vault_list.sorted.txt"
  printf 'portable\n' >"$dir/queries.txt"
  printf 'decisions/01AAA\n' >"$dir/search-01-portable.paths.txt"
  printf '{"index.db":{"notes":2,"note_links":1},"queue.sqlite":{"gradatum_jobs":0}}\n' \
    >"$dir/counts.json"
  printf '01AAA\tDone\tCurate\n' >"$dir/jobs.digest.txt"
  printf 'Done 1\n' >"$dir/jobs_by_status.txt"
  cp -- "$export" "$dir/export-features.json"
}

# Dialecte AVANT (2.4.0) et dialecte MONTÉ (2.5.0) : deux entrées, mêmes cartes,
# même ordre — `visibility` est la clé que 2.5.0 ajoute à chaque entrée.
export_avant() {
  cat <<'JSON'
[
  {
    "feature": "F-01",
    "release": "planned",
    "version": "vX.Y.Z",
    "title": "Parite de l'export des cartes a la montee"
  },
  {
    "feature": "F-02",
    "release": "planned",
    "version": "vX.Y.Z",
    "title": "Survie du compteur de cartes a la montee"
  }
]
JSON
}

export_monte() {  # $1 = valeur de `visibility` par entrée (défaut : null)
  cat <<JSON
[
  {
    "feature": "F-01",
    "release": "planned",
    "version": "vX.Y.Z",
    "visibility": ${1:-null},
    "title": "Parite de l'export des cartes a la montee"
  },
  {
    "feature": "F-02",
    "release": "planned",
    "version": "vX.Y.Z",
    "visibility": ${1:-null},
    "title": "Survie du compteur de cartes a la montee"
  }
]
JSON
}

DOC_OK="$WORK/guide-complet.md"
cat >"$DOC_OK" <<'MD'
# Upgrading

The exported report gains the `visibility` axis. Nothing else moves.
MD
DOC_MUETTE="$WORK/guide-muet.md"
cat >"$DOC_MUETTE" <<'MD'
# Upgrading

Nothing changed in this line.
MD

# ── Le banc ──────────────────────────────────────────────────────────────────

# Un cas = une comparaison. `check_case` exécute, capture le rc DANS UNE
# VARIABLE (jamais à travers un tube), puis exige le rc ET chaque motif demandé.
# Un motif précédé de `!` doit au contraire être ABSENT du rapport.
check_case() {  # $1 = nom, $2 = rc attendu, $3.. = motifs, puis -- args du script
  local name="$1" want_rc="$2"; shift 2
  local -a motifs=()
  while (($#)) && [[ "$1" != "--" ]]; do motifs+=("$1"); shift; done
  shift || true
  local out="$WORK/$name.report"
  local rc=0
  bash "$PARITY" "$@" --label "$name" --out "$out" >"$WORK/$name.stdout" 2>"$WORK/$name.stderr" || rc=$?
  local problems=""
  if (( rc != want_rc )); then problems+=" rc=$rc (attendu $want_rc)"; fi
  local m
  for m in "${motifs[@]:-}"; do
    [[ -n "$m" ]] || continue
    if [[ "$m" == '!'* ]]; then
      if [[ -f "$out" ]] && grep -qF -- "${m#!}" "$out"; then
        problems+=" motif interdit présent: ${m#!}"
      fi
    else
      if [[ ! -f "$out" ]] || ! grep -qF -- "$m" "$out"; then
        problems+=" motif absent: $m"
      fi
    fi
  done
  if [[ -z "$problems" ]]; then
    printf 'ok      %s\n' "$name"
    PASSED=$((PASSED + 1))
  else
    printf 'ÉCART   %s —%s\n' "$name" "$problems"
    printf '        rc observé : %s (attendu %s)\n' "$rc" "$want_rc"
    if [[ -f "$out" ]]; then
      sed 's/^/        | /' "$out" 2>/dev/null | head -40 || true
    fi
    FAILED=$((FAILED + 1))
  fi
  return 0
}

printf 'assert-parity-tests — clé (e), règle additive de l export de cartes\n\n'

# ── (c) clé ajoutée seulement → succès, avec relevé ──────────────────────────
B="$WORK/c/avant"; A="$WORK/c/monte"
export_avant >"$WORK/c-avant.json"
export_monte  >"$WORK/c-monte.json"
make_releve "$B" "$WORK/c-avant.json"
make_releve "$A" "$WORK/c-monte.json"
check_case "c-cles-ajoutees-relevees" 0 \
  "cles_ajoutees=visibility" \
  "PARITÉ ÉTABLIE" \
  -- \
  --before "$B" --after "$A" --export-mode additif --export-doc "$DOC_OK"

# ── (a) une valeur modifiée côté MONTÉ → échec nommé ─────────────────────────
B="$WORK/a/avant"; A="$WORK/a/monte"
make_releve "$B" "$WORK/c-avant.json"
jq '.[0].title = "Titre reecrit par 2.5.0"' "$WORK/c-monte.json" >"$WORK/a-monte.json"
make_releve "$A" "$WORK/a-monte.json"
check_case "a-valeur-modifiee" 1 \
  "DIVERGENCE" \
  "title" \
  "Parite de l'export des cartes a la montee" \
  "Titre reecrit par 2.5.0" \
  -- \
  --before "$B" --after "$A" --export-mode additif --export-doc "$DOC_OK"

# ── (b) une clé du dialecte AVANT absente côté MONTÉ → échec nommé ───────────
B="$WORK/b/avant"; A="$WORK/b/monte"
make_releve "$B" "$WORK/c-avant.json"
jq 'map(del(.version))' "$WORK/c-monte.json" >"$WORK/b-monte.json"
make_releve "$A" "$WORK/b-monte.json"
check_case "b-cle-avant-disparue" 1 \
  "DIVERGENCE" \
  "version" \
  -- \
  --before "$B" --after "$A" --export-mode additif --export-doc "$DOC_OK"

# ── (d) nombre de cartes différent → échec nommé ─────────────────────────────
B="$WORK/d/avant"; A="$WORK/d/monte"
make_releve "$B" "$WORK/c-avant.json"
jq '.[0:1]' "$WORK/c-monte.json" >"$WORK/d-monte.json"
make_releve "$A" "$WORK/d-monte.json"
check_case "d-nombre-de-cartes" 1 \
  "DIVERGENCE" \
  -- \
  --before "$B" --after "$A" --export-mode additif --export-doc "$DOC_OK"

# ── (e) la clé ajoutée doit être DOCUMENTÉE dans le guide ────────────────────
B="$WORK/e/avant"; A="$WORK/e/monte"
make_releve "$B" "$WORK/c-avant.json"
make_releve "$A" "$WORK/c-monte.json"
check_case "e-cle-non-documentee" 1 \
  "DIVERGENCE" \
  "visibility" \
  -- \
  --before "$B" --after "$A" --export-mode additif --export-doc "$DOC_MUETTE"

# ── (f) mode additif SANS guide → NON EXÉCUTÉ, jamais un vert ────────────────
check_case "f-sans-guide" 2 \
  "NON EXÉCUTÉ" \
  -- \
  --before "$WORK/e/avant" --after "$WORK/e/monte" --export-mode additif

# ── (g) le retour arrière reste OCTET À OCTET ────────────────────────────────
B="$WORK/g/avant"; A="$WORK/g/retour"
make_releve "$B" "$WORK/c-avant.json"
make_releve "$A" "$WORK/c-avant.json"
check_case "g-strict-retour-identique" 0 \
  "octet à octet" \
  -- \
  --before "$B" --after "$A" --export-mode strict

# Le retour arrière avec la clé ajoutée est un ÉCART : le mode strict ne projette
# rien, il compare les octets — c'est ce qui distingue les deux comparaisons.
check_case "h-strict-refuse-l-ajout" 1 \
  "DIVERGENCE" \
  -- \
  --before "$WORK/g/avant" --after "$WORK/e/monte" --export-mode strict

# ── (i) mode inconnu → refus, jamais un défaut silencieux ────────────────────
check_case "i-mode-inconnu" 2 \
  -- \
  --before "$WORK/g/avant" --after "$WORK/g/retour" --export-mode semantique

printf '\n%s cas conforme(s), %s en écart\n' "$PASSED" "$FAILED"
(( FAILED == 0 )) || exit 1
exit 0
