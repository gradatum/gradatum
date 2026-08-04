#!/usr/bin/env bash
# scan-fr-strings.sh — gate anti-régression i18n (F-102)
#
# Détecte le FRANÇAIS résiduel sur la surface distribuée du workspace gradatum.
# Sortie 1 si au moins un hit (bloque la CI / le publish).
#
# DEUX SURFACES, indépendantes (--surface) :
#
#   strings : littéraux de chaîne Rust du CODE DE PRODUCTION.
#     - N'inspecte QUE le contenu des littéraux (normaux, raw, byte).
#     - Exclut les modules de test inline `#[cfg(test)]` / fns `#[test]`|`#[tokio::test]`,
#       les répertoires `tests/`, les fichiers `*_test.rs` / `*_tests.rs`,
#       la crate `gradatum-bench` et les crates de parité (publish = false).
#     - Les `#[error("…")]` thiserror SONT inclus (surface d'erreurs typées).
#
#   docs    : doc-comments `///` et `//!` RENDUS PAR docs.rs.
#     - Périmètre = crates PUBLIABLES uniquement, déterminé par `cargo metadata`
#       (.publish == null). JAMAIS par `grep publish` (3 faux positifs sur ce dépôt).
#       Liste vide => exit 2 « gate non exécuté », jamais un PASS silencieux.
#     - Exclut les régions `#[cfg(test)]` (rustdoc ne les rend pas).
#     - Exclut les items `#[doc(hidden)]` et le SOUS-ARBRE DE FICHIERS des modules
#       déclarés `#[doc(hidden)] pub mod x;` — règle GÉNÉRALE, pas une liste de
#       crates en dur. En pratique elle ne réduit aujourd'hui que `gradatum-server`
#       (26/26 `pub mod` masqués), mais elle suivra toute nouvelle crate qui masque.
#       ⚠️ Elle ne dispense PAS `gradatum-server` du scan : son `//!` de crate root
#       et ses items racine NON masqués (ex. `build_rate_limit_test_app`) SONT
#       rendus sur docs.rs. Exclure la crate en bloc serait un angle mort.
#     - Détecte aussi le JARGON INTERNE (--jargon), qui ne veut rien dire pour un
#       lecteur de docs.rs : F-\d{2,3}, EX-C\d, Lot \d, Phase \d, R-\d, P0-\d, v81, §.
#     - EXEMPTION inline, jeton par jeton, quand le jargon est un EXEMPLE DE FORMAT
#       et non une référence interne (`[[feature:F-37]]` documente la syntaxe d'un
#       wikilink ; `Valid: F-37` documente le contrat d'un validateur) :
#
#           // scan-fr-strings: allow-jargon F-37 — syntaxe de wikilink documentée
#           /// | feature | `[[feature:F-37]]` | …
#
#       Le marqueur va sur un commentaire ORDINAIRE `//`, ligne du dessus — jamais
#       dans le `///`, que rustdoc publierait sur docs.rs. Il nomme LE JETON qu'il
#       autorise : un autre jeton sur la même ligne reste un hit, et il n'existe
#       AUCUN `allow-fr` — un marqueur ne peut structurellement pas taire du
#       français. Raison obligatoire. Les exemptions accordées ET les exemptions
#       devenues inutiles sont imprimées à chaque run. Marqueur mal formé ou placé
#       dans un doc-comment => exit 2 « gate défectueux », jamais un PASS.
#
# Déterminisme : LC_ALL=C.UTF-8 (évite les faux positifs octet en PCRE),
#   classe de codepoints Unicode FR explicite, lexer Rust-aware (python3).
#
# Usage :
#   scripts/scan-fr-strings.sh [--surface strings|docs|both] [--scope product|all]
#                              [--mode accent|full] [--jargon on|off] [-v]
#
#   --surface both   (défaut) : littéraux + doc-comments.
#   --surface strings        : littéraux seuls (comportement historique F-102).
#   --surface docs           : doc-comments seuls.
#   --scope all      (défaut) : toutes les crates de production du workspace.
#   --scope product          : sous-ensemble « surface » (CLI/HTTP/SDK/DTO/API publique).
#   --mode  accent   (défaut) : codepoints FR accentués (À-ÿ hors ×÷, + œŒŸ)
#                              + NOYAU de mots FR SANS accent sans homographe
#                              anglais (« vide », « trop », « octets »… — cf. NOTE).
#   --mode  full             : noyau + mots-outils FR À RISQUE d'homographe EN/FR
#                              (audit manuel exhaustif, PAS un gate CI — cf. NOTE).
#   --jargon on      (défaut) : applique la détection de jargon interne aux doc-comments.
#   --jargon off             : désactive (utile pour isoler les hits purement FR).
#   -v                        : périmètre prouvé (fichiers + crates scannés) sur stderr.
#
# NOTE angle mort du mode accent (mesuré 2026-07-26, fermé par le NOYAU) :
#   le mode accent ne voyait QUE le français ACCENTUÉ. Or « vault_id vide » et
#   « trop long ({} > {} octets) » ne portent aucun diacritique : 7 littéraux FR
#   sont ainsi partis vers la surface publique v1.0.0 sans jamais faire rougir le
#   gate. Le noyau FR_UNACCENTED ferme ce trou et tourne DANS LES DEUX MODES —
#   c'est indispensable, car les 3 appelants (les 2 ci.yml + release-readiness-
#   scan.sh) invoquent le script SANS --mode, donc toujours en accent.
#
# NOTE limite STRUCTURELLE — détection par mot ⇒ familles à moitié vues :
#   ce gate reconnaît des MOTS, pas des PHRASES. Deux messages qui disent la même
#   chose avec un vocabulaire différent sont, pour lui, deux objets sans lien : il
#   rougit sur celui dont le mot est au noyau et laisse passer ses jumeaux. Mesuré
#   deux fois le 2026-07-26 — « chunks_exact garantit 4 OCTETS » détecté, quatre
#   « … 4 BYTES » invisibles ; « doit commencer par 'code-' » invisible à côté de
#   quatre frères déjà anglicisés. Le gate était VERT avec du français dedans.
#   ⇒ Conséquence opératoire : un hit ne se corrige JAMAIS seul. Après toute
#   correction, chercher la famille par le FRAGMENT PRIVÉ DU MOT DÉCLENCHEUR
#   (`grep -rn "chunks_exact" `, `grep -rn "commencer par"`), pas par le mot qui a
#   fait rougir. Un noyau élargi réduit la fenêtre, il ne ferme pas la classe.
#
# NOTE homographes (mode full uniquement) : un mot orthographié à l'identique en
#   anglais a un pouvoir discriminant NUL — il ne signale pas du français, il
#   fabrique du bruit, et un gate bruyant devient un gate qu'on ignore (leçon
#   01KYEW55BZ, 3e mode de mensonge d'un gate : le rouge permanent). Ces mots sont
#   donc SORTIS du noyau et confinés à --mode full, réservé à l'audit manuel où un
#   humain lit les hits. Cas d'espèce : « impossible » constituait l'UNIQUE hit
#   `full` du workspace, sur une phrase anglaise (gradatum-dto/src/mcp_schema.rs) —
#   faux positif attendu en mode full, absent du mode par défaut.
#   « invalide » n'est PAS un homographe : l'anglais s'écrit « invalid », sans -e.
#
# PREUVE DE DÉTECTION : un gate dont on n'a jamais vu l'échec n'est pas un gate.
#   Vérifier périodiquement les DEUX sens — passe sur surface propre, ET échoue
#   quand on réintroduit un défaut. Recette (à dérouler sur un fichier temporaire) :
#     printf '/// Cette phrase est en français (cf. F-42).\npub fn x() {}\n' \
#       >> crates/gradatum-core/src/lib.rs
#     scripts/scan-fr-strings.sh --surface docs   # DOIT sortir 1 + 2 hits
#     git checkout -- crates/gradatum-core/src/lib.rs
#
#   Le MÉCANISME D'EXEMPTION est lui-même un vecteur de mensonge : il se prouve en
#   trois temps, dont le troisième est celui qu'on rate.
#     1. marqueur posé   -> le hit visé disparaît (exit 0)
#     2. marqueur retiré -> le hit revient        (exit 1)
#     3. marqueur posé + DÉFAUT DIFFÉRENT sur la même ligne (un mot français, ou un
#        second jeton de jargon non nommé) -> le gate DOIT encore échouer (exit 1).
#   Mesuré le 2026-07-26 sur project_map.rs:24 — 0 / 1 / 1 (français injecté sur la
#   ligne exemptée) / 1 (second jeton `Phase 3` injecté). Rejouer après toute
#   modification du bloc « Exemptions inline ».
#
# Exit codes : 0 = aucun hit ; 1 = au moins un hit ; 2 = erreur d'usage / gate non exécuté.
#
# MARQUEUR DE VERDICT — dernière ligne de stdout, INCONDITIONNELLE (pas besoin de -v) :
#
#     # VERDICT=PASS      (aucun hit)
#     # VERDICT=FAIL      (au moins un hit)
#
#   Raison d'être : le code de sortie est le SEUL verdict de ce gate, et il se perd
#   dès qu'on tuyaute. `scan-fr-strings.sh | tail` rend le `$?` de `tail`, soit 0
#   quoi qu'il arrive — une sortie pleine de hits se lit alors comme un succès. Le
#   piège a coûté une déclaration de phase verte erronée le 2026-08-01, sur ce
#   script précisément. Le marqueur donne à un lecteur humain ou à un consommateur
#   en aval un verdict qui survit au tube.
#
#   Il n'AJOUTE aucune logique : `hits` reste l'unique source, lue par le marqueur
#   ET par `sys.exit(1 if hits else 0)` qui demeure le juge. Les deux ne peuvent
#   donc pas diverger par construction — c'est la propriété à préserver si ce bloc
#   est un jour retouché.
#
#   ⚠️ ABSENCE de marqueur ≠ PASS. Les sorties en exit 2 (usage invalide, périmètre
#   non établi, marqueur d'exemption mal formé, auto-test du noyau en échec) se
#   produisent AVANT ce point et n'impriment aucun VERDICT : le gate n'a pas rendu
#   de jugement. Un consommateur doit donc exiger `# VERDICT=PASS` présent, jamais
#   se contenter de l'absence de `# VERDICT=FAIL` — fail-closed.
set -euo pipefail
export LC_ALL=C.UTF-8

