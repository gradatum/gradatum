#!/usr/bin/env bash
# import-bulk-legacy-vault.sh — Importe les notes du legacy vault dans gradatum via API HTTP.
#
# Usage :
#   bash scripts/import-bulk-legacy-vault.sh [OPTIONS]
#
# Options :
#   --vault-root <path>    Racine du legacy vault (défaut : /home/maintainer-user/.memory-vault/)
#   --server <url>         URL du serveur gradatum (défaut : http://localhost:19090)
#   --api-key-file <path>  Fichier API key chmod 600 (défaut : /etc/gradatum/claude-code.api-key)
#   --dry-run              Mode simulation — aucun envoi HTTP, log only
#   --limit N              Traiter au maximum N notes (0 = illimité)
#   --rate-ms N            Délai entre requêtes en ms (défaut : 200)
#   -h, --help             Affiche cette aide
#
# Sortie :
#   - Stats finales sur stdout (Total | Accepted | Rejected | Errors | Divergences)
#   - Log détaillé dans ~/tmp/import-bulk-<timestamp>.log
#
# Codes de retour :
#   0  — succès (même si des notes individuelles ont échoué)
#   1  — erreur fatale (auth, server inaccessible, vault-root manquant)
#
# Sections canoniques gradatum (10) :
#   architecture, debug, decisions, retrospectives, reasoning,
#   experiments, lessons-learned, feedback, agent-issues, reference
#
# Sections legacy vault supportées (les 10 canoniques gradatum + fallback) :
#   architecture, debug, decisions, retrospectives, reasoning,
#   experiments, lessons-learned, feedback, agent-issues, reference
#   → sections legacy vault hors liste (projects, agents, constitution, patterns, default)
#     sont importées avec section_hint="reference" (fallback neutre).

set -euo pipefail

# ── Constantes ────────────────────────────────────────────────────────────────

readonly CANONICAL_SECTIONS=(
    architecture
    debug
    decisions
    retrospectives
    reasoning
    experiments
    lessons-learned
    feedback
    agent-issues
    reference
)

# Sections legacy vault reconnues mais hors scope gradatum → fallback reference.
readonly VAULT_SECTIONS_FALLBACK=(
    projects
    agents
    constitution
    patterns
    default
    personal-open
    templates
)

# Délai initial entre requêtes (ms) — configurable via --rate-ms.
DEFAULT_RATE_MS=200
# Délai de backoff sur 429 (secondes).
BACKOFF_429_SECS=30
# Nombre maximum de retry sur 429.
MAX_RETRY_429=3

# ── Variables globales ────────────────────────────────────────────────────────

VAULT_ROOT="/home/maintainer-user/.memory-vault/"
SERVER_URL="http://localhost:19090"
API_KEY_FILE="/etc/gradatum/claude-code.api-key"
DRY_RUN=false
LIMIT=0
RATE_MS=${DEFAULT_RATE_MS}
LOG_FILE="${HOME}/tmp/import-bulk-$(date +%Y%m%d-%H%M%S).log"

# Compteurs.
count_total=0
count_accepted=0
count_rejected=0
count_error=0
count_divergence=0

# JWT courant (obtenu via /auth/exchange au démarrage).
JWT=""

# ── Utilitaires ───────────────────────────────────────────────────────────────

log_info()  { echo "[INFO]  $*" | tee -a "${LOG_FILE}"; }
log_note()  { echo "[NOTE]  $*" >> "${LOG_FILE}"; }
log_warn()  { echo "[WARN]  $*" | tee -a "${LOG_FILE}" >&2; }
log_error() { echo "[ERROR] $*" | tee -a "${LOG_FILE}" >&2; }
log_dry()   { echo "[DRY]   $*" | tee -a "${LOG_FILE}"; }

die() {
    log_error "$*"
    exit 1
}

# ── Aide ──────────────────────────────────────────────────────────────────────

usage() {
    sed -n '/^# Usage/,/^$/p' "$0" | head -20
    exit 0
}

