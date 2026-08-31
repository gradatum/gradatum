#!/usr/bin/env python3
"""TDD -- schema de l'inventaire semver_deviations dans RELEASE-MANIFEST.yaml.
Verifie : l'ancien champ objet a disparu ; expected_rank present et valide ; la liste
semver_deviations existe avec >= 2 entrees ; chaque entree porte les 9 champs obligatoires.
Cle d'appariement = (lint, rendered) (RÉV2) ; `symbol` reste pour l'humain."""
import re, sys
CHAMPS = {"crate", "symbol", "lint", "rendered", "kind",
          "introduced_in", "ships_in", "reason", "card"}
t = open("RELEASE-MANIFEST.yaml").read()

assert "semver_deviation_pending:" not in t, \
    "l'ancien champ objet subsiste -- la conversion n'est pas faite"
assert re.search(r"^expected_rank:\s*(patch|minor|major)\s*$", t, re.M), \
    "expected_rank absent ou invalide"

bloc = re.search(r"^semver_deviations:\n((?:  - .*\n(?:    .*\n)*)+)", t, re.M)
assert bloc, "semver_deviations absent ou vide"
entrees = re.findall(r"  - (?:.*\n)(?:    .*\n)*", bloc.group(1))
assert len(entrees) >= 2, f"conversion incomplete : {len(entrees)} entree(s), au moins 2 attendues"

for e in entrees:
    # ^\s*-?\s*(\w+)\s*: -- capte aussi le champ porte par la ligne "  - crate:"
    presents = set(re.findall(r"^\s*-?\s*(\w+)\s*:", e, re.M))
    manquants = CHAMPS - presents
    assert not manquants, f"entree incomplete, champs manquants : {manquants}\n{e}"

print(f"OK -- {len(entrees)} entrees, 9 champs presents (dont lint+rendered)")