SURFACE="both"
SCOPE="all"
MODE="accent"
JARGON="on"
ONLY_CRATE=""
VERBOSE=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --surface) SURFACE="${2:-}"; shift 2 ;;
    --crate) ONLY_CRATE="${2:-}"; shift 2 ;;
    --scope) SCOPE="${2:-}"; shift 2 ;;
    --mode)  MODE="${2:-}";  shift 2 ;;
    --jargon) JARGON="${2:-}"; shift 2 ;;
    -v|--verbose) VERBOSE=1; shift ;;
    -h|--help) grep -E '^#( |$)' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "usage: $0 [--surface strings|docs|both] [--crate NAME] [--scope product|all] [--mode accent|full] [--jargon on|off] [-v]" >&2; exit 2 ;;
  esac
done
[[ "$SURFACE" == "strings" || "$SURFACE" == "docs" || "$SURFACE" == "both" ]] \
  || { echo "surface invalide: $SURFACE" >&2; exit 2; }
[[ "$SCOPE" == "product" || "$SCOPE" == "all" ]] || { echo "scope invalide: $SCOPE" >&2; exit 2; }
[[ "$MODE" == "accent" || "$MODE" == "full" ]]   || { echo "mode invalide: $MODE" >&2; exit 2; }
[[ "$JARGON" == "on" || "$JARGON" == "off" ]]    || { echo "jargon invalide: $JARGON" >&2; exit 2; }

