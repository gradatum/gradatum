//! Handlers lecture pour l'API interne server-to-worker (Wave 2+, v0.5.3).
//!
//! Endpoints lecture-seule :
//! - `GET /internal/v1/note/:ulid`              — note complète (vault)
//! - `GET /internal/v1/note/:ulid/status`       — statut scopé par vault (index, TOCTOU purge)
//! - `GET /internal/v1/note/:ulid/embedding`    — vecteur embedding (index)
//! - `GET /internal/v1/note/:ulid/trust`        — score trust (index)
//! - `GET /internal/v1/title-lookup`            — lookup ULID par titre (index)
//! - `GET /internal/v1/notes/by-locus`          — notes par préfixe locus (index)
//! - `GET /internal/v1/notes/count-unprocessed` — comptage notes live non-processed (F-112)
//! - `GET /internal/v1/notes/by-status`         — notes par statut (index)
//! - `GET /internal/v1/notes/garbage`           — notes Garbage expirées (index)
//! - `GET /internal/v1/forget/search`           — FTS5 pour scope Topic forget (index)
//! - `GET /internal/v1/notes/by-agent`          — notes par agent (index)

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::status::NoteStatus;
use serde::Serialize;
use ulid::Ulid;

use crate::state::AppState;

// ── DTOs de réponse lecture (privés à l'API interne) ─────────────────────────

/// Réponse lecture note complète.
#[derive(Debug, Serialize)]
pub(crate) struct NoteReadResponse {
    /// ULID de la note.
    pub note_id: String,
    /// SHA-256 du contenu courant (hex 64 chars).
    pub sha256_hex: String,
    /// Corps Markdown de la note.
    pub body: String,
    /// Section sérialisée (kebab-case).
    pub section: String,
    /// Statut sérialisé (kebab-case).
    pub status: String,
    /// Tags (strings validées).
    pub tags: Vec<String>,
    /// Si `true`, la note a été oubliée (frontmatter `forgotten = true`).
    pub forgotten: bool,
    /// Si `true`, la note a déjà été distillée (extra["processed"] = true).
    pub processed: bool,
}

/// Réponse lecture embedding.
#[derive(Debug, Serialize)]
pub(crate) struct EmbeddingReadResponse {
    /// ULID de la note.
    pub note_id: String,
    /// Identifiant du modèle d'embedding.
    pub embedder_id: String,
    /// Dimension du vecteur.
    pub dim: usize,
    /// Vecteur f32.
    pub vector: Vec<f32>,
}

/// Réponse lecture trust.
#[derive(Debug, Serialize)]
pub(crate) struct TrustReadResponse {
    /// ULID de la note.
    pub note_id: String,
    /// Score trust [0.0, 1.0].
    pub trust: f32,
}

/// Réponse lecture statut scopé (index).
///
/// `status` est `None` si la note est absente du vault ciblé (l'endpoint reste `200`,
/// jamais `404` : « absence » est une information, pas une erreur — le worker de purge
/// la traite comme un skip, cf. `handle_note_status`).
#[derive(Debug, Serialize)]
pub(crate) struct NoteStatusResponse {
    /// ULID de la note.
    pub note_id: String,
    /// Statut courant (kebab-case, ex. `garbage`, `live`), ou `None` si absente du vault.
    pub status: Option<String>,
}

/// Réponse title-lookup.
#[derive(Debug, Serialize)]
pub(crate) struct TitleLookupResponse {
    /// ULID de la note correspondant au titre, ou `None` si introuvable.
    pub note_id: Option<String>,
}

/// Réponse id-lookup.
#[derive(Debug, Serialize)]
pub(crate) struct IdLookupResponse {
    /// ULID confirmé si la note existe et est `live`, ou `None` sinon.
    pub note_id: Option<String>,
}

/// DTO note dans une liste de notes (identifiant + section).
#[derive(Debug, Serialize)]
pub(crate) struct NoteIdDto {
    /// ULID de la note.
    pub note_id: String,
    /// Section sérialisée (kebab-case). Peut être vide pour list_garbage.
    pub section: String,
}

/// Réponse liste de notes.
#[derive(Debug, Serialize)]
pub(crate) struct NoteListResponse {
    /// Liste des notes.
    pub note_ids: Vec<NoteIdDto>,
}

