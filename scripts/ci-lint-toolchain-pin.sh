#!/usr/bin/env bash
# ci-lint-toolchain-pin.sh — Gate CI d'enforcement de l'invariant toolchain.
#
# Règle (council Art.15bis puis Art.19, 2026-07-24) :
#   La version de Rust d'un projet n'est écrite QU'À UN SEUL ENDROIT :
#   son `rust-toolchain.toml`. Toute autre mention d'une version ou d'un canal
#   Rust dans une surface de build (workflow, Dockerfile, script) est une
#   duplication : elle dérive en silence du pin et fait compiler les artefacts
#   par un compilateur différent de celui qui les valide.
#
# Ce que le lint cherche (par ligne, commentaires strippés) :
#   - `rustup toolchain install <version|canal>`   → doit être SANS argument
#   - `rustup default|override set|run <toolchain>` → à supprimer
#   - `RUSTUP_TOOLCHAIN: <toolchain>`              → override du pin
#   - `cargo +<toolchain>` / `rustc +<toolchain>`  → sélecteur ad hoc
#   - `FROM rust:<version>` / `image: rust:<version>` → image figée hors pin
#   - `toolchain: <version>` (actions setup-rust)  → doublon du pin
#
# Ne sont des violations que les jetons qui SONT une toolchain Rust :
# une version concrète (`1.96.1`) ou un canal (`stable`/`beta`/`nightly`).
# `rust:slim-bookworm` ou `rust:latest` ne portent aucune version → ignorés.
#
# EXCEPTION — `nightly` seulement, marquée explicitement dans le fichier :
#   # lint-toolchain-pin: allow nightly — <raison non vide>
#   sur la ligne fautive ou la ligne juste au-dessus.
#   Une version concrète (`1.96.1`) ou `stable`/`beta` ne peut JAMAIS être
#   autorisée par ce marqueur : l'échappatoire ne couvre que le cas où aucune
#   toolchain stable ne peut faire le travail (rustdoc JSON, lints instables).
#   Les exceptions accordées sont IMPRIMÉES à chaque run — elles restent visibles.
#
# Périmètre scanné : surfaces qui exécutent un build.
#   .forgejo/workflows, .github/workflows, Dockerfile*, docker-compose*.yml,
#   scripts/*.sh, packaging/**/*.sh, Makefile, justfile.
# Exclus délibérément :
#   - `rust-toolchain.toml`  → c'est LA source unique, pas une duplication
#   - `*.md` (README, CHANGELOG, docs, ADR) → citent des versions à titre
#     historique ; les linter produirait du bruit et pousserait à réécrire
#     l'histoire. Limite connue : une consigne d'install périmée dans un README
#     n'est pas détectée.
#   - `Cargo.toml` → `rust-version` = MSRV déclarée, sémantique distincte du pin
#   - `target/`, `.git/`, `node_modules/`, `crates-publish-stubs*/`
#
# Exit 0 si conforme. Exit 1 (avec rapport détaillé) sinon. Exit 2 si erreur d'E/S.
#
# Usage :
#   scripts/ci-lint-toolchain-pin.sh                 # scanne le repo
#   scripts/ci-lint-toolchain-pin.sh <fichier...>    # scanne des fichiers précis (tests)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export REPO_ROOT_OVERRIDE="$REPO_ROOT"

python3 - "$@" <<'PYEOF'
import os
import re
import sys

REPO_ROOT = os.environ.get("REPO_ROOT_OVERRIDE") or os.getcwd()

# ── Découverte des fichiers à linter ────────────────────────────────────────
EXCLUDED_DIRS = {".git", "target", "node_modules", ".venv", "dist"}
EXCLUDED_DIR_PREFIXES = ("crates-publish-stubs",)


def is_scanned(relpath, name):
    if name == "rust-toolchain.toml":
        return False  # la source unique n'est pas une duplication d'elle-même
    if relpath.startswith((".forgejo/workflows/", ".github/workflows/")):
        return bool(re.search(r"\.ya?ml$", name))
    if name.startswith("Dockerfile") or name.endswith(".dockerfile"):
        return True
    if re.match(r"^docker-compose.*\.ya?ml$", name):
        return True
    if name.endswith(".sh"):
        return True
    if name in ("Makefile", "justfile", ".justfile"):
        return True
    return False


args = sys.argv[1:]
if args:
    files = args
else:
    files = []
    for dirpath, dirnames, filenames in os.walk(REPO_ROOT):
        dirnames[:] = sorted(
            d for d in dirnames
            if d not in EXCLUDED_DIRS and not d.startswith(EXCLUDED_DIR_PREFIXES)
        )
        for name in sorted(filenames):
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, REPO_ROOT)
            if is_scanned(rel, name):
                files.append(full)