# Racine du repo = parent du dossier scripts/
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

python3 - "$SCOPE" "$MODE" "$VERBOSE" "$SURFACE" "$JARGON" "$ONLY_CRATE" <<'PYEOF'
import json, os, re, subprocess, sys

scope, mode, verbose = sys.argv[1], sys.argv[2], sys.argv[3] == "1"
surface, jargon_on, only_crate = sys.argv[4], sys.argv[5] == "on", sys.argv[6]
want_str = surface in ("strings", "both")
want_doc = surface in ("docs", "both")

# --- Périmètre fichiers ---------------------------------------------------
EXCLUDED_CRATES = {"gradatum-bench", "v1-parity-tests", "index-parity-tests"}
# Surface "product" : crates directement touchées par un consommateur externe
# (client/HTTP) ou opérateur, + API publique de base.
PRODUCT_CRATES = {
    "gradatum", "gradatum-core", "gradatum-dto", "gradatum-cli",
    "gradatum-server", "gradatum-gateway", "gradatum-sdk-rs",
    "gradatum-mcp-stub", "gradatum-admin", "gradatum-worker",
}

def crate_of(path):
    parts = path.split(os.sep)
    if "crates" in parts:
        i = parts.index("crates")
        if i + 1 < len(parts):
            return parts[i + 1]
    return ""

def included(path):
    if not path.endswith(".rs"):
        return False
    if "/tests/" in path or os.sep + "tests" + os.sep in path:
        return False
    base = os.path.basename(path)
    if base.endswith("_test.rs") or base.endswith("_tests.rs"):
        return False
    c = crate_of(path)
    if c in EXCLUDED_CRATES:
        return False
    if scope == "product" and c not in PRODUCT_CRATES:
        return False
    if only_crate and c != only_crate:
        return False
    return True

# --- Surface docs.rs : crates PUBLIABLES via cargo metadata ----------------
# `grep publish` est proscrit (3 faux positifs sur ce dépôt). Liste vide =>
# le gate n'a pas tourné : exit 2, jamais un PASS (leçon 01KYEW55BZ, cas n°1).
def cargo_metadata():
    try:
        raw = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            capture_output=True, text=True, timeout=120, check=True,
        ).stdout
    except (OSError, subprocess.SubprocessError) as exc:
        print(f"ERREUR : `cargo metadata` indisponible ({exc}) — gate docs NON EXÉCUTÉ, "
              f"pas un PASS", file=sys.stderr)
        sys.exit(2)
    return json.loads(raw)

LIB_KINDS = {"lib", "rlib", "dylib", "cdylib", "proc-macro"}

def lib_targets():
    """{crate: chemin de la racine du target LIB} pour les crates publiables.

    docs.rs ne rend QUE la bibliothèque : ni `build.rs`, ni les binaires
    (`src/main.rs` et les modules qu'ils sont seuls à déclarer). Une crate
    publiable sans target lib ne produit aucune page docs.rs.
    """
    roots = {}
    for p in cargo_metadata()["packages"]:
        if p.get("publish") is not None:      # publish = [] => non publiable
            continue
        for t in p.get("targets", []):
            if LIB_KINDS & set(t.get("kind", [])):
                roots[p["name"]] = os.path.relpath(t["src_path"], os.getcwd())
                break
    return roots

# Déclaration de module hors-ligne : `[attrs] [pub] mod NAME;`
MOD_DECL = re.compile(
    r"(?P<attrs>(?:#\[[^\]]*\]\s*)*)"
    r"(?:pub\s*(?:\([^)]*\)\s*)?)?"             # pub / pub(crate) / pub(in …)
    r"\bmod\s+(?P<name>[A-Za-z_]\w*)\s*;"
)
DOC_HIDDEN_ATTR = re.compile(r"#\[\s*doc\s*\(\s*hidden\s*\)\s*\]")
PATH_ATTR = re.compile(r"#\[\s*path\s*=\s*\"([^\"]+)\"\s*\]")

