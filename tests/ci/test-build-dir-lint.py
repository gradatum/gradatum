#!/usr/bin/env python3
"""TDD -- scripts/internal/ci-lint-build-dir.py : portee multi-fichiers et plancher par fichier.

Le lint a d'abord couvert `ci.yml` seul. Une garde qui s'arrete a un fichier rend un
vert sur le fichier qu'elle ne lit pas : `release.yml` portait quatre jobs compilant
sans le step, zero occurrence de la variable, et rien ne pouvait le dire.

Les quatre mutations ci-dessous sont produites A PARTIR DU FICHIER REEL, jamais d'une
fixture ecrite a la main : une fixture qui derive du fichier reel prouve que le gate
mord sur la forme qu'il garde, pas sur une forme que seul le test connait.

Preuve exigee : codes de sortie captures par subprocess, jamais a travers un tube.
"""
import pathlib
import re
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
LINT = ROOT / "scripts/internal/ci-lint-build-dir.py"
CI = ROOT / ".forgejo/workflows/ci.yml"
REL = ROOT / ".forgejo/workflows/release.yml"

JOB = re.compile(r"^  ([A-Za-z0-9_.-]+):\s*$", re.M)
STEP = re.compile(r"^      - ")


def run(*paths):
    proc = subprocess.run(
        [sys.executable, str(LINT)] + [str(p) for p in paths],
        capture_output=True,
        text=True,
    )
    return proc.returncode, proc.stdout + proc.stderr


def job_span(lines, job):
    start = next(i for i, l in enumerate(lines) if l == "  %s:" % job)
    end = next(
        (i for i in range(start + 1, len(lines)) if JOB.match(lines[i])), len(lines)
    )
    return start, end


def drop_first_step(text, job):
    """Mutation 1 -- un job cargo perd son step `Cargo build-dir`."""
    lines = text.split("\n")
    start, end = job_span(lines, job)
    steps = next(i for i in range(start, end) if lines[i].strip() == "steps:")
    first = next(i for i in range(steps + 1, end) if STEP.match(lines[i]))
    second = next((i for i in range(first + 1, end) if STEP.match(lines[i])), end)
    return "\n".join(lines[:first] + lines[second:])


def inject_env_var(text, job):
    """Mutation 2 -- la variable repose en CLE de mapping `env:` d'un job."""
    lines = text.split("\n")
    start, end = job_span(lines, job)
    steps = next(i for i in range(start, end) if lines[i].strip() == "steps:")
    return "\n".join(
        lines[:steps]
        + ["    env:", "      CARGO_BUILD_BUILD_DIR: /tmp/inert"]
        + lines[steps:]
    )


def strip_cargo(text, job):
    """Mutation 3 -- un job cesse d'invoquer la famille cargo (plancher non atteint)."""
    lines = text.split("\n")
    start, end = job_span(lines, job)
    kept = [
        l
        for i, l in enumerate(lines)
        if not (start < i < end and re.search(r"\b(?:cargo|rustup|rustc|rustdoc)\b", l))
    ]
    return "\n".join(kept)


def with_file(name, text, fn):
    """Ecrit la mutation sous le BASENAME reel : le plancher est resolu par nom de fichier."""
    with tempfile.TemporaryDirectory() as tmp:
        path = pathlib.Path(tmp) / name
        path.write_text(text, encoding="utf-8")
        return fn(path)


failures = []


def check(label, ok, detail=""):
    print(("OK   : " if ok else "FAIL : ") + label + ("" if ok else " — " + detail))
    if not ok:
        failures.append(label)


# (0) Les deux workflows reels, ensemble : le critere de sortie.
rc, out = run(CI, REL)
check("paire reelle ci.yml + release.yml -> rc=0", rc == 0, "rc=%d\n%s" % (rc, out))
check(
    "les DEUX fichiers sont lus (release.yml n'est pas ignore en silence)",
    bool(re.search(r"release\.yml.*jobs cargo", out)),
    out,
)
check(
    "release.yml : le plancher per-file suit la mesure (3 jobs cargo)",
    bool(re.search(r"release\.yml.*3 jobs cargo", out)),
    "si un job cargo a ete ajoute ou retire, remesurer et mettre a jour "
    "MIN_CARGO_JOBS dans le lint ET ce test\n" + out,
)

rel_text = REL.read_text(encoding="utf-8")
ci_text = CI.read_text(encoding="utf-8")

# (1) Un job cargo qui perd son step est une VIOLATION, sur le second fichier aussi.
mutated = drop_first_step(rel_text, "build-arm64")
check(
    "release.yml sans le step sur build-arm64 -> rc=1",
    with_file("release.yml", mutated, lambda p: run(p)[0]) == 1,
    with_file("release.yml", mutated, lambda p: run(p)[1]),
)

# (2) La variable en cle de mapping `env:` est une VIOLATION, sur le second fichier aussi.
mutated = inject_env_var(rel_text, "ci-gate")
check(
    "release.yml avec CARGO_BUILD_BUILD_DIR en `env:` -> rc=1",
    with_file("release.yml", mutated, lambda p: run(p)[0]) == 1,
    with_file("release.yml", mutated, lambda p: run(p)[1]),
)

# (3) Sous le plancher : NON EXECUTE, jamais un vert par vacuite.
mutated = strip_cargo(strip_cargo(rel_text, "build-x86_64"), "build-arm64")
rc, out = with_file("release.yml", mutated, lambda p: run(p))
check("release.yml sous son plancher -> rc=2 (jamais 0)", rc == 2, "rc=%d\n%s" % (rc, out))
check(
    "le blocage NOMME le plancher et le compte detecte",
    "plancher" in out and "1 job(s) cargo" in out,
    out,
)

# (4) Aucun plancher devine pour un fichier non mesure : NON EXECUTE, pas un pass.
check(
    "fichier sans plancher declare -> rc=2 (aucun plancher devine)",
    with_file("autre-workflow.yml", ci_text, lambda p: run(p)[0]) == 2,
    with_file("autre-workflow.yml", ci_text, lambda p: run(p)[1]),
)

# (5) Appel sans argument : NON EXECUTE, jamais un fichier par defaut lu en silence.
rc, out = run()
check("appel sans chemin -> rc=2 (aucun defaut implicite)", rc == 2, "rc=%d\n%s" % (rc, out))

if failures:
    print("\n%d echec(s) : %s" % (len(failures), " ; ".join(failures)))
    sys.exit(1)
print("\nOK -- lint build-dir : deux workflows, plancher par fichier, mutations du fichier reel")