# ── Parsing des arguments ─────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --vault-root)
            VAULT_ROOT="$2"; shift 2 ;;
        --server)
            SERVER_URL="$2"; shift 2 ;;
        --api-key-file)
            API_KEY_FILE="$2"; shift 2 ;;
        --dry-run)
            DRY_RUN=true; shift ;;
        --limit)
            LIMIT="$2"; shift 2 ;;
        --rate-ms)
            RATE_MS="$2"; shift 2 ;;
        -h|--help)
            usage ;;
        *)
            die "argument inconnu : $1 (utiliser --help pour l'aide)" ;;
    esac
done

# ── Validation pré-démarrage ──────────────────────────────────────────────────

# Créer ~/tmp/ si absent.
mkdir -p "${HOME}/tmp"

log_info "=== import-bulk-legacy-vault.sh démarrage ==="
log_info "vault-root : ${VAULT_ROOT}"
log_info "server     : ${SERVER_URL}"
log_info "dry-run    : ${DRY_RUN}"
log_info "limit      : ${LIMIT} (0=illimité)"
log_info "rate-ms    : ${RATE_MS}"
log_info "log        : ${LOG_FILE}"

# Vérifier vault-root.
[[ -d "${VAULT_ROOT}" ]] || die "vault-root '${VAULT_ROOT}' n'existe pas"

# Vérifier jq disponible (parsing YAML/JSON).
command -v jq >/dev/null 2>&1 || die "jq requis (apt install jq)"
command -v curl >/dev/null 2>&1 || die "curl requis"

# ── Auth : obtenir JWT via /auth/exchange ────────────────────────────────────

exchange_jwt() {
    local api_key="$1"
    local response
    local http_code
    local body

    response=$(curl -s -w "\n%{http_code}" \
        -X POST \
        -H "Authorization: Bearer ${api_key}" \
        -H "Content-Type: application/json" \
        "${SERVER_URL}/auth/exchange" \
        2>/dev/null) || {
        log_error "curl POST /auth/exchange : erreur réseau"
        return 1
    }

    http_code=$(echo "${response}" | tail -1)
    body=$(echo "${response}" | head -n -1)

    if [[ "${http_code}" != "200" ]]; then
        log_error "POST /auth/exchange HTTP ${http_code} : ${body}"
        return 1
    fi

    JWT=$(echo "${body}" | jq -r '.token' 2>/dev/null) || {
        log_error "Impossible d'extraire le token depuis la réponse : ${body}"
        return 1
    }

    if [[ -z "${JWT}" || "${JWT}" == "null" ]]; then
        log_error "Token JWT vide dans la réponse /auth/exchange"
        return 1
    fi

    local ttl_secs
    ttl_secs=$(echo "${body}" | jq -r '.ttl_secs' 2>/dev/null) || ttl_secs="?"
    log_info "JWT obtenu (ttl=${ttl_secs}s)"
}

if [[ "${DRY_RUN}" == "false" ]]; then
    # Vérifier que le fichier API key existe et est lisible.
    [[ -f "${API_KEY_FILE}" ]] || die "API key file '${API_KEY_FILE}' introuvable. Créer avec : gradatum-admin api-keys create --owner claude-code --tenant main"
    [[ -r "${API_KEY_FILE}" ]] || die "API key file '${API_KEY_FILE}' non lisible (vérifier permissions)"

    API_KEY=$(cat "${API_KEY_FILE}" | tr -d '[:space:]')
    [[ -n "${API_KEY}" ]] || die "API key file '${API_KEY_FILE}' est vide"
    [[ "${API_KEY}" == ak_* ]] || die "API key '${API_KEY_FILE}' : format invalide (attendu: ak_...)"

    exchange_jwt "${API_KEY}" || die "Authentification échouée — vérifier que gradatum-server est LIVE sur ${SERVER_URL}"
    log_info "Authentification OK"
else
    log_dry "Mode dry-run — pas d'authentification"
fi

# ── Parsing frontmatter YAML ──────────────────────────────────────────────────