def rendered_files(roots):
    """Ensemble des fichiers atteignables depuis chaque racine lib, en élaguant
    les sous-arbres `#[doc(hidden)] mod x;` (non rendus par rustdoc)."""
    seen = set()
    per_crate = {}
    for crate, root in roots.items():
        stack = [root] if os.path.exists(root) else []
        local = set()
        while stack:
            path = stack.pop()
            if path in local:
                continue
            local.add(path)
            try:
                src = open(path, "r", encoding="utf-8").read()
            except (UnicodeDecodeError, OSError):
                continue
            d = os.path.dirname(path)
            # `foo.rs` héberge ses sous-modules dans `foo/` (edition >= 2018) ;
            # `foo/mod.rs` les héberge à côté de lui.
            base = os.path.basename(path)
            sub = d if base == "mod.rs" or path == root else os.path.join(d, base[:-3])
            for m in MOD_DECL.finditer(src):
                attrs = m.group("attrs")
                if DOC_HIDDEN_ATTR.search(attrs):
                    continue                      # sous-arbre non rendu : élagué
                pm = PATH_ATTR.search(attrs)
                if pm:
                    cands = [os.path.normpath(os.path.join(d, pm.group(1)))]
                else:
                    name = m.group("name")
                    cands = [os.path.join(sub, name + ".rs"),
                             os.path.join(sub, name, "mod.rs")]
                for cand in cands:
                    if os.path.exists(cand):
                        stack.append(cand)
                        break
        per_crate[crate] = local
        seen |= local
    return seen, per_crate

# Fail-closed : un nom de crate erroné ne doit JAMAIS produire un exit 0 « aucun hit »
# sur un périmètre vide. Cette garde était conditionnée à `want_doc` : en
# `--surface strings --crate <mal-orthographié>`, le périmètre était vide et le gate
# sortait 0 — l'inverse exact de ce que promettait son propre commentaire.
if only_crate and not os.path.isdir(os.path.join("crates", only_crate)):
    print(f"ERREUR : crate inconnue « {only_crate} » — gate NON EXÉCUTÉ, pas un PASS",
          file=sys.stderr)
    sys.exit(2)

LIB_ROOTS = lib_targets() if want_doc else {}
if only_crate and want_doc:
    LIB_ROOTS = {k: v for k, v in LIB_ROOTS.items() if k == only_crate}
    if not LIB_ROOTS:
        print(f"# {only_crate} : aucun target lib (binaire pur) — 0 page docs.rs",
              file=sys.stderr)
if want_doc and not LIB_ROOTS and not only_crate:
    print("ERREUR : aucune crate publiable avec target lib — gate docs NON EXÉCUTÉ, "
          "pas un PASS", file=sys.stderr)
    sys.exit(2)
PUBLISHABLE = set(LIB_ROOTS)
RENDERED, RENDERED_PER_CRATE = rendered_files(LIB_ROOTS) if want_doc else (set(), {})

# --- Détection FR ---------------------------------------------------------
# Classe accentués : Latin-1 À(U+00C0)..ÿ(U+00FF) hors × ÷, + ligatures œ Œ Ÿ.
ACCENT = re.compile(r"[À-ÿ]")
ACCENT_EXCLUDE = {"×", "÷"}  # × ÷
LIGATURES = set("ŒœŸ")  # Œ œ Ÿ
# NOYAU FR sans accent — actif dans LES DEUX modes (donc en CI).
#
# Règle de sélection, mécanique et non « au jugé » : un mot n'entre ici que s'il
#   (a) apparaît plausiblement dans un message d'erreur / de validation FR, ET
#   (b) n'a AUCUNE orthographe identique en anglais.
# (b) est ce qui rend le noyau utilisable comme gate bloquant : zéro homographe
# => zéro bruit structurel. Tout mot violant (b) va dans HOMOGRAPH_PRONE.
#
# Les délimiteurs (?<![\w-]) / (?![\w-]) sont load-bearing, pas décoratifs : ils
# empêchent « pro-vide-d », « en-trop-y », « vide-o » et « octet-stream » de
# matcher. Les tester est le seul moyen de le savoir : l'auto-test ci-dessous
# tourne à CHAQUE run et sort en 2 (« gate défectueux ») s'il tombe.
FR_UNACCENTED = re.compile(
    r"(?<![\w-])(?:"
    # ── angle mort mesuré : les mots des 7 littéraux partis en v1.0.0 ──
    # « octets » a été RETIRÉ du noyau le 2026-07-26 : c'est un homographe EN/FR
    # d'usage standard (RFC 791 et suivantes disent « octet » pour « byte »), donc
    # une violation directe du critère (b) ci-dessus. Il produisait un faux positif
    # PERMANENT sur `gradatum-chat/src/lib.rs` (« RFC 1918 class C: first octet 192 »),
    # phrase anglaise et correcte, pendant que son unique vrai positif historique
    # (`trop long ({} > {} octets)`) reste couvert par « trop ». Un mot au pouvoir
    # discriminant nul ne signale rien : il fabrique le rouge permanent (3e mode de
    # mensonge d'un gate, leçon 01KYEW55BZ). Il vit désormais dans HOMOGRAPH_PRONE,
    # donc reste visible en `--mode full` (audit manuel). Le retour en arrière est
    # verrouillé par l'entrée RFC 1918 de _MUST_NOT_MATCH.
    r"vides?|trop|"
    r"interdit(?:es|e|s)?|inconnu(?:es|e|s)?|requis(?:es|e)?|"
    # ── 2e angle mort mesuré (2026-07-26) : la détection PAR MOT signale UN
    # membre d'une famille de messages et rate ses jumeaux, qui disent la même
    # chose avec d'autres mots. « octets » avait fait rougir un `chunks_exact
    # garantit 4 octets` en laissant vivre 4 jumeaux en « bytes » ; « doit » ne
    # figurait nulle part alors que 2 gardes « doit commencer par 'code-' »
    # survivaient à côté de leurs 4 frères déjà anglicisés. Blast radius mesuré
    # AVANT ajout, sur --surface strings --scope all : garantit=3 hits, doit=2
    # hits — 5/5 vrais positifs, 0 faux positif, 0 hit doc-comment. Le bruit
    # redouté de « doit » (messages d'assert des tests) n'atteint PAS ce gate :
    # les régions #[cfg(test)] et l'arborescence tests/ sont déjà élaguées.
    # « connexion » : l'anglais s'écrit « connection », avec -tion — donc pas un
    # homographe. Trouvé par la méthode ci-dessous (grep du fragment « retry »
    # privé du mot déclencheur), pas par le gate : le chemin GET portait encore
    # « connexion/timeout → retry » à côté du chemin POST déjà anglicisé.
    r"garantit|doit|connexions?|"
    # ── noyau historique (conservé, + variantes genre/nombre) ──
    r"manquant(?:es|e|s)?|attendu(?:es|e|s)?|inattendu(?:es|e|s)?|"
    r"invalides?|introuvables?|corrompu(?:es|e|s)?|echoue(?:es|e|s)?|"
    r"aucun(?:es|e|s)?|fichiers?|cles?|recu(?:es|e|s)?|deja|etre|ete"
    r")(?![\w-])",
    re.IGNORECASE,
)
# Homographes EN/FR — pouvoir discriminant nul, réservés à --mode full (audit
# manuel). Ne JAMAIS remonter dans le noyau : c'est ce qui a produit le seul hit
# `full` du workspace, un faux positif sur une phrase anglaise.
HOMOGRAPH_PRONE = re.compile(
    r"(?<![\w-])(?:impossible|refuse|octets?)(?![\w-])",
    re.IGNORECASE,
)

