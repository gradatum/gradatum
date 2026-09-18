#!/usr/bin/env bash
#
# assert-parity.sh — compare deux relevés de mesures produits par
# `run-upgrade-test.sh` (`--before` / `--after`) et rend un verdict NOMMÉ.
#
# Ce script ne connaît AUCUN binaire, aucun port, aucun serveur : il ne lit que
# des fichiers. Il est donc rejouable seul, sur deux relevés conservés.
#
# Ce qu'il compare, et à quelle force :
#
#   (a) `_sqlx_migrations` (queue.sqlite) — ligne à ligne, `installed_on` et
#       `checksum` compris. Une réapplication change `installed_on` : elle est
#       donc vue, là où une comparaison limitée à `(version, checksum, success)`
#       la laisserait passer. Aucune ligne `success != 1`. BLOQUANT.
#   (b) `_schema_migrations` (index.db) — même logique. La clé est PLUS STRICTE
#       que la lettre du brief (qui ne nomme que `_sqlx_migrations`) : elle est
#       ajoutée délibérément, parce que c'est l'autre moitié des migrations et
#       qu'une montée qui appliquerait une migration d'index sans l'écrire dans
#       `_sqlx_migrations` doit se voir. BLOQUANT.
#   (c) `vault_list` — identifiants dans l'ordre rendu, PLUS le tri. BLOQUANT.
#   (d) les 5 `vault_search` — identifiants dans l'ordre rendu, requête par
#       requête. BLOQUANT.
#   (e) `export-features` — deux forces, choisies par `--export-mode` :
#
#       `strict` (défaut) — octet à octet (sha256 + `cmp`). C'est la force du
#         RETOUR ARRIÈRE : les deux relevés sont produits par le même binaire
#         (2.4.0), donc rien ne doit bouger, pas même un champ. BLOQUANT.
#
#       `additif` — la force de la MONTÉE. L'export est un RAPPORT : entre deux
#         versions son dialecte peut GAGNER des clés, et c'est une évolution
#         voulue, pas une divergence. La règle appliquée :
#
#           • chaque entrée du relevé AVANT doit être ÉGALE à l'entrée du relevé
#             MONTÉ **projetée sur les clés du dialecte AVANT** (même nombre de
#             cartes, même ordre) ;
#           • les clés que le MONTÉ ajoute sont RELEVÉES (`cles_ajoutees=…`) et
#             doivent être citées dans le guide de montée (`--export-doc`) ;
#           • une clé du dialecte AVANT qui DISPARAÎT côté MONTÉ, ou dont la
#             valeur CHANGE, est une divergence nommée. BLOQUANT.
#
#       En mode `additif`, `--export-doc <guide de montée>` est REQUIS : sans lui
#       la clé (e) n'est pas mesurée du tout (`2`), parce qu'un ajout non
#       documenté est précisément ce que cette force existe pour attraper. En mode
#       `strict`, `--export-doc` est REFUSÉ (une comparaison d'octets n'a pas de
#       dialecte à documenter — l'accepter ferait croire à un contrôle qui n'a pas
#       eu lieu).
#   (f) comptes par table — `notes` et `gradatum_jobs` à tolérance zéro (lettre
#       du brief), plus une liste NOMMÉE de tables dérivées du corpus. Toute
#       autre table est REPORTÉE avec son delta, sans bloquer : leur contenu
#       dépend du trafic (télémétrie, journaux d'événements), pas du corpus.
#   (g) la file de travaux — digest ordonné `(id, statut, kind)` : plus strict que
#       le seul compte, et stable tant qu'aucun travailleur ne tourne.
#
# BLOQUANT ne veut pas dire « toujours concluant » : une clé bloquante peut être
# VACUE — les deux côtés rendent la même chose, mais cette chose est vide, donc
# la parité d'ordre n'y est pas un résultat. C'est un fait REPORTÉ (`noté`, sans
# effet sur le verdict, lignes (d) et (e)), jamais un écart : le `/1` du verdict
# porte la sémantique « divergence entre AVANT et APRÈS », pas « couverture
# insuffisante du test ». Une vraie divergence (vide d'un côté, non vide de
# l'autre) reste vue par la comparaison elle-même, qui est bloquante.
#
# Usage :
#   bash tests/upgrade/assert-parity.sh --before <dir> --after <dir> \
#       [--label <texte>] [--out <fichier>] \
#       [--export-mode strict|additif] [--export-doc <guide>]
#
#   --export-mode strict (défaut) : la clé (e) compare les octets — la force du
#       retour arrière.
#   --export-mode additif : la clé (e) compare le dialecte AVANT projeté — la
#       force de la montée ; `--export-doc` est alors REQUIS.
#   --export-doc <fichier> : guide de montée où chaque clé ajoutée doit être
#       citée. Refusé en mode `strict`.
#
# Codes de retour :
#   0  parité établie sur toutes les clés bloquantes
#   1  divergence — chaque écart est nommé, avec les deux valeurs
#   2  NON EXÉCUTÉ — un relevé est absent ou illisible, ou la clé (e) n'a pas pu
#      être mesurée (mode `additif` sans guide) — jamais un vert