# ── Motifs ──────────────────────────────────────────────────────────────────
# (libellé, regex, correctif à afficher). Le groupe 1 capture le jeton toolchain.
PATTERNS = [
    ("rustup toolchain install <version>",
     re.compile(r"\brustup\s+toolchain\s+install\s+(?!-)(\S+)"),
     "retirer la version : `rustup toolchain install` SANS argument lit rust-toolchain.toml"),
    ("rustup default <toolchain>",
     re.compile(r"\brustup\s+default\s+(?!-)(\S+)"),
     "supprimer la ligne : rust-toolchain.toml prime déjà sur le défaut rustup"),
    ("rustup override set <toolchain>",
     re.compile(r"\brustup\s+override\s+set\s+(?!-)(\S+)"),
     "supprimer la ligne : rust-toolchain.toml EST l'override du répertoire"),
    ("rustup run <toolchain>",
     re.compile(r"\brustup\s+run\s+(?!-)(\S+)"),
     "supprimer le sélecteur : invoquer cargo/rustc directement"),
    ("RUSTUP_TOOLCHAIN",
     re.compile(r"\bRUSTUP_TOOLCHAIN\s*[:=]\s*[\"']?([^\s\"']+)"),
     "supprimer la variable : elle court-circuite rust-toolchain.toml"),
    ("cargo/rustc +<toolchain>",
     re.compile(r"\b(?:cargo|rustc|rustdoc)\s+\+(\S+)"),
     "retirer le `+toolchain` : la toolchain active vient de rust-toolchain.toml"),
    ("image rust:<version>",
     re.compile(r"\brust:([A-Za-z0-9][A-Za-z0-9._-]*)"),
     "utiliser un tag sans version (`rust:slim-bookworm`) + `COPY rust-toolchain.toml`"),
    ("clé `toolchain:` (action setup-rust)",
     re.compile(r"^\s*toolchain\s*:\s*[\"']?([^\s\"']+)"),
     "supprimer la clé : les actions rust lisent rust-toolchain.toml par défaut"),
    ("action rust-toolchain@<rev>",
     re.compile(r"\brust-toolchain@([A-Za-z0-9][A-Za-z0-9._-]*)"),
     "remplacer par `run: rustup toolchain install --no-self-update` "
     "(dtolnay/rust-toolchain déduit la toolchain de son @rev, PAS de rust-toolchain.toml)"),
]

VERSION_RE = re.compile(r"^\d+\.\d+(\.\d+)?")
CHANNEL_RE = re.compile(r"^(stable|beta|nightly)\b")
# Marqueur d'exception : réservé à `nightly`, raison obligatoire et non vide.
MARKER_RE = re.compile(r"lint-toolchain-pin:\s*allow\s+nightly\s*(?:—|--|-|:)\s*(\S.*?)\s*$")


def strip_comment(line):
    """Retire un commentaire de fin de ligne hors guillemets (best effort)."""
    out = []
    in_s = in_d = False
    for ch in line:
        if ch == "'" and not in_d:
            in_s = not in_s
        elif ch == '"' and not in_s:
            in_d = not in_d
        elif ch == "#" and not in_s and not in_d:
            break
        out.append(ch)
    return "".join(out)


def classify(token):
    """-> 'version' | 'channel' | None (le jeton n'est pas une toolchain Rust)."""
    tok = token.strip("\"'`,;)")
    if VERSION_RE.match(tok):
        return "version"
    if CHANNEL_RE.match(tok):
        return "channel"
    return None


violations = []   # (rel, lineno, label, token, fix, raw)
exceptions = []   # (rel, lineno, token, reason)
scanned_lines = 0

for fpath in files:
    try:
        with open(fpath, encoding="utf-8", errors="replace") as fh:
            raw_lines = fh.read().splitlines()
    except OSError as exc:
        print(f"::error:: impossible de lire {fpath}: {exc}", file=sys.stderr)
        sys.exit(2)

    rel = os.path.relpath(fpath, REPO_ROOT)
    prev_raw = ""
    for idx, raw in enumerate(raw_lines, start=1):
        scanned_lines += 1
        code = strip_comment(raw)
        if code.strip():
            for label, rx, fix in PATTERNS:
                m = rx.search(code)
                if not m:
                    continue
                token = m.group(1)
                kind = classify(token)
                if kind is None:
                    continue
                # Exception : uniquement `nightly`, uniquement avec marqueur motivé.
                marker = MARKER_RE.search(raw) or MARKER_RE.search(prev_raw)
                if kind == "channel" and token.startswith("nightly") and marker:
                    exceptions.append((rel, idx, token, marker.group(1)))
                    continue
                violations.append((rel, idx, label, token, fix, raw.strip()))
        prev_raw = raw

# ── Le pin doit exister (sinon « aucune version nulle part » passerait à tort) ──
missing_pin = False
if not args:
    pin = os.path.join(REPO_ROOT, "rust-toolchain.toml")
    missing_pin = not os.path.isfile(pin)

print(f"[toolchain-pin lint] {len(files)} fichier(s) · {scanned_lines} ligne(s) · "
      f"{len(violations)} violation(s) · {len(exceptions)} exception(s) accordée(s)")

for rel, lineno, token, reason in exceptions:
    print(f"  ~ exception nightly · {rel}:{lineno} ({token}) — {reason}")

if missing_pin:
    print("")
    print("ÉCHEC — `rust-toolchain.toml` absent à la racine du dépôt.")
    print("Sans ce fichier la toolchain flotte sur le `stable` du contexte : le lint")
    print("n'aurait plus rien à comparer. Créer le pin AVANT de retirer les versions.")
    sys.exit(1)

if violations:
    print("")
    print("ÉCHEC — version/canal Rust écrit ailleurs que dans rust-toolchain.toml :")
    for rel, lineno, label, token, fix, raw in violations:
        print(f"  ✗ {rel}:{lineno} — {label} → '{token}'")
        print(f"      ligne   : {raw}")
        print(f"      corriger: {fix}")
    print("")
    print("Invariant (council Art.19 2026-07-24) : la version de Rust d'un projet")
    print("n'est écrite qu'à un seul endroit, son `rust-toolchain.toml`. Une version")
    print("dupliquée dans un workflow dérive du pin sans bruit — et fait livrer des")
    print("artefacts compilés par un autre compilateur que celui qui les a validés.")
    print("")
    print("Exception nightly (rustdoc JSON, lint instable) — à motiver sur la ligne")
    print("fautive ou juste au-dessus :")
    print("  # lint-toolchain-pin: allow nightly — <raison>")
    print("Aucun marqueur n'autorise une version concrète ni `stable`/`beta`.")
    sys.exit(1)

print("OK — aucune version Rust écrite hors rust-toolchain.toml.")
sys.exit(0)
PYEOF