/// Réponse comptage (F-112 `count-unprocessed`).
#[derive(Debug, Serialize)]
pub(crate) struct CountResponse {
    /// Nombre de notes correspondant au filtre (plafonné à `min` si fourni).
    pub count: u64,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[allow(clippy::result_large_err)]
fn parse_ulid(s: &str) -> Result<NoteId, Response> {
    Ulid::from_string(s).map(NoteId).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid ULID: {s:?} — {e}"),
        )
            .into_response()
    })
}

/// Valide la longueur d'un paramètre de requête.
///
/// Retourne `400 Bad Request` si `param.len() > max` (protection anti-DoS et path-traversal).
/// `max` est une safety cap, non un paramètre utilisateur.
///
/// # Erreurs
///
/// - `(StatusCode::BAD_REQUEST, message)` si la longueur dépasse `max`.
#[allow(clippy::result_large_err)]
pub(super) fn validate_param_len(param: &str, max: usize, name: &str) -> Result<(), Response> {
    if param.len() > max {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "parameter `{name}` too long ({} > {max} bytes)",
                param.len()
            ),
        )
            .into_response());
    }
    Ok(())
}

/// Parse le paramètre `limit` avec sémantique explicite-0 :
/// - absent → `default`
/// - présent et 0 → 0 (liste vide)
/// - présent et > `max_cap` → `max_cap`
///
/// Contrairement à `unwrap_or(default)`, cette fonction distingue `limit=0`
/// (vide explicite) de l'absence du paramètre (défaut).
fn parse_limit(params: &HashMap<String, String>, default: usize, max_cap: usize) -> usize {
    match params.get("limit") {
        None => default,
        Some(s) => s.parse::<usize>().unwrap_or(default).min(max_cap),
    }
}

/// Parse un statut string en `NoteStatus` pour les filtres de liste.
#[allow(clippy::result_large_err)]
fn parse_note_status(s: &str) -> Result<NoteStatus, Response> {
    match s {
        "live" => Ok(NoteStatus::Live),
        "draft" => Ok(NoteStatus::Draft),
        "pending-review" => Ok(NoteStatus::PendingReview),
        "staging" => Ok(NoteStatus::Staging),
        "garbage" => Ok(NoteStatus::Garbage),
        "archived" | "deprecated" => Ok(NoteStatus::Deprecated),
        _ => Err((StatusCode::BAD_REQUEST, format!("invalid status: {s:?}")).into_response()),
    }
}

/// Résout le handle de read-back pour un endpoint interne, **gaté sur `multi_tenant`**
/// (évite le split-brain read-back).
///
/// - `multi_tenant.enabled = false` (défaut LIVE) → singleton `state.vault` inchangé
///   (byte-identical : un seul vault physique, aucun split-brain possible).
/// - `enabled = true` → route via le registre `state.vaults.resolve` sur le `vault_id`
///   effectif (`vault` absent → `"main"`), **fail-closed** : vault inconnu → 500, JAMAIS
///   un repli silencieux sur `main`.
///
/// `vault` provient d'un query param (frontière non fiable) → [`VaultId::parse`] (400 si
/// malformé), aligné sur `effective_read_vault`.
#[allow(clippy::result_large_err)]
fn resolve_read_back_reader(
    state: &AppState,
    vault: Option<&str>,
) -> Result<Arc<dyn gradatum_vault::Registry>, Response> {
    if !state.server_config.multi_tenant.enabled {
        return Ok(Arc::clone(&state.vault));
    }
    let vid = VaultId::parse(vault.unwrap_or("main"))
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid vault_id: {e}")).into_response())?;
    state.vaults.resolve(&vid).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("vault routing failed: {e}"),
        )
            .into_response()
    })
}

// ── Handlers existants ────────────────────────────────────────────────────────