set -euo pipefail
umask 077

BEFORE=""
AFTER=""
LABEL=""
OUT=""
EXPORT_MODE="strict"
EXPORT_DOC=""

usage() {
  cat >&2 <<'EOF'
Usage: bash tests/upgrade/assert-parity.sh --before <dir> --after <dir>
           [--label <texte>] [--out <fichier>]
           [--export-mode strict|additif] [--export-doc <guide>]

  --before <dir>   relevé de référence (mesure AVANT)
  --after  <dir>   relevé à comparer (mesure APRÈS)
  --label  <txt>   étiquette de la comparaison (ex. « montée 2.4.0 »)
  --out    <file>  fichier de rapport (défaut : stdout seul)
  --export-mode    force de la clé (e) — `strict` (octet à octet, défaut) ou
                   `additif` (projection sur le dialecte AVANT, montée)
  --export-doc     guide de montée exigeant la citation des clés ajoutées
                   (requis avec `--export-mode additif`, refusé avec `strict`)

Codes : 0 parité · 1 divergence nommée · 2 NON EXÉCUTÉ (relevé absent, ou
clé (e) non mesurable)
EOF
}

while (($#)); do
  case "$1" in
    --before)      BEFORE="${2-}";      shift 2 ;;
    --after)       AFTER="${2-}";       shift 2 ;;
    --label)       LABEL="${2-}";       shift 2 ;;
    --out)         OUT="${2-}";         shift 2 ;;
    --export-mode) EXPORT_MODE="${2-}"; shift 2 ;;
    --export-doc)  EXPORT_DOC="${2-}";  shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'assert-parity: argument inconnu : %s\n' "$1" >&2; exit 2 ;;
  esac
done

[[ -n "$BEFORE" ]] || { printf 'assert-parity: --before est requis\n' >&2; exit 2; }
[[ -n "$AFTER"  ]] || { printf 'assert-parity: --after est requis\n' >&2;  exit 2; }
[[ -d "$BEFORE" ]] || { printf 'assert-parity: NON EXÉCUTÉ — relevé AVANT illisible : %s\n' "$BEFORE" >&2; exit 2; }
[[ -d "$AFTER"  ]] || { printf 'assert-parity: NON EXÉCUTÉ — relevé APRÈS illisible : %s\n' "$AFTER" >&2;  exit 2; }

case "$EXPORT_MODE" in
  strict|additif) ;;
  *) printf 'assert-parity: --export-mode doit valoir strict ou additif (reçu : %s)\n' \
       "$EXPORT_MODE" >&2; exit 2 ;;
esac
if [[ "$EXPORT_MODE" == "strict" && -n "$EXPORT_DOC" ]]; then
  printf 'assert-parity: --export-doc est refusé en mode strict — une comparaison octet à octet \
n a pas de dialecte à documenter ; l accepter ferait croire à un contrôle qui n a pas eu lieu\n' >&2
  exit 2
fi

# Rapport : tout est accumulé dans un tampon, écrit à la fin sur stdout et dans
# `--out`. Rien n'est affiché « seulement ».
REPORT=""
GAPS=0
CHECKS=0

say() { REPORT+="$1"$'\n'; }
gap() { GAPS=$((GAPS + 1)); say "  ÉCART   $*"; }
ok()  { CHECKS=$((CHECKS + 1)); say "  ok      $*"; }
info() { say "  noté    $*"; }