# Auto-test des délimiteurs du noyau (preuve de détection embarquée). Un gate qui
# ne détecte plus rien doit tomber BRUYAMMENT, jamais rendre un PASS silencieux.
_MUST_MATCH = ("vault_id vide", "trop long ({} > {} octets)", "locus vide",
               "charset interdit", "champ requis", "tenant inconnu",
               "valeur manquante", "fichiers introuvables",
               "chunks_exact garantit 4 bytes",
               "vault_id '{vault_id}' doit commencer par 'code-'",
               "connexion/timeout → retry")
# Les deux dernières entrées sont les traductions EN effectivement retenues pour
# les familles ci-dessus : les épingler ici verrouille le fait qu'anglicier ces
# messages ÉTEINT bien le gate — sans quoi la correction serait invérifiable.
_MUST_NOT_MATCH = ("provided by the caller", "video stream", "entropy pool",
                   "application/octet-stream", "isotropic", "divided",
                   "invalid input", "completed",
                   "chunks_exact guarantees 4 bytes — invariant",
                   "vault_id '{vault_id}' must start with 'code-'",
                   "connection/timeout error — retry",
                   # Anglais RFC : verrouille le retrait d'« octets » du noyau.
                   # Réintroduire le mot ici fait ÉCHOUER le gate en exit 2 au lieu
                   # de laisser revenir un rouge permanent sur une phrase correcte.
                   "RFC 1918 class C: first octet 192, second octet 168")
for _s in _MUST_MATCH:
    if not FR_UNACCENTED.search(_s):
        print(f"ERREUR : auto-test du noyau FR — « {_s} » aurait dû matcher. "
              f"Gate DÉFECTUEUX, pas un PASS.", file=sys.stderr)
        sys.exit(2)
for _s in _MUST_NOT_MATCH:
    if FR_UNACCENTED.search(_s):
        print(f"ERREUR : auto-test du noyau FR — « {_s} » est anglais et n'aurait "
              f"PAS dû matcher. Gate BRUYANT, corriger les délimiteurs.", file=sys.stderr)
        sys.exit(2)
# Jargon interne : références de plan/gouvernance homelab sans aucun sens pour un
# lecteur de docs.rs. `§` est flaggé nu (renvoi de spec interne dans tous les cas).
JARGON = re.compile(
    r"(?<![\w-])(?:F-\d{2,3}|EX-C\d|Lot \d|Phase \d|R-\d|P0-\d|v81)(?![\w-])"
    r"|§"
)