/// `GET /internal/v1/note/:ulid` — lecture note complète depuis le vault.
///
/// ## Réponses
///
/// ## Query params
///
/// - `vault_id` (optionnel, défaut `"main"`) — vault de résolution du read-back.
///   Honoré uniquement à `multi_tenant.enabled = true` ; à OFF le singleton `main` reste
///   inline (byte-identical).
///
/// - `200` + JSON [`NoteReadResponse`] si la note existe.
/// - `404` si la note est absente du vault.
/// - `400` si l'ULID (ou le `vault_id`) est invalide.
/// - `500` pour toute erreur I/O inattendue (ou vault_id inconnu du registre — fail-closed).
pub(crate) async fn handle_note_read(
    State(state): State<AppState>,
    Path(ulid): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Validate ULID format first (400 on invalid, before any I/O).
    if let Err(e) = parse_ulid(&ulid) {
        return e;
    }
    // Task 14 (W3) : read-back routé par le vault effectif (fail-closed à ON, singleton à OFF).
    let reader = match resolve_read_back_reader(&state, params.get("vault_id").map(String::as_str))
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    match reader.read_note_by_id(&ulid).await {
        Ok(note) => {
            let sha256_hex = note.content_hash.hex();
            let tags: Vec<String> = note
                .frontmatter
                .tags
                .iter()
                .map(|t| t.as_str().to_string())
                .collect();

            Json(NoteReadResponse {
                note_id: ulid,
                sha256_hex,
                body: note.body.markdown,
                section: note.frontmatter.section.as_str().to_string(),
                status: note.frontmatter.status.to_string(),
                tags,
                forgotten: note.frontmatter.forgotten.unwrap_or(false),
                processed: note
                    .frontmatter
                    .extra
                    .get("processed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
            .into_response()
        }
        Err(GradatumError::NoteNotFound(_)) => {
            (StatusCode::NOT_FOUND, format!("note not found: {ulid}")).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("note read failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /internal/v1/note/:ulid/status` — lecture du statut d'une note depuis l'INDEX,
/// **scopée par vault** (C4-1e W3, TOCTOU purge).
///
/// Contrairement à `handle_note_read` (qui lit le `.md` du vault via le singleton `main`),
/// ce handler interroge `IndexStore::get_note_status(vault_id, id)` — la **même source
/// que `list_garbage`** — filtrée `WHERE vault_id = ?1 AND id = ?2`. Il ferme la fenêtre
/// TOCTOU cross-vault de la purge : la re-vérification de statut du candidat se fait dans
/// SON vault, plus dans `main`.
///
/// ## Query params
///
/// - `vault_id` (optionnel, défaut `"main"`) — vault de résolution du statut. À flag OFF
///   le worker émet `main` (ou rien) → byte-identical au comportement mono-vault.
///
/// ## Réponses
///
/// - `200` + JSON [`NoteStatusResponse`] : `status = Some(..)` si la note existe dans le
///   vault ciblé, `status = None` si absente. **L'absence n'est PAS un `404`** — c'est une
///   information exploitable (le worker de purge skip la note).
/// - `400` si l'ULID est invalide.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_note_status(
    State(state): State<AppState>,
    Path(ulid): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Valider le format ULID d'abord (400 avant toute I/O).
    if let Err(e) = parse_ulid(&ulid) {
        return e;
    }

    // C4-1e (W3) : vault de résolution optionnel, défaut "main" (byte-identical à OFF).
    let vault_id = params.get("vault_id").map(String::as_str).unwrap_or("main");
    if let Err(r) = validate_param_len(vault_id, 256, "vault_id") {
        return r;
    }

    match state.search.get_note_status(vault_id, &ulid).await {
        Ok(status) => Json(NoteStatusResponse {
            note_id: ulid,
            status: status.map(|s| s.to_string()),
        })
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get_note_status failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /internal/v1/note/:ulid/embedding` — lecture vecteur embedding depuis l'index.
///
/// L'`embedder_id` est passé en query-param `?embedder_id=<id>`.
/// Si absent, utilise `"default"` comme fallback.
///
/// ## Query params
///
/// - `vault_id` (optionnel, défaut `"main"`) — vault de résolution de l'embedding (C4-1e
///   Slice E). Un worker antérieur, sans le param, reste byte-identical (défaut main).
///
/// ## Réponses
///
/// - `200` + JSON [`EmbeddingReadResponse`] si l'embedding existe.
/// - `404` si absent.
/// - `400` si l'ULID est invalide.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_note_embedding(
    State(state): State<AppState>,
    Path(ulid): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let note_id = match parse_ulid(&ulid) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let embedder_id = params
        .get("embedder_id")
        .map(String::as_str)
        .unwrap_or("default")
        .to_string();

    // C4-1e (Slice E) EXPAND : `vault_id` optionnel, défaut "main" (un worker antérieur,
    // sans le param, reste byte-identical). Non requis — jamais de 400 sans param.
    let vault_id = params.get("vault_id").map(String::as_str).unwrap_or("main");

    match state
        .search
        .get_note_embedding(vault_id, &note_id, &embedder_id)
        .await
    {
        Ok(Some(vector)) => {
            let dim = vector.len();
            Json(EmbeddingReadResponse {
                note_id: ulid,
                embedder_id,
                dim,
                vector,
            })
            .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            format!("embedding absent for: {ulid}"),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get_note_embedding failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /internal/v1/note/:ulid/trust` — lecture score trust depuis l'index.
///
/// ## Query params
///
/// - `vault_id` (optionnel, défaut `"main"`) — vault de résolution du trust (C4-1e Slice B).
///
/// ## Réponses
///
/// - `200` + JSON [`TrustReadResponse`] si le trust est défini.
/// - `404` si aucun trust n'est indexé pour la note.
/// - `400` si l'ULID est invalide.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_note_trust(
    State(state): State<AppState>,
    Path(ulid): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let note_id = match parse_ulid(&ulid) {
        Ok(id) => id,
        Err(e) => return e,
    };

    // C4-1e (Slice B) EXPAND : `vault_id` optionnel, défaut "main" (un worker antérieur,
    // sans le param, reste byte-identical). Non requis — le contract final relève d'une
    // slice ultérieure.
    let vault_id = params.get("vault_id").map(String::as_str).unwrap_or("main");

    match state.search.get_trust(vault_id, &note_id).await {
        Ok(Some(trust)) => Json(TrustReadResponse {
            note_id: ulid,
            trust,
        })
        .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, format!("trust absent for: {ulid}")).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get_trust failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /internal/v1/vaults/active` — vaults actifs (`tenants.status='active'`, triés).
///
/// Source d'itération des crons per-vault du worker à `multi_tenant.enabled = true`
/// (C2, EX-C2-3 / INV-JOB-SCOPE). À OFF le worker n'appelle pas cet endpoint.
///
/// ## Réponses
///
/// - `200` + JSON `Vec<String>` (au minimum `["main"]` après migration 0030).
/// - `500` sur erreur SQLite.
pub(crate) async fn handle_active_vaults(State(state): State<AppState>) -> Response {
    match state.search.list_active_vaults().await {
        Ok(vaults) => Json(vaults).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list_active_vaults failed: {e}"),
        )
            .into_response(),
    }
}

// ── Nouveaux handlers lecture (worker-flip single-owner, v0.5.3) ──────────────

/// `GET /internal/v1/title-lookup?tenant=<t>&title=<title>` — lookup note par titre.
///
/// Utilisé par `handle_curate` du worker pour résoudre les wikilinks `[[...]]`.
/// Non-fatal pour le caller — une erreur retourne un `note_id: null` (absent).
///
/// ## Réponses
///
/// - `200` + JSON [`TitleLookupResponse`] (`note_id: null` si non trouvé).
/// - `400` si le paramètre `title` est absent.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_title_lookup(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // DT-INTERNAL-1 : invariant mono-vault — tenant hardcodé "main".
    // Le paramètre `tenant` dans la query string est ignoré intentionnellement.
    let tenant = "main";
    let title = match params.get("title") {
        Some(t) => t.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `title` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(title, 256, "title") {
        return r;
    }

    match state.search.title_lookup(tenant, title).await {
        Ok(note_id) => Json(TitleLookupResponse { note_id }).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("title_lookup failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /internal/v1/id-lookup?tenant=<t>&note_id=<ulid>` — vérifie qu'une note existe et est live.
///
/// Utilisé par `handle_curate` du worker pour résoudre les wikilinks `[[section:ULID]]`
/// directement par identifiant, sans passer par la correspondance H1.
/// Non-fatal pour le caller — une erreur retourne `note_id: null`.
///
/// ## Réponses
///
/// - `200` + JSON [`IdLookupResponse`] (`note_id: null` si absent ou non-live).
/// - `400` si le paramètre `note_id` est absent ou si l'ULID est invalide.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_id_lookup(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // DT-INTERNAL-1 : invariant mono-vault — tenant hardcodé "main".
    let tenant = "main";
    let note_id_str = match params.get("note_id") {
        Some(id) => id.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `note_id` parameter".to_string(),
            )
                .into_response();
        }
    };
    // Validation : le note_id doit être un ULID valide (26 chars Crockford).
    if let Err(r) = parse_ulid(note_id_str) {
        return r;
    }

    match state.search.id_lookup(tenant, note_id_str).await {
        Ok(note_id) => Json(IdLookupResponse { note_id }).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("id_lookup failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /internal/v1/notes/by-locus?vault=<v>&prefix=<p>` — notes par préfixe locus.
///
/// Utilisé par `handle_forget` (scope Locus) et `handle_distill` (scope Locus).
///
/// ## Réponses
///
/// - `200` + JSON [`NoteListResponse`].
/// - `400` si `vault` ou `prefix` est absent.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_notes_by_locus(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let vault = match params.get("vault") {
        Some(v) => v.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `vault` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(vault, 256, "vault") {
        return r;
    }
    let prefix = match params.get("prefix") {
        Some(p) => p.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `prefix` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(prefix, 512, "prefix") {
        return r;
    }

    match state.search.list_notes_by_locus_prefix(vault, prefix).await {
        Ok(rows) => {
            let note_ids = rows
                .into_iter()
                .map(|(note_id, section)| NoteIdDto { note_id, section })
                .collect();
            Json(NoteListResponse { note_ids }).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list_notes_by_locus_prefix failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /internal/v1/notes/count-unprocessed?vault=<v>&locus=<l>&min=<n>` —
/// compte les notes `live` non-`processed` d'un locus (pression de consolidation F-112).
///
/// Réutilise le chemin de récupération de [`handle_notes_by_locus`] (index → IDs de notes)
/// puis lit le frontmatter de chaque note pour filtrer `status == Live && !processed`
/// (`processed` vit dans `extra["processed"]`, pas une colonne SQL). Coût `O(N)` par locus
/// (une lecture vault par note) — assumé pour le cron distill hebdomadaire (F-112).
///
/// Le paramètre optionnel `min` plafonne le comptage : dès que `min` notes correspondantes
/// sont trouvées, le comptage s'arrête (early-exit) et retourne `{"count": min}`. Le cron
/// n'a besoin que de savoir si la pression atteint `pressure_min`, pas le total exact.
/// `min` absent = compte toutes les notes correspondantes.
///
/// ## Réponses
///
/// - `200` + JSON [`CountResponse`].
/// - `400` si `vault` ou `locus` est absent.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_count_unprocessed(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let vault = match params.get("vault") {
        Some(v) => v.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `vault` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(vault, 256, "vault") {
        return r;
    }
    let locus = match params.get("locus") {
        Some(l) => l.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `locus` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(locus, 512, "locus") {
        return r;
    }
    // `min` optionnel : plafond d'early-exit. Absent ou non-parseable → aucun plafond.
    let min: Option<u64> = params.get("min").and_then(|s| s.parse::<u64>().ok());

    let rows = match state.search.list_notes_by_locus_prefix(vault, locus).await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_notes_by_locus_prefix failed: {e}"),
            )
                .into_response();
        }
    };

    // Task 14 (W3) : read-back routé par le vault effectif (`vault` du query param) — à OFF
    // singleton `main` inchangé (byte-identical), à ON le corps lu provient du bon vault.
    let reader = match resolve_read_back_reader(&state, Some(vault)) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let mut count: u64 = 0;
    for (note_id, _section) in rows {
        match reader.read_note_by_id(&note_id).await {
            Ok(note) => {
                let is_live = note.frontmatter.status == NoteStatus::Live;
                let processed = note
                    .frontmatter
                    .extra
                    .get("processed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if is_live && !processed {
                    count += 1;
                    // Early-exit : le cron ne compare le résultat qu'à `min` (pressure_min).
                    if min.is_some_and(|m| count >= m) {
                        break;
                    }
                }
            }
            // Skew index/vault (note indexée mais `.md` absent) → ignorée, best-effort.
            Err(GradatumError::NoteNotFound(_)) => {}
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("count-unprocessed read failed: {e}"),
                )
                    .into_response();
            }
        }
    }

    Json(CountResponse { count }).into_response()
}

/// `GET /internal/v1/notes/by-status?vault=<v>&status=<s>` — notes par statut.
///
/// Utilisé par `handle_distill` (LiveNotes scope) et `handle_purge` (Garbage sans grace_days).
///
/// ## Réponses
///
/// - `200` + JSON [`NoteListResponse`] (section = "" car `list_by_status` retourne `Vec<NoteId>`).
/// - `400` si `vault`, `status` est absent, ou si `status` est invalide.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_notes_by_status(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let vault = match params.get("vault") {
        Some(v) => v.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `vault` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(vault, 256, "vault") {
        return r;
    }
    let status_str = match params.get("status") {
        Some(s) => s.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `status` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(status_str, 64, "status") {
        return r;
    }
    let status = match parse_note_status(status_str) {
        Ok(s) => s,
        Err(e) => return e,
    };

    let vault_id = VaultId::new(vault);
    match state.search.list_by_status(&vault_id, status).await {
        Ok(note_ids) => {
            let note_ids = note_ids
                .into_iter()
                .map(|id| NoteIdDto {
                    note_id: id.to_string(),
                    section: String::new(),
                })
                .collect();
            Json(NoteListResponse { note_ids }).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list_by_status failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /internal/v1/notes/garbage?vault=<v>&before_ms=<i64>&grace_days=<u32>` — notes Garbage expirées.
///
/// Utilisé par `handle_purge` quand `grace_days` est présent.
/// `section = ""` car `list_garbage_older_than` retourne `Vec<NoteId>` sans section.
///
/// ## Réponses
///
/// - `200` + JSON [`NoteListResponse`].
/// - `400` si `vault` ou `before_ms` est absent ou invalide.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_notes_garbage(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let vault = match params.get("vault") {
        Some(v) => v.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `vault` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(vault, 256, "vault") {
        return r;
    }
    let before_ms: i64 = match params.get("before_ms").and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing or invalid `before_ms` parameter (i64 required)".to_string(),
            )
                .into_response();
        }
    };
    // grace_days est passé pour information seulement — le cutoff est déjà calculé par le worker.
    // L'endpoint l'accepte sans l'utiliser pour un contrat de routage cohérent.
    let _grace_days: u32 = params
        .get("grace_days")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    match state.search.list_garbage_older_than(vault, before_ms).await {
        Ok(note_ids) => {
            let note_ids = note_ids
                .into_iter()
                .map(|id| NoteIdDto {
                    note_id: id.to_string(),
                    section: String::new(),
                })
                .collect();
            Json(NoteListResponse { note_ids }).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list_garbage_older_than failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /internal/v1/forget/search?vault=<v>&query=<q>&limit=<n>` — FTS5 pour Topic forget.
///
/// Utilisé par `handle_forget` (scope Topic).
///
/// ## Réponses
///
/// - `200` + JSON [`NoteListResponse`].
/// - `400` si `vault` ou `query` est absent.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_forget_search(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let vault = match params.get("vault") {
        Some(v) => v.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `vault` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(vault, 256, "vault") {
        return r;
    }
    let query = match params.get("query") {
        Some(q) => q.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `query` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(query, 512, "query") {
        return r;
    }
    // V3 : limit=0 explicite → liste vide (pas masqué par unwrap_or).
    let limit: usize = parse_limit(&params, 50, 200);

    match state
        .search
        .search_fts_for_forget(&VaultId::new(vault), query, limit)
        .await
    {
        Ok(rows) => {
            let note_ids = rows
                .into_iter()
                .map(|(note_id, section)| NoteIdDto { note_id, section })
                .collect();
            Json(NoteListResponse { note_ids }).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("search_fts_for_forget failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /internal/v1/notes/by-agent?agent=<a>&vaults[]=<v1>&vaults[]=<v2>` — notes par agent.
///
/// Utilisé par `handle_forget` (scope Agent).
/// Les vaults sont passés en répétant le paramètre `vaults[]`.
///
/// ## Réponses
///
/// - `200` + JSON [`NoteListResponse`].
/// - `400` si `agent` est absent.
/// - `500` pour toute erreur I/O inattendue.
pub(crate) async fn handle_notes_by_agent(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, Vec<String>>>,
) -> Response {
    let agent = match params.get("agent").and_then(|v| v.first()) {
        Some(a) => a.as_str(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing `agent` parameter".to_string(),
            )
                .into_response();
        }
    };
    if let Err(r) = validate_param_len(agent, 256, "agent") {
        return r;
    }
    let vaults: Vec<String> = params.get("vaults[]").cloned().unwrap_or_default();

    match state.search.list_notes_by_agent(agent, &vaults).await {
        Ok(rows) => {
            let note_ids = rows
                .into_iter()
                .map(|(note_id, section)| NoteIdDto { note_id, section })
                .collect();
            Json(NoteListResponse { note_ids }).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list_notes_by_agent failed: {e}"),
        )
            .into_response(),
    }
}
