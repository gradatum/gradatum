#!/usr/bin/env python3
"""Behaviour of the semver deviation inventory, asserted PER RELEASE RANK.

The release gate answers one question per rank: is this measured break allowed?
That answer depends on what the rank permits -- never on how many entries the
shipped inventory happens to hold. These tests therefore drive the real
classifier with inventories whose SHAPE they control (empty, absent, named,
partial) and assert the outcome per rank. They never assert the current content
of RELEASE-MANIFEST.yaml, so a purge of that file cannot move them, and a
populated file cannot make them green by accident.

Three states of the inventory, two of which are NOT green lights:
  absent   -> nothing was read            -> the gate does not run
  empty    -> read, authorizes nothing    -> any measured break blocks
  populated-> read, authorizes the ENTRIES IT NAMES, and nothing else

The inventory path is injected through SEMVER_INVENTORY_PATH, consumed by the
matcher; the shipped manifest stays the default for the gate itself.

Usage: python3 tests/release-gate/test-inventaire-schema.py  (from repo root)
"""
import importlib.util
import os
import re
import subprocess
import sys

sys.dont_write_bytecode = True  # loading the matcher must not litter the source tree

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
CLASSIFY = os.path.join(ROOT, "scripts", "internal", "classify-semver-output.sh")
MATCHER_PATH = os.path.join(ROOT, "scripts", "internal", "check-deviation-match.py")
MANIFEST = os.path.join(ROOT, "RELEASE-MANIFEST.yaml")
INVENTORY_ENV = "SEMVER_INVENTORY_PATH"

FIX = os.path.join(HERE, "fixtures")
INS = os.path.join(FIX, "semver-inscrite.out")          # real tool output, 2 named breaks
NON = os.path.join(FIX, "semver-non-inscrite.out")      # real tool output + one unnameable break
MAJ = os.path.join(FIX, "semver-majeur.out")            # real tool output, 0 checks
VIDE = os.path.join(FIX, "inventaire-vide.yaml")        # key present, no entry
VIDE_FLOW = os.path.join(FIX, "inventaire-vide-flow.yaml")
ABSENT = os.path.join(FIX, "inventaire-absent.yaml")    # no key at all
NOMME = os.path.join(FIX, "inventaire-nomme.yaml")      # names the 2 breaks of INS
PARTIEL = os.path.join(FIX, "inventaire-partiel.yaml")  # names one of the 2
GONE = os.path.join(FIX, "inventaire-qui-n-existe-pas.yaml")

CHAMPS = {"crate", "symbol", "lint", "rendered", "kind",
          "introduced_in", "ships_in", "reason", "card"}

executed = 0
failed = 0


def _load_matcher():
    spec = importlib.util.spec_from_file_location("check_deviation_match", MATCHER_PATH)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


MATCHER = _load_matcher()


def check(label, ok, detail=""):
    global executed, failed
    executed += 1
    print(f"{'OK  ' if ok else 'FAIL'} {label}{' — ' + detail if detail else ''}")
    if not ok:
        failed += 1


def classer(rank, out_file, inventory):
    """Run the real classifier for one rank against one inventory."""
    env = dict(os.environ, **{INVENTORY_ENV: inventory})
    p = subprocess.run(["bash", CLASSIFY, "gradatum-core", rank, out_file],
                       env=env, capture_output=True, text=True)
    return p.returncode, p.stdout + p.stderr


def expect(label, rank, out_file, inventory, want_rc, *markers):
    """Assert the rank outcome AND that the branch which produced it was reached."""
    rc, text = classer(rank, out_file, inventory)
    missing = [m for m in markers if not re.search(m, text)]
    detail = f"rc={rc} (attendu {want_rc})"
    if missing:
        detail += f", branche non atteinte, marqueur(s) absent(s) : {missing}"
    check(label, rc == want_rc and not missing, detail)
    return rc, text


# ── 1. An inventory that exists and holds nothing authorizes nothing ─────────
def inventaire_vide_sous_mineur_bloque_toute_rupture():
    expect("inventaire_vide_sous_mineur_bloque_toute_rupture [minor]",
           "minor", INS, VIDE, 1, "sous un rang MINEUR", "authorizes nothing")
    expect("inventaire_vide_sous_mineur_bloque_toute_rupture [patch]",
           "patch", INS, VIDE, 1, "rang de CORRECTIF")
    expect("inventaire_vide_sous_mineur_bloque_toute_rupture [major]",
           "major", INS, VIDE, 0, "^RAPPORT ")