# --- Exemptions inline ----------------------------------------------------
# Un jeton de jargon peut être un EXEMPLE DE FORMAT et non une référence interne :
# `[[feature:F-37]]` documente la SYNTAXE d'un wikilink, `Valid: F-37` documente le
# contrat d'un validateur. Les retirer viderait la doc de son sens ; les laisser
# rougir indéfiniment fait un gate qu'on ignore. On les exempte donc UN PAR UN.
#
# Syntaxe (calquée sur `# lint-toolchain-pin: allow nightly — <raison>` de
# scripts/ci-lint-toolchain-pin.sh, même esprit : exception nommée + motivée + imprimée) :
#
#   // scan-fr-strings: allow-jargon <JETON> — <raison non vide>
#   /// … le jeton <JETON> …
#
# Trois propriétés load-bearing, pas décoratives :
#
#  1. PORTÉE = LE JETON, PAS LA LIGNE. Le marqueur nomme le jeton exact qu'il
#     autorise. Un AUTRE jeton de jargon sur la même ligne reste un hit, et le
#     français sur cette ligne aussi — voir (3).
#  2. UN MARQUEUR NE PEUT JAMAIS TAIRE DU FRANÇAIS. Il n'existe pas de
#     `allow-fr` : le seul verbe est `allow-jargon`, consulté uniquement dans la
#     branche jargon, elle-même subordonnée à `is_fr()` qui tranche en premier.
#     L'échappatoire est donc structurellement bornée au jargon.
#  3. LE MARQUEUR VIT SUR UN COMMENTAIRE ORDINAIRE `//`, LIGNE DU DESSUS. Jamais
#     dans le `///` : rustdoc rendrait le marqueur sur docs.rs, c'est-à-dire
#     publierait du jargon de gate sur la surface même que le gate protège. Un
#     `scan-fr-strings:` trouvé dans un `///`/`//!` est donc une ERREUR (exit 2),
#     pas une exemption silencieusement ignorée.
#
# Alternative écartée — « un jeton entre backticks est un exemple » : elle couvre
# 10 des 11 cas sans annotation (mesuré), mais son pouvoir discriminant est NUL
# pour la distinction qu'elle prétend faire. `F-37` en exemple de format et
# `F-114` en référence interne sont typographiquement identiques dans un code
# span — or un lecteur de docs.rs ne comprend pas plus le second entouré de
# backticks. La règle transformerait le rouge permanent en VERT permanent, avec
# une échappatoire qu'il suffit d'ouvrir en tapant deux backticks. C'est le même
# raisonnement qui a sorti les homographes du noyau FR (cf. NOTE homographes).
MARKER_RE = re.compile(
    r"scan-fr-strings:\s*allow-jargon\s+(?P<token>\S+)\s*(?:—|--|:)\s*(?P<reason>\S.*?)\s*$"
)
MARKER_ANY = re.compile(r"scan-fr-strings:")
ORDINARY_COMMENT = re.compile(r"^\s*//(?![/!])")


def read_markers(path):
    """{ligne_cible: {jeton: raison}} — un marqueur couvre la 1re ligne NON-marqueur
    qui le suit. Les marqueurs s'EMPILENT : une ligne portant deux jetons de jargon
    distincts (`Valid: F-37, F-061`) exige deux marqueurs, un par jeton, chacun avec
    sa raison. Aucun marqueur ne couvre plus d'un jeton.

    Fail-closed : tout `scan-fr-strings:` mal formé (raison vide, verbe inconnu)
    ou placé dans un doc-comment sort en 2. Une exemption qu'on croit posée mais
    qui ne l'est pas est exactement le mensonge que ce mécanisme doit éviter.
    """
    try:
        lines = open(path, "r", encoding="utf-8").read().splitlines()
    except (UnicodeDecodeError, OSError):
        return {}
    at = {}
    for idx, raw in enumerate(lines, start=1):
        if not MARKER_ANY.search(raw):
            continue
        if not ORDINARY_COMMENT.match(raw):
            print(f"ERREUR : {path}:{idx} — marqueur `scan-fr-strings:` hors d'un "
                  f"commentaire ordinaire `//`. Dans un `///`/`//!` il serait RENDU "
                  f"sur docs.rs. Gate DÉFECTUEUX, pas un PASS.", file=sys.stderr)
            sys.exit(2)
        m = MARKER_RE.search(raw)
        if not m:
            print(f"ERREUR : {path}:{idx} — marqueur `scan-fr-strings:` mal formé. "
                  f"Attendu : `// scan-fr-strings: allow-jargon <JETON> — <raison>`. "
                  f"Gate DÉFECTUEUX, pas un PASS.", file=sys.stderr)
            sys.exit(2)
        at[idx] = (m.group("token"), m.group("reason"))
    out = {}
    for idx, (tok, reason) in at.items():
        target = idx + 1
        while target in at:               # pile de marqueurs → même ligne cible
            target += 1
        out.setdefault(target, {})[tok] = reason
    return out

def is_fr(s):
    for ch in s:
        if ch in LIGATURES:
            return True
        if ACCENT.match(ch) and ch not in ACCENT_EXCLUDE:
            return True
    if FR_UNACCENTED.search(s):
        return True
    if mode == "full" and HOMOGRAPH_PRONE.search(s):
        return True
    return False