# Extrait un champ YAML du frontmatter (entre --- et ---).
# Usage : extract_yaml_field <file> <field>
# Retourne la valeur scalaire ou "" si absent.
extract_yaml_field() {
    local file="$1"
    local field="$2"
    # Extrait le bloc frontmatter (entre les deux ---).
    local frontmatter
    frontmatter=$(awk '/^---/{p++} p==1{print} p==2{exit}' "${file}" | tail -n +2)
    # Cherche le champ (format: "field: value").
    echo "${frontmatter}" | grep -E "^${field}:" | head -1 | sed "s/^${field}:[[:space:]]*//" | tr -d "'\""
}

# Extrait les tags YAML du frontmatter (format liste YAML).
# Retourne un tableau JSON ["tag1","tag2",...].
extract_yaml_tags() {
    local file="$1"
    local frontmatter
    frontmatter=$(awk '/^---/{p++} p==1{print} p==2{exit}' "${file}" | tail -n +2)

    # Cas 1 : tags sur une seule ligne → "tags: [tag1, tag2]"
    local inline_tags
    inline_tags=$(echo "${frontmatter}" | grep -E "^tags:" | sed 's/^tags:[[:space:]]*//')
    if [[ "${inline_tags}" == \[* ]]; then
        # Déjà au format JSON-like, convertir en JSON strict.
        echo "${inline_tags}" | sed 's/\[//g;s/\]//g' | tr ',' '\n' | \
            tr -d ' ' | grep -v '^$' | \
            jq -R -s 'split("\n") | map(select(. != ""))' 2>/dev/null || echo "[]"
        return
    fi

    # Cas 2 : tags en bloc liste YAML (lignes "- tagname" après "tags:").
    local in_tags=false
    local tags=()
    while IFS= read -r line; do
        if [[ "${line}" =~ ^tags: ]]; then
            in_tags=true
            continue
        fi
        if [[ "${in_tags}" == true ]]; then
            if [[ "${line}" =~ ^[[:space:]]*-[[:space:]]+(.*) ]]; then
                tags+=("${BASH_REMATCH[1]}")
            elif [[ "${line}" =~ ^[a-zA-Z_] ]]; then
                # Fin du bloc tags (nouveau champ YAML).
                break
            fi
        fi
    done <<< "${frontmatter}"

    if [[ ${#tags[@]} -gt 0 ]]; then
        printf '%s\n' "${tags[@]}" | jq -R -s 'split("\n") | map(select(. != ""))' 2>/dev/null || echo "[]"
    else
        echo "[]"
    fi
}

# Extrait le body markdown sans le frontmatter.
extract_body() {
    local file="$1"
    # Supprime le frontmatter (entre le premier --- et le second ---).
    awk 'BEGIN{p=0} /^---/{p++; if(p<=2) next} p>=2{print}' "${file}"
}

# ── Détermine la section gradatum depuis la section legacy vault ────────────────

# Retourne la section gradatum correspondante.
# Si la section legacy vault est dans les 10 canoniques → la retourner.
# Sinon → "reference" (fallback).
map_section() {
    local vault_section="$1"
    for s in "${CANONICAL_SECTIONS[@]}"; do
        if [[ "${vault_section}" == "${s}" ]]; then
            echo "${vault_section}"
            return
        fi
    done
    # Section hors scope → fallback.
    echo "reference"
}

# ── Envoi HTTP POST /api/v1/vault_write ──────────────────────────────────────

# Envoie une note à gradatum. Retourne le code HTTP ou "DRY".
send_note() {
    local title="$1"
    local body="$2"
    local section_hint="$3"
    local tags_json="$4"
    local author="${5:-Claude Code}"

    local payload
    payload=$(jq -n \
        --arg title "${title}" \
        --arg body "${body}" \
        --arg section_hint "${section_hint}" \
        --arg author "${author}" \
        --arg tenant_id "main" \
        --argjson tags "${tags_json}" \
        '{
            "title": $title,
            "body": $body,
            "section_hint": $section_hint,
            "tags": $tags,
            "author": $author,
            "tenant_id": $tenant_id
        }' 2>/dev/null) || {
        log_error "jq payload construction échoué pour titre '${title}'"
        return 1
    }

    if [[ "${DRY_RUN}" == "true" ]]; then
        echo "DRY"
        return 0
    fi

    local response http_code resp_body
    local retry=0

    while [[ ${retry} -le ${MAX_RETRY_429} ]]; do
        response=$(curl -s -w "\n%{http_code}" \
            -X POST \
            -H "Authorization: Bearer ${JWT}" \
            -H "Content-Type: application/json" \
            -d "${payload}" \
            "${SERVER_URL}/api/v1/vault_write" \
            2>/dev/null) || {
            log_warn "curl erreur réseau pour '${title}'"
            echo "ERR_NETWORK"
            return 0
        }

        http_code=$(echo "${response}" | tail -1)
        resp_body=$(echo "${response}" | head -n -1)

        if [[ "${http_code}" == "429" ]]; then
            retry=$((retry + 1))
            if [[ ${retry} -le ${MAX_RETRY_429} ]]; then
                log_warn "429 Too Many Requests — backoff ${BACKOFF_429_SECS}s (retry ${retry}/${MAX_RETRY_429})"
                sleep "${BACKOFF_429_SECS}"
                continue
            fi
        fi

        break
    done

    echo "${http_code}"
    # Retourner le body pour extraction section_returned.
    LAST_RESPONSE_BODY="${resp_body}"
}

LAST_RESPONSE_BODY=""

# ── Trap EXIT : log stats finales ────────────────────────────────────────────

print_stats() {
    echo ""
    log_info "=== RÉSULTATS IMPORT ==="
    log_info "Total      : ${count_total}"
    log_info "Accepted   : ${count_accepted}   (HTTP 202)"
    log_info "Rejected   : ${count_rejected}   (HTTP 4xx)"
    log_info "Errors     : ${count_error}    (réseau/5xx)"
    log_info "Divergences: ${count_divergence}   (section_hint ≠ section_returned)"
    echo ""
    echo "Total: ${count_total} | Accepted: ${count_accepted} | Rejected: ${count_rejected} | Errors: ${count_error} | Section divergences: ${count_divergence}"
}

trap 'print_stats' EXIT

# ── Boucle principale ─────────────────────────────────────────────────────────

log_info "Démarrage de l'import..."

for section in "${CANONICAL_SECTIONS[@]}"; do
    section_dir="${VAULT_ROOT}/${section}"
    [[ -d "${section_dir}" ]] || continue

    # Lister les fichiers .md dans la section.
    while IFS= read -r -d '' md_file; do
        # Vérifier la limite.
        if [[ ${LIMIT} -gt 0 && ${count_total} -ge ${LIMIT} ]]; then
            log_info "Limite ${LIMIT} atteinte — arrêt."
            exit 0
        fi

        count_total=$((count_total + 1))

        # Extraire les champs depuis le fichier.
        local_title=$(extract_yaml_field "${md_file}" "title")
        if [[ -z "${local_title}" ]]; then
            # Fallback : utiliser le nom de fichier sans extension.
            local_title=$(basename "${md_file}" .md)
        fi

        local_tags_json=$(extract_yaml_tags "${md_file}")
        [[ -z "${local_tags_json}" ]] && local_tags_json="[]"

        local_body=$(extract_body "${md_file}")
        local_section_hint=$(map_section "${section}")
        local_author=$(extract_yaml_field "${md_file}" "author")
        [[ -z "${local_author}" ]] && local_author="Claude Code"

        body_size=${#local_body}

        if [[ "${DRY_RUN}" == "true" ]]; then
            log_dry "WOULD IMPORT: section=${section} → hint=${local_section_hint} | title='${local_title}' | body=${body_size}B | tags=${local_tags_json}"
            count_accepted=$((count_accepted + 1))
            continue
        fi

        # Envoyer la note.
        http_code=$(send_note \
            "${local_title}" \
            "${local_body}" \
            "${local_section_hint}" \
            "${local_tags_json}" \
            "${local_author}") || http_code="ERR_SEND"

        case "${http_code}" in
            202)
                count_accepted=$((count_accepted + 1))
                # Extraire section_returned pour détecter divergence taxonomique.
                section_returned=$(echo "${LAST_RESPONSE_BODY}" | jq -r '.section // empty' 2>/dev/null || true)
                if [[ -n "${section_returned}" && "${section_returned}" != "${local_section_hint}" ]]; then
                    count_divergence=$((count_divergence + 1))
                    log_note "DIVERGENCE: ${md_file} | hint=${local_section_hint} → returned=${section_returned}"
                else
                    log_note "OK: ${md_file} | section=${local_section_hint} | title=${local_title}"
                fi
                ;;
            200)
                # Certains backends retournent 200 au lieu de 202.
                count_accepted=$((count_accepted + 1))
                log_note "OK(200): ${md_file} | section=${local_section_hint}"
                ;;
            4[0-9][0-9]|ERR_*)
                count_rejected=$((count_rejected + 1))
                log_warn "REJECTED(${http_code}): ${md_file} | section=${local_section_hint} | title=${local_title}"
                ;;
            5[0-9][0-9])
                count_error=$((count_error + 1))
                log_warn "ERROR(${http_code}): ${md_file} | section=${local_section_hint} — skip"
                ;;
            *)
                count_error=$((count_error + 1))
                log_warn "INCONNU(${http_code}): ${md_file} — skip"
                ;;
        esac

        # Rate limiting.
        if [[ ${RATE_MS} -gt 0 ]]; then
            sleep "$(awk "BEGIN{printf \"%.3f\", ${RATE_MS}/1000}")"
        fi

    done < <(find "${section_dir}" -maxdepth 1 -name "*.md" -print0 2>/dev/null | sort -z)
