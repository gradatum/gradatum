#!/usr/bin/env bash
# Eprouvette TDD of the G-README gate (scripts/internal/check-readme-versions.sh, consumed
# by scripts/internal/release-readiness-scan.sh).
#
# WHAT IS PROVED. The gate judges the version the documentation advertises to consumers:
#   1. a publishable crate README announces `**Status**: v<workspace>` ;
#   2. no README of a publishable crate pins an INSTALL extract to another version of that
#      crate itself ;
#   3. the root README carries the milestone row `| **v<workspace>** |`.
# Each of the three is exercised RED first, then GREEN, on a minimal temp workspace built by
# this eprouvette -- the real gate body is run, never a copy of its logic.
#
# WHY A FIXTURE WORKSPACE AND NOT THE HOST REPO. A gate tested against the tree it is meant
# to guard proves nothing on the day the tree is correct: the real 28 READMEs are supposed to
# be aligned, so running the gate on them can only ever render the green branch. The fixture
# carries the drift on purpose, so the RED branch is observed instead of assumed.
#
# WHY `cargo` IS STUBBED FOR THE END-TO-END SECTION. The scan's other gates are out of this
# eprouvette's perimeter and would drag a Rust toolchain, a registry and a git tag space into
# a test that runs in a job deliberately kept light. The stub answers `cargo metadata` with a
# one-crate manifest and exits 0 elsewhere, which keeps every other gate off the critical
# path. It is a no-op, never a fake verdict: any gate that needs real cargo output fails
# loudly instead of silently passing.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
CHECK="${CHECK_README_VERSIONS:-$ROOT/scripts/internal/check-readme-versions.sh}"
SCAN="$ROOT/scripts/internal/release-readiness-scan.sh"
[ -r "$CHECK" ] || { echo "FAIL artefact introuvable : $CHECK"; exit 2; }
[ -r "$SCAN" ] || { echo "FAIL artefact introuvable : $SCAN"; exit 2; }
fail=0

# The scan resolves the release rank against the repository it runs from (git tags), so the
# end-to-end section must run from the repo root.
cd "$ROOT"

TMPD="$(mktemp -d)"
trap 'rm -rf "$TMPD"' EXIT
WS="$TMPD/ws"
STUB="$TMPD/stub"
FIXVER=9.9.9
mkdir -p "$STUB"

# The version the fixture workspaces announce is deliberately NOT any real release number:
# a fixture aligned with the current workspace version would pass for the wrong reason on the
# day someone edits that number.
cat > "$TMPD/metadata.json" <<'JSON'
{"packages":[{"name":"gradatum-fixture","publish":null,"metadata":{},"features":{}}]}
JSON
export FIXTURE_METADATA_JSON="$TMPD/metadata.json"
cat > "$STUB/cargo" <<'STUBEOF'
#!/usr/bin/env bash
# Test stub: only `cargo metadata` answers, and only with the fixture manifest.
if [ "${1:-}" = "metadata" ]; then cat "$FIXTURE_METADATA_JSON"; exit 0; fi
exit 0
STUBEOF
chmod +x "$STUB/cargo"

# build_ws <aligned|drift-status|drift-install|drift-milestone>
build_ws() {
  local mode="$1" sver iver
  rm -rf "$WS"
  mkdir -p "$WS/crates/gradatum-fixture/src" "$WS/scripts"
  cat > "$WS/Cargo.toml" <<EOF
[workspace]
resolver = "3"
members = ["crates/gradatum-fixture"]

[workspace.package]
version = "$FIXVER"
edition = "2024"
EOF
  cat > "$WS/crates/gradatum-fixture/Cargo.toml" <<'EOF'
[package]
name = "gradatum-fixture"
version.workspace = true
edition.workspace = true
description = "Fixture crate for the G-README gate eprouvette"
license = "Apache-2.0"
EOF
  echo 'pub fn fixture() {}' > "$WS/crates/gradatum-fixture/src/lib.rs"
  # The scan delegates its French-string gate to a workspace-relative script. The fixture
  # stubs it so that gate reports nothing about a workspace that carries no source strings.
  printf '#!/usr/bin/env bash\nexit 0\n' > "$WS/scripts/scan-fr-strings.sh"
  chmod +x "$WS/scripts/scan-fr-strings.sh"

  case "$mode" in
    drift-status)  sver="1.0.0" ;;
    drift-install) sver="$FIXVER"; iver="1.0.0" ;;
    *)             sver="$FIXVER" ;;
  esac
  iver="${iver:-$sver}"
  # The sibling pin (`gradatum-fixture-extra`) is present in every variant: it is the
  # boundary trap. A gate matching the crate name as a SUBSTRING would flag it and turn the
  # aligned case red -- which is the only reason this line exists.
  cat > "$WS/crates/gradatum-fixture/README.md" <<EOF
# gradatum-fixture

**Status**: v$sver — public, Apache-2.0. Stable API under SemVer.

## Install