# --- Lexer Rust : littéraux de chaîne + doc-comments, hors commentaires/tests ---
def scan_file(path, doc_ok):
    """Yield (kind, lineno, content) — kind ∈ {"str","doc"}.

    Hors commentaires ordinaires et hors module/fn de test inline. Les `///`
    sont bufferisés jusqu'à l'item auquel ils s'attachent, afin de pouvoir les
    supprimer si cet item porte `#[doc(hidden)]` (non rendu par rustdoc).
    """
    try:
        src = open(path, "r", encoding="utf-8").read()
    except (UnicodeDecodeError, OSError):
        return
    i, n = 0, len(src)
    line = 1
    brace_depth = 0
    skip_from = None          # profondeur d'accolade où débute une région de test
    pending_test = False      # attribut #[cfg(test)]/#[test] vu, en attente du bloc
    block_depth = 0           # commentaires /* */ imbriqués
    attr_depth = 0            # profondeur d'attribut #[...] en cours de lexage
    pending_doc = []          # `///` en attente de l'item auquel ils s'attachent
    pending_hidden = False    # #[doc(hidden)] vu avant cet item

    out = []

    def in_skip():
        return skip_from is not None

    def flush_doc():
        nonlocal pending_doc, pending_hidden
        if pending_doc and not pending_hidden:
            out.extend(("doc", ln, txt) for ln, txt in pending_doc)
        pending_doc = []
        pending_hidden = False

    while i < n:
        c = src[i]
        # commentaire bloc /* ... */ (imbricable)
        if block_depth > 0:
            if c == "\n":
                line += 1; i += 1; continue
            if src.startswith("/*", i):
                block_depth += 1; i += 2; continue
            if src.startswith("*/", i):
                block_depth -= 1; i += 2; continue
            i += 1; continue
        # commentaire ligne — c'est ICI que les doc-comments étaient perdus :
        # l'ancien gate sautait jusqu'au \n sans jamais regarder le contenu.
        if src.startswith("//", i):
            j = src.find("\n", i)
            if j == -1:
                j = n
            seg = src[i:j]
            if doc_ok and not in_skip():
                if seg.startswith("//!"):
                    # doc INTERNE : s'attache au module englobant, jamais à un item
                    # suivant => émission directe, hors bufferisation.
                    out.append(("doc", line, seg[3:].strip()))
                elif seg.startswith("///") and not seg.startswith("////"):
                    # `////` n'est PAS un doc-comment pour rustdoc.
                    pending_doc.append((line, seg[3:].strip()))
            i = j; continue
        if src.startswith("/*", i):
            block_depth = 1; i += 2; continue
        if c == "\n":
            line += 1; i += 1; continue
        # attribut #[...] : on note son intention puis on le LEXE normalement
        # (les `#[error("…")]` sont ainsi capturés). attr_depth évite qu'un
        # identifiant d'attribut ne déclenche le flush des doc-comments.
        if src.startswith("#[", i):
            head = src[i:i + 64]
            if re.match(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]", head) or \
               re.match(r"#\[\s*(?:tokio::|async_std::|rstest::)?test\s*\]", head) or \
               re.match(r"#\[\s*rstest\b", head):
                pending_test = True
            if re.match(r"#\[\s*doc\s*\(\s*hidden\s*\)\s*\]", head):
                pending_hidden = True
            attr_depth += 1; i += 2; continue
        if c == "]" and attr_depth > 0:
            attr_depth -= 1; i += 1; continue
        # premier caractère significatif hors attribut = début de l'item porteur
        # des `///` bufferisés => on tranche maintenant (rendu ou masqué).
        if attr_depth == 0 and not c.isspace() and (pending_doc or pending_hidden):
            flush_doc()
        # littéral de caractère 'x' / '\n' / '"' (NE PAS confondre avec un tick
        # de lifetime 'a ni laisser '"' ouvrir une fausse chaîne)
        if c == "'":
            chm = re.match(r"'(?:\\.|[^'\\\n])'", src[i:])
            if chm:
                i += chm.end(); continue
            i += 1; continue  # tick de lifetime
        # raw string : r"..."  r#"..."#  br#"..."# ...
        rawm = re.match(r'(b?r)(#*)"', src[i:])
        if rawm:
            hashes = rawm.group(2)
            close = '"' + hashes
            start_content = i + rawm.end()
            j = src.find(close, start_content)
            if j == -1:
                break
            content = src[start_content:j]
            sline = line
            line += content.count("\n")
            if not in_skip():
                out.append(("str", sline, content))
            i = j + len(close); continue
        # string normale "..." (ou byte b"...")
        if c == '"' or (c == "b" and src.startswith('b"', i)):
            if c == "b":
                i += 1
            i += 1  # consomme le guillemet ouvrant
            buf = []
            sline = line
            while i < n:
                d = src[i]
                if d == "\\":
                    if i + 1 < n and src[i+1] == "\n":
                        line += 1
                    buf.append(src[i:i+2]); i += 2; continue
                if d == "\n":
                    line += 1; buf.append(d); i += 1; continue
                if d == '"':
                    i += 1; break
                buf.append(d); i += 1
            if not in_skip():
                out.append(("str", sline, "".join(buf)))
            continue
        # accolades — gestion des régions de test
        if c == "{":
            if pending_test and not in_skip():
                skip_from = brace_depth
                pending_test = False
            brace_depth += 1; i += 1; continue
        if c == "}":
            brace_depth -= 1
            if in_skip() and brace_depth == skip_from:
                skip_from = None
            i += 1; continue
        if c == ";":
            # attribut de test sur un item sans bloc (use/mod;) → annuler
            if pending_test:
                pending_test = False
            i += 1; continue
        i += 1

    flush_doc()
    out.sort(key=lambda t: t[1])
    for item in out:
        yield item