# ── 2. Empty is a state of the inventory, absent is the lack of one ──────────
def inventaire_vide_est_distinct_d_inventaire_absent():
    rc_vide, txt_vide = expect("inventaire_vide_est_distinct_d_inventaire_absent [vide -> bloqué]",
                               "minor", INS, VIDE, 1, "authorizes nothing")
    rc_absent, txt_absent = expect("inventaire_vide_est_distinct_d_inventaire_absent [absent -> non exécuté]",
                                   "minor", INS, ABSENT, 2, "NON LU", "gate NON EXECUTE")
    check("inventaire_vide_est_distinct_d_inventaire_absent [codes distincts]",
          rc_vide != rc_absent, f"vide={rc_vide}, absent={rc_absent}")
    check("inventaire_vide_est_distinct_d_inventaire_absent [motifs distincts]",
          "authorizes nothing" in txt_vide and "authorizes nothing" not in txt_absent)
    expect("inventaire_vide_est_distinct_d_inventaire_absent [liste vide en flow: lu, pas absent]",
           "minor", INS, VIDE_FLOW, 1, "authorizes nothing")
    expect("inventaire_vide_est_distinct_d_inventaire_absent [fichier introuvable -> non exécuté]",
           "minor", INS, GONE, 2, "NON LU")


# ── 3. A populated inventory authorizes the entries it NAMES, nothing else ───
def inventaire_peuple_sous_mineur_autorise_les_entrees_nommees():
    expect("inventaire_peuple_sous_mineur_autorise_les_entrees_nommees [nomme/minor -> autorisé]",
           "minor", INS, NOMME, 0, "toutes INSCRITES")
    expect("inventaire_peuple_sous_mineur_autorise_les_entrees_nommees [partiel/minor -> non nommée bloquée]",
           "minor", INS, PARTIEL, 1, "sous un rang MINEUR")
    expect("inventaire_peuple_sous_mineur_autorise_les_entrees_nommees [sortie non inscrite -> bloquée]",
           "minor", NON, NOMME, 1, "sous un rang MINEUR")
    expect("inventaire_peuple_sous_mineur_autorise_les_entrees_nommees [nomme/patch -> rien d'autorisé]",
           "patch", INS, NOMME, 1, "rang de CORRECTIF")
    expect("inventaire_peuple_sous_mineur_autorise_les_entrees_nommees [nomme/major -> rapport]",
           "major", INS, NOMME, 0, "^RAPPORT ")


# ── 4. Vacuum is decided BEFORE the inventory is ever consulted ──────────────
def vacuite_echoue_hors_majeur_quel_que_soit_l_inventaire():
    expect("vacuite [N==0 sous minor -> non exécuté]",
           "minor", MAJ, NOMME, 2, "gate NON EXECUTE")
    expect("vacuite [N==0 sous minor, inventaire vide -> non exécuté]",
           "minor", MAJ, VIDE, 2, "gate NON EXECUTE")
    expect("vacuite [N==0 sous patch, inventaire vide -> non exécuté]",
           "patch", MAJ, VIDE, 2, "gate NON EXECUTE")
    expect("vacuite [N==0 sous major -> rapport]",
           "major", MAJ, VIDE, 0, "^RAPPORT ")


# ── 5. Schema of the inventories the gate reads (no assertion on any count) ──
def inventaires_lus_portent_le_schema_complet():
    status, entries = MATCHER.load_inventory(MANIFEST)
    check("schema [manifeste: cle semver_deviations lisible]",
          status == "ok",
          f"status={status} (absent = inventaire NON LU, jamais un feu vert)")
    if status == "ok":
        bad = [d for d in entries if CHAMPS - set(d)]
        check("schema [manifeste: chaque entree porte les 9 champs]",
              not bad, f"{len(entries)} entree(s) lue(s), incompletes : {bad}")
    for label, path in (("nomme", NOMME), ("partiel", PARTIEL)):
        _, ents = MATCHER.load_inventory(path)
        bad = [d for d in ents if CHAMPS - set(d)]
        check(f"schema [fixture {label}: chaque entree porte les 9 champs]",
              bool(ents) and not bad, f"{len(ents)} entree(s), incompletes : {bad}")


if __name__ == "__main__":
    inventaire_vide_sous_mineur_bloque_toute_rupture()
    inventaire_vide_est_distinct_d_inventaire_absent()
    inventaire_peuple_sous_mineur_autorise_les_entrees_nommees()
    vacuite_echoue_hors_majeur_quel_que_soit_l_inventaire()
    inventaires_lus_portent_le_schema_complet()
    if executed == 0:
        print("FAIL -- aucune assertion executee : le harnais n'a pas ete atteint, ce n'est pas un vert")
        sys.exit(1)
    print(f"-- {executed} assertions executees, {failed} echec(s)")
    sys.exit(1 if failed else 0)