\`\`\`toml
gradatum-fixture = "$iver"
\`\`\`

Historical note: the sibling crate \`gradatum-fixture-extra = "1.0.0"\` is out of scope.
EOF

  if [ "$mode" = "drift-milestone" ]; then
    cat > "$WS/README.md" <<EOF
# Fixture root

| Version | Status | What it brings |
|---|---|---|
| **v1.0.0** | ✅ shipped | A past milestone. |
EOF
  else
    cat > "$WS/README.md" <<EOF
# Fixture root

| Version | Status | What it brings |
|---|---|---|
| **v$FIXVER** | ✅ shipped | The current milestone. |
EOF
  fi
}

# run_check <libelle> <attendu 0|1|2> [args...]
run_check() {
  local label="$1" want="$2"; shift 2
  bash "$CHECK" "$@" > "$TMPD/last.out" 2>&1
  echo "$?" > "$TMPD/last.rc"
  local got; got="$(cat "$TMPD/last.rc")"
  if [ "$got" = "$want" ]; then
    echo "OK   $label -> rc=$got"
  else
    echo "FAIL $label -> rc=$got attendu $want"
    sed 's/^/     | /' "$TMPD/last.out"
    fail=1
  fi
}

# assert_out <libelle> <motif-etendu>  -- the gate must NAME the faulty file / expected line.
assert_out() {
  if grep -qE -- "$2" "$TMPD/last.out"; then
    echo "OK   $1"
  else
    echo "FAIL $1 (motif '$2' absent de la sortie)"
    sed 's/^/     | /' "$TMPD/last.out"
    fail=1
  fi
}
# assert_out_fixed <libelle> <chaine> -- same, on a fixed string: the expected LINE the gate
# prints carries Markdown (`**Status**`, `| **vX** |`) whose escaping in an ERE would test the
# escaping rather than the gate.
assert_out_fixed() {
  if grep -qF -- "$2" "$TMPD/last.out"; then
    echo "OK   $1"
  else
    echo "FAIL $1 (chaine '$2' absente de la sortie)"
    sed 's/^/     | /' "$TMPD/last.out"
    fail=1
  fi
}

echo "--- 1. README aligne : la gate doit etre VERTE ---"
build_ws aligned
run_check "aligned / 1 crate" 0 "$WS" gradatum-fixture

echo "--- 2. ligne de statut desalignee : ROUGE, fichier fautif + ligne attendue nommes ---"
build_ws drift-status
run_check "status v1.0.0 vs workspace $FIXVER" 1 "$WS" gradatum-fixture
assert_out "le fichier fautif est nomme" 'crates/gradatum-fixture/README\.md'
assert_out "la version fautive est nommee" 'v1\.0\.0'
assert_out_fixed "la ligne attendue est nommee" "expected: **Status**: v$FIXVER"

echo "--- 3. extrait d'installation desaligne : ROUGE ---"
build_ws drift-install
run_check "extrait = \"1.0.0\" vs workspace $FIXVER" 1 "$WS" gradatum-fixture
assert_out_fixed "l'extrait fautif est nomme" 'gradatum-fixture = "1.0.0"'
assert_out_fixed "la ligne attendue est nommee" "expected: gradatum-fixture = \"$FIXVER\""

echo "--- 4. ligne de jalon racine absente : ROUGE nomme ---"
build_ws drift-milestone
run_check "jalon v$FIXVER absent du README racine" 1 "$WS" gradatum-fixture
assert_out_fixed "la ligne de jalon attendue est nommee" "expected: | **v$FIXVER** |"

echo "--- 5. perimetre vide ou non etabli : rc 2, jamais un PASS ---"
build_ws aligned
run_check "aucun crate fourni" 2 "$WS"
run_check "workspace inexistant" 2 "$TMPD/does-not-exist" gradatum-fixture
mkdir -p "$TMPD/no-version/crates/gradatum-fixture"
printf '[workspace]\nmembers = []\n' > "$TMPD/no-version/Cargo.toml"
run_check "version workspace illisible" 2 "$TMPD/no-version" gradatum-fixture

echo "--- 6. bout en bout : la gate est REELLEMENT branchee dans le scan ---"
# Without this section, deleting the G-README block from release-readiness-scan.sh would
# leave every assertion above green: they exercise the gate body, not its wiring.
build_ws drift-status
PATH="$STUB:$PATH" bash "$SCAN" "$WS" > "$TMPD/scan-red.out" 2>&1
echo "$?" > "$TMPD/scan-red.rc"
if grep -q 'G-README FAIL' "$TMPD/scan-red.out"; then
  echo "OK   scan sur README desaligne -> verdict G-README FAIL"
else
  echo "FAIL scan sur README desaligne : aucun verdict 'G-README FAIL' (gate debranchee ?)"
  sed 's/^/     | /' "$TMPD/scan-red.out"
  fail=1
fi
if [ "$(cat "$TMPD/scan-red.rc")" = "0" ]; then
  echo "FAIL scan sur README desaligne -> rc=0 : le verdict G-README ne remonte pas au scan"
  fail=1
else
  echo "OK   scan sur README desaligne -> rc=$(cat "$TMPD/scan-red.rc") (non nul)"
fi

build_ws aligned
PATH="$STUB:$PATH" bash "$SCAN" "$WS" > "$TMPD/scan-green.out" 2>&1
if grep -q 'G-README : PASS' "$TMPD/scan-green.out"; then
  echo "OK   scan sur README aligne -> verdict G-README : PASS"
else
  echo "FAIL scan sur README aligne : aucun verdict 'G-README : PASS'"
  sed 's/^/     | /' "$TMPD/scan-green.out"
  fail=1
fi

exit $fail