# Un relevé absent n'est PAS une divergence, c'est une non-exécution : compté à
# part (`MISSING`) et rendu `2`, pour ne jamais conclure un vert sur une mesure
# qui n'a pas eu lieu.
MISSING=0

# Tables dont le contenu est une fonction du CORPUS : une montée ne doit pas les
# bouger. Liste volontairement explicite — une table oubliée ici n'est pas
# silencieuse pour autant, elle tombe dans le rapport « autres tables ».
readonly -a BLOCKING_TABLES=(
  notes
  note_index
  note_links
  temporal_index
  note_history
  note_embeddings
  notes_fts
  feature_counter
  archive_index
  redirect_table
  code_vault
  file_checksums
  note_overrides
  agent_vault_grants
  tenant_vault_grants
  tenants
  gradatum_jobs
)
# `notes_fts_data`, `notes_fts_idx`, `notes_fts_config`, `notes_fts_docsize`,
# `sqlite_sequence` : structures internes de SQLite/FTS5 et compteurs d'auto-
# incrément. Leur nombre de lignes n'est pas un invariant de corpus — reportés.

json_scalar() {  # $1 = fichier json, $2 = chemin jq
  jq -r "$2" "$1" 2>/dev/null || printf '__ILLISIBLE__'
}

say "parité ${LABEL:+(}$LABEL${LABEL:+)}"
say "  AVANT : $BEFORE"
say "  APRÈS : $AFTER"
if [[ "$EXPORT_MODE" == "additif" ]]; then
  say "  clé (e) : mode additif — projection sur le dialecte AVANT ; guide : ${EXPORT_DOC:-AUCUN}"
else
  say "  clé (e) : mode strict — octet à octet (retour arrière)"
fi
say ""

# ── (a) _sqlx_migrations ─────────────────────────────────────────────────────
say "(a) _sqlx_migrations (queue.sqlite) — version, description, installed_on, checksum, success"

b_sqlx="$BEFORE/sqlx_migrations.tsv"
a_sqlx="$AFTER/sqlx_migrations.tsv"
if [[ -f "$b_sqlx" && -f "$a_sqlx" ]]; then
  if cmp -s -- "$b_sqlx" "$a_sqlx"; then
    n=$(( $(wc -l <"$b_sqlx") - 1 ))
    ok "$n ligne(s) identiques, octet à octet (aucune migration appliquée par la montée)"
  else
    gap "_sqlx_migrations diffère — lignes divergentes :"
    while IFS= read -r line; do
      printf -v g '    AVANT : %s' "$line"; say "$g"
    done < <(comm -23 <(sort "$b_sqlx") <(sort "$a_sqlx"))
    while IFS= read -r line; do
      printf -v g '    APRÈS : %s' "$line"; say "$g"
    done < <(comm -13 <(sort "$b_sqlx") <(sort "$a_sqlx"))
  fi
  bad_b="$(awk -F'\t' 'NR>1 && $5 != 1 {print $1}' "$b_sqlx" | tr '\n' ' ')"
  bad_a="$(awk -F'\t' 'NR>1 && $5 != 1 {print $1}' "$a_sqlx" | tr '\n' ' ')"
  if [[ -z "$bad_b" ]]; then ok "AVANT : aucune ligne success != 1"; else gap "AVANT : success != 1 pour $bad_b"; fi
  if [[ -z "$bad_a" ]]; then ok "APRÈS : aucune ligne success != 1"; else gap "APRÈS : success != 1 pour $bad_a"; fi
else
  MISSING=$((MISSING + 1))
  printf -v g 'relevé _sqlx_migrations absent (AVANT : %s, APRÈS : %s)' \
    "$([[ -f $b_sqlx ]] && echo présent || echo absent)" \
    "$([[ -f $a_sqlx ]] && echo présent || echo absent)"
  info "$g"
fi

