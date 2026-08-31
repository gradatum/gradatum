#!/usr/bin/env python3
"""TDD -- check-deviation-match.py : appariement par triplet (crate, lint, rendered).
Le cas discriminant (bon lint+rendered, MAUVAIS crate) prouve que l'appariement est par
symbole, jamais un cheque en blanc par crate."""
import subprocess, sys
S = "scripts/internal/check-deviation-match.py"
def run(*args):
    return subprocess.run([sys.executable, S, *args]).returncode

assert run("gradatum-core", "enum_variant_missing", "variant KindKind::Chore") == 0, \
    "inscrite (bon triplet) -> doit passer"
assert run("gradatum-core", "enum_variant_missing", "variant KindKind::Fantome") == 1, \
    "rendered non inscrit -> doit echouer"
assert run("gradatum-queue", "enum_variant_missing", "variant KindKind::Chore") == 1, \
    "bon (lint,rendered) mais MAUVAIS crate -> doit echouer (pas de cheque en blanc)"
assert run("gradatum-core", "function_missing", "variant KindKind::Chore") == 1, \
    "bon (crate,rendered) mais MAUVAIS lint -> doit echouer (lint dans la cle)"
print("OK -- appariement par triplet (crate, lint, rendered)")