done

# ── Sections legacy-vault non-canoniques (fallback → reference) ─────────────────

for section in "${VAULT_SECTIONS_FALLBACK[@]}"; do
    section_dir="${VAULT_ROOT}/${section}"
    [[ -d "${section_dir}" ]] || continue

    while IFS= read -r -d '' md_file; do
        if [[ ${LIMIT} -gt 0 && ${count_total} -ge ${LIMIT} ]]; then
            log_info "Limite ${LIMIT} atteinte — arrêt."
            exit 0
        fi

        count_total=$((count_total + 1))
        local_title=$(extract_yaml_field "${md_file}" "title")
        [[ -z "${local_title}" ]] && local_title=$(basename "${md_file}" .md)
        local_tags_json=$(extract_yaml_tags "${md_file}")
        [[ -z "${local_tags_json}" ]] && local_tags_json="[]"
        local_body=$(extract_body "${md_file}")
        local_author=$(extract_yaml_field "${md_file}" "author")
        [[ -z "${local_author}" ]] && local_author="Claude Code"

        # Fallback section → reference.
        local_section_hint="reference"

        if [[ "${DRY_RUN}" == "true" ]]; then
            log_dry "WOULD IMPORT (fallback): section=${section} → hint=reference | title='${local_title}' | body=${#local_body}B"
            count_accepted=$((count_accepted + 1))
            continue
        fi

        http_code=$(send_note \
            "${local_title}" \
            "${local_body}" \
            "${local_section_hint}" \
            "${local_tags_json}" \
            "${local_author}") || http_code="ERR_SEND"

        case "${http_code}" in
            202|200)
                count_accepted=$((count_accepted + 1))
                log_note "OK(fallback): ${md_file} | vault_section=${section} → hint=reference"
                ;;
            4[0-9][0-9]|ERR_*)
                count_rejected=$((count_rejected + 1))
                log_warn "REJECTED(${http_code}): ${md_file} (fallback)"
                ;;
            5[0-9][0-9])
                count_error=$((count_error + 1))
                log_warn "ERROR(${http_code}): ${md_file} (fallback) — skip"
                ;;
            *)
                count_error=$((count_error + 1))
                log_warn "INCONNU(${http_code}): ${md_file} (fallback) — skip"
                ;;
        esac

        if [[ ${RATE_MS} -gt 0 ]]; then
            sleep "$(awk "BEGIN{printf \"%.3f\", ${RATE_MS}/1000}")"
        fi

    done < <(find "${section_dir}" -maxdepth 1 -name "*.md" -print0 2>/dev/null | sort -z)
done

# stats imprimées par le trap EXIT.