# ── (b) _schema_migrations ───────────────────────────────────────────────────
say ""
say "(b) _schema_migrations (index.db) — clé plus stricte que la lettre du brief"
b_sm="$BEFORE/schema_migrations.tsv"
a_sm="$AFTER/schema_migrations.tsv"
if [[ -f "$b_sm" && -f "$a_sm" ]]; then
  if cmp -s -- "$b_sm" "$a_sm"; then
    n=$(( $(wc -l <"$b_sm") - 1 ))
    ok "$n migration(s) d'index identiques — la montée n'en applique aucune"
  else
    gap "_schema_migrations diffère — versions divergentes :"
    while IFS= read -r v; do printf -v g '    AVANT seulement : %s' "$v"; say "$g"; done \
      < <(comm -23 <(sort "$b_sm") <(sort "$a_sm"))
    while IFS= read -r v; do printf -v g '    APRÈS seulement : %s' "$v"; say "$g"; done \
      < <(comm -13 <(sort "$b_sm") <(sort "$a_sm"))
  fi
else
  MISSING=$((MISSING + 1))
  info "relevé _schema_migrations absent"
fi

# ── (c) vault_list ───────────────────────────────────────────────────────────
say ""
say "(c) vault_list — identifiants rendus, ordre rendu puis trié"
cmp_lines() {  # $1 = clé, $2 = fichier avant, $3 = fichier après
  local key="$1" fb="$2" fa="$3"
  if [[ ! -f "$fb" || ! -f "$fa" ]]; then
    MISSING=$((MISSING + 1)); info "$key : relevé absent"; return 0
  fi
  local nb na
  nb="$(wc -l <"$fb")"; na="$(wc -l <"$fa")"
  if cmp -s -- "$fb" "$fa"; then
    ok "$key : $nb identifiant(s), identiques et dans le même ordre"
    return 0
  fi
  gap "$key : divergence d'ordre ou de contenu ($nb avant, $na après) — identifiants divergents :"
  local v
  while IFS= read -r v; do [[ -n "$v" ]] && { printf -v g '    AVANT seulement : %s' "$v"; say "$g"; }; done \
    < <(comm -23 <(sort "$fb") <(sort "$fa"))
  while IFS= read -r v; do [[ -n "$v" ]] && { printf -v g '    APRÈS seulement : %s' "$v"; say "$g"; }; done \
    < <(comm -13 <(sort "$fb") <(sort "$fa"))
  # Ordre : mêmes identifiants, ordre différent → nommé séparément, sans
  # recompter le même écart deux fois.
  if cmp -s <(sort "$fb") <(sort "$fa"); then
    g="    (mêmes identifiants des deux côtés — l'écart est un ORDRE différent)"
    say "$g"
  fi
}

cmp_lines "vault_list (ordre rendu)" "$BEFORE/vault_list.paths.txt" "$AFTER/vault_list.paths.txt"
cmp_lines "vault_list (trié)"       "$BEFORE/vault_list.sorted.txt" "$AFTER/vault_list.sorted.txt"

# ── (d) les 5 recherches ─────────────────────────────────────────────────────
say ""
say "(d) vault_search — 5 requêtes littérales versionnées dans le harnais"
for f in "$BEFORE"/search-*.paths.txt; do
  [[ -e "$f" ]] || break
  name="$(basename "$f" .paths.txt)"
  cmp_lines "vault_search/$name" "$f" "$AFTER/$name.paths.txt"
done
b_q="$(cat "$BEFORE/queries.txt" 2>/dev/null || true)"
say "  requêtes : $(printf '%s' "$b_q" | tr '\n' ' ')"
hit0=0
for f in "$BEFORE"/search-*.paths.txt; do
  [[ -e "$f" ]] || break
  (( $(wc -l <"$f") == 0 )) && hit0=$((hit0 + 1))
done
if (( hit0 > 0 )); then
  # REPORTÉ, jamais compté comme un écart : deux relevés également vides ne
  # divergent pas. Le cas est symétrique de la clé (e) (export vide des deux
  # côtés), qui est déjà en `info`. Un vrai changement reste bloquant — c'est
  # `cmp_lines` ci-dessus qui le voit, pas ce décompte de vacuité.
  info "$hit0 des 5 recherches ne rendent AUCUN résultat sur le relevé AVANT — \
la parité d'ordre y est vacue et ne prouve rien (même traitement que la clé (e) : \
parité vraie mais vacue n'est pas une divergence)"
fi

# ── (e) export-features ──────────────────────────────────────────────────────
say ""
b_ex="$BEFORE/export-features.json"
a_ex="$AFTER/export-features.json"