# --- Parcours -------------------------------------------------------------
# Fail-closed sur la surface `strings`, symétrique de celui de la surface `docs`.
# `os.walk` sur un répertoire ABSENT ne lève pas : il itère zéro fois. Le périmètre
# restait donc vide, `hits` vide, et `sys.exit(1 if hits else 0)` rendait 0 « aucun
# hit » — un PASS pour zéro fichier examiné. C'est le chemin exercé par la CI
# (`--surface strings` / `both`), et le seul qui n'avait AUCUNE garde.
if want_str and not os.path.isdir("crates"):
    print("ERREUR : répertoire « crates » introuvable depuis "
          f"{os.getcwd()} — gate strings NON EXÉCUTÉ, pas un PASS", file=sys.stderr)
    sys.exit(2)

str_paths = set()
for root, dirs, files in os.walk("crates"):
    for f in files:
        p = os.path.join(root, f)
        if included(p):
            str_paths.add(p)

if want_str and not str_paths:
    print("ERREUR : 0 fichier .rs retenu sur la surface strings "
          f"(scope={scope}, crate={only_crate or '(toutes)'}) — gate NON EXÉCUTÉ, "
          "pas un PASS", file=sys.stderr)
    sys.exit(2)

# La surface docs est celle de l'arbre de modules lib, PAS le walk de fichiers :
# elle peut donc inclure un fichier que le filtre `strings` exclut, et inversement.
doc_paths = set(RENDERED)
if scope == "product":
    doc_paths = {p for p in doc_paths if crate_of(p) in PRODUCT_CRATES}

hits = []
nstr_files = 0
granted = {}    # (path, lineno, jeton) -> raison  : exemptions effectivement APPLIQUÉES
declared = set()  # (path, lineno, jeton)          : exemptions ÉCRITES dans les sources
for p in sorted(str_paths | doc_paths):
    doc_ok = want_doc and p in doc_paths
    str_ok = want_str and p in str_paths
    if not doc_ok and not str_ok:
        continue
    if str_ok:
        nstr_files += 1
    markers = read_markers(p)
    for ln, toks in markers.items():
        for tok in toks:
            declared.add((p, ln, tok))
    for kind, lineno, content in scan_file(p, doc_ok):
        if kind == "str" and not str_ok:
            continue
        tag = None
        # Ordre load-bearing : le français tranche EN PREMIER et ne consulte
        # aucun marqueur. Une ligne exemptée pour son jargon qui contient aussi
        # du français reste donc un hit — l'exemption ne peut pas la couvrir.
        if is_fr(content):
            tag = "FR"
        elif kind == "doc" and jargon_on:
            allowed = markers.get(lineno, {})
            # finditer, pas search : un marqueur ne fait sauter QUE son jeton.
            # Le jeton suivant, non exempté, redevient le hit de la ligne.
            for m in JARGON.finditer(content):
                tok = m.group(0)
                if tok in allowed:
                    granted[(p, lineno, tok)] = allowed[tok]
                    continue
                tag = f"JARGON:{tok}"
                break
        if tag is None:
            continue
        flat = content.replace("\n", "\\n")
        snippet = flat if len(flat) <= 120 else flat[:117] + "..."
        hits.append((p, lineno, kind, tag, snippet))

# Les exemptions accordées sont IMPRIMÉES à chaque run (comme le lint toolchain-pin) :
# une exception invisible redevient une échappatoire. Une exemption déclarée mais
# jamais appliquée est signalée — c'est ainsi que ce mécanisme pourrit (le code bouge,
# le marqueur reste et donne l'illusion d'une couverture).
for (p, ln, tok), reason in sorted(granted.items()):
    print(f"# ~ exemption jargon · {p}:{ln} ({tok}) — {reason}", file=sys.stderr)
for p, ln, tok in sorted(declared - set(granted)):
    print(f"# ~ exemption INUTILISÉE · {p}:{ln} ({tok}) — le jeton n'apparaît plus "
          f"sur la ligne suivante ; retirer le marqueur", file=sys.stderr)

hits.sort()
for p, ln, kind, tag, s in hits:
    print(f"{p}:{ln}:[{kind}/{tag}] {s}")
if verbose:
    # Périmètre PROUVÉ, pas annoncé : nombre d'unités effectivement scannées.
    print(f"# surface={surface} scope={scope} mode={mode} "
          f"jargon={'on' if jargon_on else 'off'}", file=sys.stderr)
    if want_str:
        print(f"# strings : {nstr_files} fichiers scannés", file=sys.stderr)
    if want_doc:
        print(f"# docs.rs : {len(doc_paths)} fichiers rendus, atteints depuis "
              f"{len(LIB_ROOTS)} racines lib / {len(PUBLISHABLE)} crates publiables "
              f"(arbre de modules, sous-arbres #[doc(hidden)] élagués)", file=sys.stderr)
        for c in sorted(RENDERED_PER_CRATE):
            n = len(RENDERED_PER_CRATE[c])
            print(f"#   {c}: {n} fichier(s) rendu(s)", file=sys.stderr)
    n_fr = sum(1 for h in hits if h[3] == "FR")
    print(f"# hits={len(hits)} (FR={n_fr}, jargon={len(hits) - n_fr})", file=sys.stderr)

# Verdict lisible, sur stdout, sans condition : un code de sortie ne survit pas à un
# tube (`… | tail` rend le statut de `tail`). Même source que la sortie ci-dessous —
# `hits` — donc marqueur et code de sortie ne peuvent pas se contredire. Ne pas
# déplacer après le sys.exit, ni le conditionner à `verbose`.
print(f"# VERDICT={'FAIL' if hits else 'PASS'}")
sys.exit(1 if hits else 0)
PYEOF
