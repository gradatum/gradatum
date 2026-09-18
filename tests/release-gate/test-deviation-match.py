#!/usr/bin/env python3
"""Contract of the deviation matcher, asserted against fixture inventories.

The matcher answers one question: is this MEASURED break allowed? It matches the
exact (crate, lint, rendered) triple -- allowing a whole crate would be a blank
cheque. These tests point it at inventories whose shape they control, so the
assertion is about the matcher's behaviour and never about the content of the
shipped manifest: a manifest emptied tomorrow must not make these tests vacuous,
and a manifest filled tomorrow must not make them green.

The discriminating case is the one with the right (lint, rendered) and the WRONG
crate: it can only pass if the matching is per entry rather than per crate.

  populated inventory -> rc 0 for the named triple, rc 1 for anything else
  empty inventory     -> rc 1 (read, authorizes nothing)
  absent inventory    -> rc 2 (nothing to read -- not a green light either)

Usage: python3 tests/release-gate/test-deviation-match.py  (from repo root)
"""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
MATCHER = os.path.join(ROOT, "scripts", "internal", "check-deviation-match.py")
INVENTORY_ENV = "SEMVER_INVENTORY_PATH"
FIX = os.path.join(HERE, "fixtures")

VIDE = os.path.join(FIX, "inventaire-vide.yaml")
ABSENT = os.path.join(FIX, "inventaire-absent.yaml")
NOMME = os.path.join(FIX, "inventaire-nomme.yaml")
PARTIEL = os.path.join(FIX, "inventaire-partiel.yaml")
GONE = os.path.join(FIX, "inventaire-qui-n-existe-pas.yaml")

executed = 0
failed = 0


def check(label, ok, detail=""):
    global executed, failed
    executed += 1
    print(f"{'OK  ' if ok else 'FAIL'} {label}{' — ' + detail if detail else ''}")
    if not ok:
        failed += 1


def run(inventory, crate, lint, rendered):
    env = dict(os.environ, **{INVENTORY_ENV: inventory})
    p = subprocess.run([sys.executable, MATCHER, crate, lint, rendered],
                       env=env, capture_output=True, text=True)
    return p.returncode, p.stdout + p.stderr


def expect(label, inventory, crate, lint, rendered, want_rc, marker=None):
    rc, text = run(inventory, crate, lint, rendered)
    ok = rc == want_rc and (marker is None or marker in text)
    detail = f"rc={rc} (attendu {want_rc})"
    if marker is not None and marker not in text:
        detail += f", marqueur absent : {marker!r}"
    check(label, ok, detail)


# ── Populated inventory: the named triple only ───────────────────────────────
expect("peuple [triplet nommé -> autorisé]",
       NOMME, "gradatum-core", "enum_variant_missing", "variant KindKind::Chore", 0, "AUTORISEE")
expect("peuple [rendered inconnu -> refusé]",
       NOMME, "gradatum-core", "enum_variant_missing", "variant KindKind::Fantome", 1, "NON INSCRITE")
expect("peuple [mauvais crate -> refusé, pas de chèque en blanc]",
       NOMME, "gradatum-queue", "enum_variant_missing", "variant KindKind::Chore", 1, "NON INSCRITE")
expect("peuple [mauvais lint -> refusé, le lint est dans la clé]",
       NOMME, "gradatum-core", "function_missing", "variant KindKind::Chore", 1, "NON INSCRITE")

# ── Granularity: a neighbour entry of the SAME (crate, lint) authorizes nothing
expect("partiel [entrée nommée -> autorisée]",
       PARTIEL, "gradatum-core", "enum_variant_missing", "variant KindKind::Chore", 0, "AUTORISEE")
expect("partiel [entrée voisine non nommée -> refusée]",
       PARTIEL, "gradatum-core", "enum_variant_missing", "variant KindKind::Spike", 1, "NON INSCRITE")

# ── Empty: read, authorizes nothing. Not a green light ───────────────────────
expect("vide [aucune entrée -> refusé, motif explicite]",
       VIDE, "gradatum-core", "enum_variant_missing", "variant KindKind::Chore", 1, "authorizes nothing")

# ── Absent: nothing to read. Not a green light either, and not the same state ─
expect("absent [clé absente -> non lu]",
       ABSENT, "gradatum-core", "enum_variant_missing", "variant KindKind::Chore", 2, "NON LU")
expect("absent [fichier introuvable -> non lu]",
       GONE, "gradatum-core", "enum_variant_missing", "variant KindKind::Chore", 2, "NON LU")

# ── Usage contract: a missing argument can never be read as an authorization ─
p = subprocess.run([sys.executable, MATCHER, "gradatum-core", "enum_variant_missing"],
                   capture_output=True, text=True)
check("usage [arguments incomplets -> refusé]", p.returncode != 0, f"rc={p.returncode}")

if __name__ == "__main__":
    if executed == 0:
        print("FAIL -- aucune assertion executee : le harnais n'a pas ete atteint, ce n'est pas un vert")
        sys.exit(1)
    print(f"-- {executed} assertions executees, {failed} echec(s)")
    sys.exit(1 if failed else 0)