# Le programme de projection. Il ne juge RIEN : il rend les faits — dialecte du
# relevé AVANT, clés ajoutées par le relevé APRÈS, clés disparues, et les entrées
# dont la projection diverge. Le verdict, lui, se rend plus bas, en clair.
# `unique` trie (ordre stable du rapport) ; l'ORDRE DES CARTES, lui, est celui du
# tableau, et il est comparé tel quel — deux entrées permutées sont un écart.
PROJECTION_JQ='
  ($B[0]) as $b | ($A[0]) as $a |
  if (($b | type) != "array") or (($a | type) != "array") then
    {illisible: true}
  else
    ([$b[] | keys_unsorted[]] | unique) as $kb |
    ([$a[] | keys_unsorted[]] | unique) as $ka |
    {
      nb: ($b | length),
      na: ($a | length),
      dialecte: $kb,
      ajoutees: ($ka - $kb),
      disparues: ($kb - $ka),
      ecarts: [
        range(0; ([($b | length), ($a | length)] | min)) as $i |
        ($b[$i]) as $be | ($a[$i]) as $ae |
        ($ae | with_entries(select(.key as $k | $kb | index($k)))) as $proj |
        select($be != $proj) |
        {
          index: $i,
          feature: ($be.feature // "?"),
          manquantes: [$kb[] as $k | select(($ae | has($k)) | not) | $k],
          valeurs: [
            $kb[] as $k | select($ae | has($k)) |
            select($ae[$k] != $be[$k]) |
            {cle: $k, avant: $be[$k], apres: $ae[$k]}
          ]
        }
      ]
    }
  end
'
readonly PROJECTION_JQ

if [[ ! -f "$b_ex" || ! -f "$a_ex" ]]; then
  MISSING=$((MISSING + 1))
  printf -v g "export-features absent d'un côté (AVANT : %s, APRÈS : %s)" \
    "$([[ -f $b_ex ]] && echo présent || echo absent)" \
    "$([[ -f $a_ex ]] && echo présent || echo absent)"
  info "$g"
elif [[ "$EXPORT_MODE" == "strict" ]]; then
  say "(e) export-features — octet à octet (retour arrière)"
  sb="$(sha256sum <"$b_ex" | cut -d' ' -f1)"
  sa="$(sha256sum <"$a_ex" | cut -d' ' -f1)"
  if cmp -s -- "$b_ex" "$a_ex"; then
    ok "octet à octet identique (sha256 ${sb:0:16}…, $(wc -c <"$b_ex") octets)"
    if [[ "$(jq 'length' <"$b_ex" 2>/dev/null || echo '?')" == "0" ]]; then
      info "⚠ l'export est VIDE des deux côtés — la parité (e) est satisfaite mais VACUE : \
les cartes de la fixture ne sont pas dans l'état mesuré. Ici c'est REPORTÉ (cette clé ne \
juge que l'écart AVANT/APRÈS) ; c'est le harnais qui rend ce cas BLOQUANT, par son \
assertion de non-vacuité — voir le point 4 de tests/upgrade/README.md."
    fi
  else
    gap "export-features diffère : sha256 AVANT=${sb:0:16}… APRÈS=${sa:0:16}… (${sb} / ${sa})"
  fi
else
  say "(e) export-features — mode additif : projection sur le dialecte AVANT"
  if [[ ! -f "$EXPORT_DOC" ]]; then
    # Pas de mesure plutôt qu'une mesure muette : le mode additif existe pour
    # attraper un ajout de clé NON DOCUMENTÉ. Sans guide, cette moitié de la
    # règle n'est pas mesurable — c'est une non-exécution, jamais un vert.
    MISSING=$((MISSING + 1))
    info "mode additif SANS guide de montée lisible (--export-doc : ${EXPORT_DOC:-absent}) — \
la clé (e) n'est PAS mesurée : l'ajout de clé ne serait relevé que pour moitié"
  else
    e_rc=0
    e_sum="$(jq -n --slurpfile B "$b_ex" --slurpfile A "$a_ex" "$PROJECTION_JQ" 2>/dev/null)" \
      || e_rc=$?
    if (( e_rc != 0 )) || [[ -z "$e_sum" ]] || [[ "$(jq -r '.illisible // false' <<<"$e_sum")" == "true" ]]; then
      MISSING=$((MISSING + 1))
      # L'absence de l'outil et l'illisibilité du relevé ne se disent pas de la
      # même façon : la première envoie corriger l'environnement, la seconde le
      # relevé. Les confondre ferait chercher au mauvais endroit.
      if ! command -v jq >/dev/null 2>&1; then
        info "mode additif : outil requis absent (jq) — la projection sur le dialecte AVANT \
ne peut pas être calculée, la clé (e) n'est PAS mesurée"
      else
        info "export-features illisible en JSON d'un côté au moins — la projection ne peut pas \
être calculée (AVANT : $b_ex, APRÈS : $a_ex)"
      fi
    else
      e_nb="$(jq -r '.nb' <<<"$e_sum")"
      e_na="$(jq -r '.na' <<<"$e_sum")"
      e_nkb="$(jq -r '.dialecte | length' <<<"$e_sum")"
      e_dial="$(jq -r '.dialecte | join(", ")' <<<"$e_sum")"
      e_add="$(jq -r '.ajoutees | join(", ")' <<<"$e_sum")"
      e_disp="$(jq -r '.disparues | join(", ")' <<<"$e_sum")"

      say "  dialecte AVANT : $e_nkb clé(s) — ${e_dial:-aucune}"
      if (( e_nb == 0 && e_na == 0 )); then
        info "⚠ l'export est VIDE des deux côtés — la projection est satisfaite mais VACUE : \
les cartes de la fixture ne sont pas dans l'état mesuré. REPORTÉ ici ; c'est le harnais qui \
rend ce cas BLOQUANT, par son assertion de non-vacuité — voir le point 4 de tests/upgrade/README.md."
      fi
      if (( e_nb != e_na )); then
        gap "nombre de cartes : AVANT=$e_nb APRÈS=$e_na — le relevé APRÈS doit porter les \
mêmes cartes, dans le même ordre"
      fi
      if [[ -n "$e_disp" ]]; then
        gap "clé(s) du dialecte AVANT absente(s) du relevé APRÈS : $e_disp"
      fi
      # Le relevé des clés ajoutées est TOUJOURS écrit — sans quoi la règle serait
      # satisfaite sans rien dire de ce qu'elle a vu.
      if [[ -n "$e_add" ]]; then
        ok "clés ajoutées côté MONTÉ (dialecte qui s'élargit, relevé) : cles_ajoutees=$e_add"
      else
        info "aucune clé ajoutée par le relevé APRÈS : cles_ajoutees=aucune"
      fi

      e_necart="$(jq -r '.ecarts | length' <<<"$e_sum")"
      if (( e_necart > 0 )); then
        gap "$e_necart entrée(s) divergente(s) après projection sur le dialecte AVANT :"
        while IFS=$'\t' read -r idx feat manq vals; do
          printf -v g '    entrée %s (feature=%s)' "$idx" "${feat:-?}"; say "$g"
          if [[ -n "$manq" ]]; then
            printf -v g '      clé(s) du dialecte AVANT absente(s) : %s' "$manq"; say "$g"
          fi
          if [[ -n "$vals" ]]; then
            printf -v g '      valeur(s) divergente(s) : %s' "$vals"; say "$g"
          fi
        done < <(jq -r '.ecarts[] | [
                   (.index | tostring), .feature,
                   (.manquantes | join(", ")),
                   ([.valeurs[] | "\(.cle) : \(.avant | tojson) → \(.apres | tojson)"] | join(" ; "))
                 ] | @tsv' <<<"$e_sum")
      else
        ok "$e_nb entrée(s) identique(s) à la projection du dialecte AVANT (ordre compris)"
      fi

      # Chaque clé ajoutée doit être CITÉE dans le guide de montée : une clé
      # nouvelle non documentée est une évolution de contrat que personne ne lit.
      while IFS= read -r k; do
        [[ -n "$k" ]] || continue
        if grep -qE -- "(^|[^A-Za-z0-9_])${k}([^A-Za-z0-9_]|\$)" "$EXPORT_DOC" 2>/dev/null; then
          ok "clé ajoutée « $k » citée dans le guide ($EXPORT_DOC)"
        else
          gap "clé ajoutée « $k » ABSENTE du guide $EXPORT_DOC — une clé nouvelle non \
documentée est une évolution de contrat non annoncée"
        fi
      done < <(jq -r '.ajoutees[]' <<<"$e_sum")
    fi
  fi
fi

# ── (f) comptes par table ────────────────────────────────────────────────────
say ""
say "(f) comptes par table — notes et gradatum_jobs à tolérance zéro"
b_c="$BEFORE/counts.json"
a_c="$AFTER/counts.json"
if [[ -f "$b_c" && -f "$a_c" ]]; then
  # Les clés portent un point (`index.db`) : chemin jq en crochets, jamais
  # `.["index.db"]` écrit en `.index.db` (que jq lirait comme trois niveaux).
  for key in 'index.db|.["index.db"].notes' 'queue.sqlite|.["queue.sqlite"].gradatum_jobs'; do
    dbname="${key%%|*}"; path="${key#*|}"
    vb="$(json_scalar "$b_c" "$path")"
    va="$(json_scalar "$a_c" "$path")"
    if [[ "$vb" == "$va" ]]; then ok "$dbname.${path##*.} = $vb (tolérance zéro)"; else
      gap "$dbname.${path##*.} : AVANT=$vb APRÈS=$va"; fi
  done
  say "  tables dérivées du corpus (bloquantes) :"
  for t in "${BLOCKING_TABLES[@]}"; do
    [[ "$t" == "notes" || "$t" == "gradatum_jobs" ]] && continue
    vb="$(jq -r --arg t "$t" '.["index.db"][$t] // empty' "$b_c" 2>/dev/null || true)"
    va="$(jq -r --arg t "$t" '.["index.db"][$t] // empty' "$a_c" 2>/dev/null || true)"
    [[ -n "$vb" ]] || continue
    if [[ "$vb" == "$va" ]]; then ok "$t = $vb"; else gap "$t : AVANT=$vb APRÈS=$va"; fi
  done
  # Le reste : reporté, jamais tu.
  say "  autres tables (reportées, non bloquantes — trafic/télémétrie/structures internes) :"
  while IFS=$'\t' read -r db t vb; do
    va="$(jq -r --arg db "$db" --arg t "$t" '.[$db][$t] // empty' "$a_c" 2>/dev/null || true)"
    if [[ -n "$va" && "$va" != "$vb" ]]; then
      info "$db.$t : AVANT=$vb APRÈS=$va (delta $((va - vb)))"
    fi
  done < <(jq -r 'to_entries[] | .key as $db | .value | to_entries[] | [$db, .key, (.value|tostring)] | @tsv' "$b_c")
else
  MISSING=$((MISSING + 1))
  info "comptes absents d'un côté"
fi

# ── (g) digest de la file ────────────────────────────────────────────────────
say ""
say "(g) file de travaux — digest ordonné (id, statut, kind)"
cmp_lines "jobs (id/statut/kind)" "$BEFORE/jobs.digest.txt" "$AFTER/jobs.digest.txt"
b_st="$BEFORE/jobs_by_status.txt"
a_st="$AFTER/jobs_by_status.txt"
if [[ -f "$b_st" && -f "$a_st" ]]; then
  if cmp -s -- "$b_st" "$a_st"; then
    ok "répartition par statut identique : $(tr '\n' ' ' <"$b_st")"
  else
    gap "répartition par statut : AVANT=[$(tr '\n' ' ' <"$b_st")] APRÈS=[$(tr '\n' ' ' <"$a_st")]"
  fi
fi

# ── Verdict ──────────────────────────────────────────────────────────────────
say ""
if (( MISSING > 0 )); then
  printf -v g 'NON EXÉCUTÉ — %d relevé(s) attendu(s) absent(s) ou illisible(s) : aucune parité ne peut être conclue' "$MISSING"
  say "$g"
  VERDICT=2
elif (( GAPS > 0 )); then
  printf -v g 'DIVERGENCE — %d écart(s) nommé(s) ci-dessus, %d clé(s) vérifiée(s)' "$GAPS" "$CHECKS"
  say "$g"
  VERDICT=1
else
  say "PARITÉ ÉTABLIE — $CHECKS clé(s) vérifiée(s), 0 écart"
  VERDICT=0
fi

printf '%s' "$REPORT"
if [[ -n "$OUT" ]]; then
  mkdir -p -- "$(dirname -- "$OUT")" 2>/dev/null || true
  printf '%s' "$REPORT" >"$OUT"
fi
exit "$VERDICT"
