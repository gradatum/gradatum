//! Handlers persist/* pour l'API interne server-to-worker (Wave 2, v0.5.3).
//!
//! ## Limite transactionnelle (IMPORTANT)
//!
//! Les writes sont séquentiels et NON atomiques.
//! `Arc<dyn Index>` utilise `SqliteIndex` (rusqlite via Mutex) — pas de pool sqlx,
//! impossible d'obtenir une transaction cross-write.
//! Le vault (write_note_with_id_internal) est TOUJOURS le premier write.
//! Si le vault write échoue → 409/500, aucun write index n'est tenté.
//! Si un write index échoue → WARN loggué, response 200 quand même (best-effort).
//! Les callers du worker doivent être idempotents (retryables).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use gradatum_core::author::{AuthorKind, AuthorRef};
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::index::TemporalEntry;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse,
};
use smallvec::SmallVec;
use toml::Value as TomlValue;
use tracing::{info, warn};
use ulid::Ulid;

use crate::state::AppState;

// ── Tenant ────────────────────────────────────────────────────────────────────

/// Tenant utilisé par tous les handlers persist internes.
///
/// ## Rationale
///
/// L'API interne est destinée au worker (mono-tenant aujourd'hui).
/// Le champ `tenant_id` des DTOs est conservé pour compatibilité worker
/// mais N'EST PAS utilisé pour router l'écriture — seule cette constante fait foi.
///
/// ## DT-INTERNAL-1 — dette multi-tenant
///
/// In a future multi-tenant revision, the tenant will be derived from the JWT claim
/// of the internal token (`X-Gradatum-Internal`). This constant will then be removed
/// and replaced with claim extraction.
const INTERNAL_TENANT_ID: &str = "main";

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse un ULID string → NoteId (400 si invalide).
#[allow(clippy::result_large_err)]
fn parse_ulid(s: &str) -> Result<NoteId, Response> {
    Ulid::from_string(s).map(NoteId).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("ULID invalide : {s:?} — {e}"),
        )
            .into_response()
    })
}

/// Parse une section string → Section enum (400 si invalide).
///
/// Délègue à [`Section::from_canonical_str`] (SSOT : itère sur `Section::ALL`)
/// pour éviter tout match arm hardcodé. Toute nouvelle section dans l'enum
/// devient automatiquement acceptée sans patch supplémentaire ici.
#[allow(clippy::result_large_err)]
fn parse_section(s: &str) -> Result<Section, Response> {
    Section::from_canonical_str(s).ok_or_else(|| {
        (StatusCode::BAD_REQUEST, format!("section invalide : {s:?}")).into_response()
    })
}

/// Parse un statut string → NoteStatus (400 si invalide).
#[allow(clippy::result_large_err)]
fn parse_status(s: &str) -> Result<NoteStatus, Response> {
    match s {
        "draft" => Ok(NoteStatus::Draft),
        "live" => Ok(NoteStatus::Live),
        "pending-review" => Ok(NoteStatus::PendingReview),
        "archived" => Ok(NoteStatus::Deprecated),
        _ => Err((StatusCode::BAD_REQUEST, format!("statut invalide : {s:?}")).into_response()),
    }
}

/// Parse un author string optionnel → AuthorRef.
///
/// Format attendu : "kind:id" (ex: "main-agent:backend", "human:alice").
/// Fallback : si le format est absent ou type inconnu → `MainAgent` avec l'id brut.
fn parse_author(s: &str) -> AuthorRef {
    if let Some((kind_str, id)) = s.split_once(':') {
        let kind = match kind_str {
            "human" => AuthorKind::Human,
            "main-agent" => AuthorKind::MainAgent,
            "sub-agent" => AuthorKind::SubAgent,
            "system" => AuthorKind::System,
            _ => AuthorKind::MainAgent,
        };
        AuthorRef {
            kind,
            id: id.to_string(),
            display_name: None,
        }
    } else {
        AuthorRef {
            kind: AuthorKind::MainAgent,
            id: s.to_string(),
            display_name: None,
        }
    }
}

/// Normalise et déduplique les tags depuis `Vec<String>` → `SmallVec<[Tag; 4]>`.
///
/// Comportement :
/// - Chaque tag est normalisé via `Tag::normalize` (kebab-ify, lowercase, trim, troncature 64).
/// - Les tags inrécupérables (résultat vide après normalisation) sont silencieusement ignorés,
///   avec un warn de tracing sur les transformations non triviales.
/// - La déduplication est appliquée après normalisation : deux tags distincts en entrée
///   peuvent produire la même valeur normalisée — le doublon est retiré (ordre conservé).
///
/// Infaillible : ne retourne jamais d'erreur HTTP 400 sur un tag invalide.
fn parse_tags(tags: &[String]) -> SmallVec<[Tag; 4]> {
    let mut seen = std::collections::HashSet::with_capacity(tags.len());
    let mut result: SmallVec<[Tag; 4]> = SmallVec::new();

    for t in tags {
        let norm = Tag::normalize(t.clone());

        // Warn si la valeur normalisée diffère de l'entrée originale.
        if norm.as_ref().map(|n| n.as_str()) != Some(t.as_str()) {
            tracing::warn!(
                original = %t,
                normalized = ?norm.as_ref().map(|n| n.as_str()),
                "tag normalisé"
            );
        }

        if let Some(tag) = norm {
            // Déduplication : on insère uniquement si la valeur n'a pas déjà été vue.
            if seen.insert(tag.as_str().to_owned()) {
                result.push(tag);
            }
        }
    }

    result
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `POST /internal/v1/persist/curated` — pipeline persist 2 phases.
///
/// ## Séquence
///
/// 1. Vault write (`write_note_with_id_internal`) — BLOQUANT (409/500 si erreur).
/// 2. Mutations index atomiques (`persist_curated_index_atomic`) — BLOQUANT (500 si erreur).
///    Comprend : upsert_note_title + write_temporal_entry (optionnel) + upsert_link (×N) + set_note_trust (optionnel).
///
/// ## Contrat d'atomicité (writes index)
///
/// Les 4 mutations index (étape 2) sont exécutées dans une transaction SQLite unique.
/// Si l'une échoue → TOUTES sont rollback. HTTP 500 retourné au worker.
/// Le vault write est cohérent (CoW + .history) — l'état est ré-exécutable par le worker.
///
/// ## Séparation vault/index
///
/// Le vault write (markdown disque) n'est PAS dans la même transaction que les mutations index
/// (deux systèmes de stockage distincts). L'état intermédiaire (vault OK + index rollback)
/// est temporaire et récupérable par retry du worker (idempotence).
pub(crate) async fn handle_persist_curated(
    State(state): State<AppState>,
    Json(req): Json<PersistCuratedRequest>,
) -> Response {
    let note_id = match parse_ulid(&req.note_id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let section = match parse_section(&req.section) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let status = match parse_status(&req.status) {
        Ok(s) => s,
        Err(e) => return e,
    };

    let author_ref = req.author.as_deref().map(parse_author);

    let tags = parse_tags(&req.tags);

    let frontmatter = Frontmatter {
        schema_version: 1,
        // DT-INTERNAL-1 : tenant dérivé du token claim en v0.6.x multi-tenant (Slice 2b).
        // req.tenant_id du body est ignoré — INTERNAL_TENANT_ID fait foi.
        vault_id: VaultId::new(INTERNAL_TENANT_ID),
        locus: None,
        section,
        status,
        status_reason: None,
        status_changed: None,
        tags,
        author: author_ref,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: req.provenance.clone(),
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };

    // 1. Vault write — BLOQUANT.
    let written = state
        .vault
        .write_note_with_id_internal(frontmatter, req.body.clone(), note_id)
        .await;

    let note = match written {
        Ok(n) => n,
        Err(GradatumError::Storage(ref msg)) if msg.contains("conflict: hash mismatch") => {
            return (StatusCode::CONFLICT, msg.clone()).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("vault write échoué : {e}"),
            )
                .into_response();
        }
    };

    // 2-5. Mutations index — ATOMIQUES (transaction SQLite).
    //
    // ## Contrat d'atomicité
    //
    // `persist_curated_index_atomic` exécute upsert_note_title + temporal + links + trust
    // dans une transaction SQLite unique (`unchecked_transaction`). Si l'une échoue,
    // TOUTES sont rollback (état ré-exécutable : le markdown vault est déjà écrit, idempotent).
    //
    // ## Retour d'erreur
    //
    // HTTP 500 si la transaction échoue. Le vault write est cohérent (CoW + .history).
    // Le worker re-tentera le job — l'état est récupérable.
    //
    // DT-INTERNAL-1 : tenant dérivé du token claim en v0.6.x multi-tenant (Slice 2b).
    let temporal_entry = req.temporal.as_ref().map(|temporal| {
        let anchor_src = match temporal.anchor_src.as_str() {
            "occurred_at" | "OccurredAt" => gradatum_core::index::AnchorSrc::OccurredAt,
            "event-date" | "EventDate" => gradatum_core::index::AnchorSrc::EventDate,
            "valid_from" | "ValidFrom" => gradatum_core::index::AnchorSrc::ValidFrom,
            _ => gradatum_core::index::AnchorSrc::Created,
        };
        TemporalEntry {
            note_id: req.note_id.clone(),
            vault_id: INTERNAL_TENANT_ID.to_string(),
            anchor_ms: temporal.anchor_ms,
            anchor_src,
            doc_kind: temporal.doc_kind.clone(),
            valid_until_ms: temporal.valid_until_ms,
        }
    });

    let links: Vec<(String, String)> = req
        .links
        .iter()
        .map(|l| (l.src.clone(), l.dst.clone()))
        .collect();

    if let Err(e) = state
        .search
        .persist_curated_index_atomic(
            &note.id,
            &req.title,
            temporal_entry.as_ref(),
            &links,
            req.trust,
            INTERNAL_TENANT_ID,
        )
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist/curated : transaction index échouée (vault OK, ré-exécutable) : {e}"),
        )
            .into_response();
    }

    info!(
        note_id = %req.note_id,
        section = %req.section,
        "persist/curated : OK"
    );

    Json(PersistOkResponse {
        note_id: req.note_id,
        status: "ok".to_string(),
    })
    .into_response()
}

/// `POST /internal/v1/persist/embedding` — stockage d'un vecteur d'embedding.
pub(crate) async fn handle_persist_embedding(
    State(state): State<AppState>,
    Json(req): Json<PersistEmbeddingRequest>,
) -> Response {
    let note_id = match parse_ulid(&req.note_id) {
        Ok(id) => id,
        Err(e) => return e,
    };

    match state
        .search
        .insert_note_embedding(&note_id, &req.embedder_id, req.dim, &req.vector)
        .await
    {
        Ok(()) => {
            info!(
                note_id = %req.note_id,
                embedder_id = %req.embedder_id,
                dim = req.dim,
                "persist/embedding : OK"
            );
            Json(EmbeddingOkResponse {
                note_id: req.note_id,
                embedder_id: req.embedder_id,
                dim: req.vector.len(),
            })
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert_note_embedding échoué : {e}"),
        )
            .into_response(),
    }
}

/// `POST /internal/v1/persist/forget` — marquage oubli sémantique.
///
/// ## Limite transactionnelle
///
/// Write vault (update frontmatter) suivi du mark_forgotten index.
/// Si le vault write échoue → 500, mark_forgotten non tenté.
/// Si mark_forgotten échoue → WARN, response 200 (best-effort).
pub(crate) async fn handle_persist_forget(
    State(state): State<AppState>,
    Json(req): Json<PersistForgetRequest>,
) -> Response {
    let note_id = match parse_ulid(&req.note_id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let section = match parse_section(&req.section) {
        Ok(s) => s,
        Err(e) => return e,
    };

    // Lire la note existante pour construire le frontmatter mis à jour.
    let existing = match state.vault.read_note_by_id(&req.note_id).await {
        Ok(n) => n,
        Err(GradatumError::NoteNotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                format!("note introuvable : {}", req.note_id),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("lecture note échouée : {e}"),
            )
                .into_response();
        }
    };

    let mut new_fm = existing.frontmatter.clone();
    new_fm.section = section;
    new_fm.forgotten = Some(true);
    new_fm.forgotten_at = Some(Utc::now());
    new_fm.forgotten_by = req.forgotten_by.clone();

    // Vault write — BLOQUANT.
    if let Err(e) = state
        .vault
        .write_note_with_id_internal(new_fm, req.body.clone(), note_id)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("vault forget write échoué : {e}"),
        )
            .into_response();
    }

    // Mark_forgotten dans l'index — non bloquant.
    if let Err(e) = state
        .search
        // DT-INTERNAL-1 : tenant dérivé du token claim en v0.6.x multi-tenant (Slice 2b).
        .mark_forgotten(
            INTERNAL_TENANT_ID,
            &req.note_id,
            req.forgotten_by.as_deref(),
        )
        .await
    {
        warn!(
            note_id = %req.note_id,
            error = %e,
            "persist/forget : mark_forgotten échoué (non bloquant)"
        );
    }

    Json(PersistOkResponse {
        note_id: req.note_id,
        status: "ok".to_string(),
    })
    .into_response()
}

/// `DELETE /internal/v1/note/:ulid` — suppression d'une note.
///
/// ## Séquence
///
/// 1. Suppression vault (fichier .md) — BLOQUANT (404/500 si erreur).
/// 2. Suppression index SQLite (`delete_note_from_index`) — WARN si erreur (non bloquant).
///
/// ## Limite transactionnelle
///
/// Le vault et l'index sont purgés séquentiellement (non atomique).
pub(crate) async fn handle_delete_note(
    State(state): State<AppState>,
    Path(ulid): Path<String>,
) -> Response {
    let note_id = match parse_ulid(&ulid) {
        Ok(id) => id,
        Err(e) => return e,
    };

    match state.vault.delete_note_by_id(note_id).await {
        Ok(()) => {
            info!(note_id = %ulid, "DELETE note vault : OK");
            // Purger l'index — non bloquant (best-effort).
            // DT-INTERNAL-1 : tenant dérivé du token claim en v0.6.x multi-tenant (Slice 2b).
            if let Err(e) = state
                .search
                .delete_note_from_index(INTERNAL_TENANT_ID, &ulid)
                .await
            {
                warn!(
                    note_id = %ulid,
                    error = %e,
                    "DELETE note : delete_note_from_index échoué (non bloquant)"
                );
            }
            // Purger les redirections wikilink — non bloquant (best-effort).
            if let Err(e) = state.search.delete_redirect_by_ulid(&ulid).await {
                warn!(
                    note_id = %ulid,
                    error = %e,
                    "DELETE note : delete_redirect_by_ulid échoué (non bloquant)"
                );
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(GradatumError::NoteNotFound(_)) => {
            (StatusCode::NOT_FOUND, format!("note introuvable : {ulid}")).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("delete_note échoué : {e}"),
        )
            .into_response(),
    }
}

/// `POST /internal/v1/persist/distill` — mise à jour note distillée.
///
/// ## Limite transactionnelle
///
/// Vault write → upsert_note_title → set_note_trust (non bloquants après vault).
pub(crate) async fn handle_persist_distill(
    State(state): State<AppState>,
    Json(req): Json<PersistDistillRequest>,
) -> Response {
    let note_id = match parse_ulid(&req.note_id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let section = match parse_section(&req.section) {
        Ok(s) => s,
        Err(e) => return e,
    };

    // Lire la note existante pour conserver le frontmatter canonique.
    // Si la note est absente (nouvelle note de synthèse), créer un frontmatter PendingReview.
    let mut new_fm = match state.vault.read_note_by_id(&req.note_id).await {
        Ok(existing) => {
            let mut fm = existing.frontmatter.clone();
            fm.section = section;
            fm
        }
        Err(GradatumError::NoteNotFound(_)) => {
            use gradatum_core::author::{AuthorKind, AuthorRef};
            use gradatum_core::frontmatter::ExtraFields;
            use gradatum_core::status::NoteStatus;
            use toml::Value as TomlValue;
            let mut extra = ExtraFields::empty();
            if !req.derived_from.is_empty() {
                let vals: Vec<TomlValue> = req
                    .derived_from
                    .iter()
                    .map(|id| TomlValue::String(id.clone()))
                    .collect();
                let extra_map = extra
                    .0
                    .get_or_insert_with(|| Box::new(std::collections::BTreeMap::new()));
                extra_map.insert("derived-from".to_string(), TomlValue::Array(vals));
            }
            gradatum_core::frontmatter::Frontmatter {
                schema_version: 1,
                vault_id: gradatum_core::scope::VaultId::new(&req.tenant_id),
                locus: None,
                section,
                status: NoteStatus::PendingReview,
                status_reason: Some("distilled — en attente de revue".to_string()),
                status_changed: None,
                tags: parse_tags(&req.tags),
                author: Some(AuthorRef {
                    kind: AuthorKind::System,
                    id: "vault-distiller".to_string(),
                    display_name: None,
                }),
                created: chrono::Utc::now(),
                updated: None,
                extra,
                provenance: Some("distilled".to_string()),
                forgotten: None,
                forgotten_at: None,
                forgotten_by: None,
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("lecture note échouée : {e}"),
            )
                .into_response();
        }
    };

    // Marquage source distillée (mark_source_processed) — optionnel.
    // `processed = true` + `derived-into = <synth_ulid>` dans ExtraFields.
    // Les deux clés sont dans HISTORY_EXCLUDED_FIELDS → CoW-safe.
    if req.mark_processed {
        let extra_map = new_fm
            .extra
            .0
            .get_or_insert_with(|| Box::new(std::collections::BTreeMap::new()));
        extra_map.insert("processed".to_string(), TomlValue::Boolean(true));
        if let Some(ref into_ulid) = req.derived_into {
            extra_map.insert(
                "derived-into".to_string(),
                TomlValue::String(into_ulid.clone()),
            );
        }
    }

    // Vault write — BLOQUANT.
    let written = state
        .vault
        .write_note_with_id_internal(new_fm, req.body.clone(), note_id)
        .await;

    let note = match written {
        Ok(n) => n,
        Err(GradatumError::Storage(ref msg)) if msg.contains("conflict: hash mismatch") => {
            return (StatusCode::CONFLICT, msg.clone()).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("vault distill write échoué : {e}"),
            )
                .into_response();
        }
    };

    // Upsert title — non bloquant.
    if let Err(e) = state.search.upsert_note_title(&note.id, &req.title).await {
        warn!(
            note_id = %req.note_id,
            error = %e,
            "persist/distill : upsert_note_title échoué (non bloquant)"
        );
    }

    // Trust — non bloquant.
    if let Some(trust) = req.trust
        && let Err(e) = state.search.set_note_trust(&note.id, trust).await
    {
        warn!(
            note_id = %req.note_id,
            trust,
            error = %e,
            "persist/distill : set_note_trust échoué (non bloquant)"
        );
    }

    Json(PersistOkResponse {
        note_id: req.note_id,
        status: "ok".to_string(),
    })
    .into_response()
}

// ── Tests unitaires parse_section + parse_tags ────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{parse_section, parse_tags};
    use gradatum_core::section::Section;

    /// parse_section accepte la 12ᵉ section (anti-régression stage1 bug).
    #[test]
    fn parse_section_accepts_project_map() {
        let result = parse_section("project-map");
        assert!(
            result.is_ok(),
            "project-map doit être accepté par parse_section"
        );
        assert_eq!(result.unwrap(), Section::ProjectMap);
    }

    /// parse_section accepte les 11 sections d'origine sans régression.
    #[test]
    fn parse_section_accepts_decisions() {
        let result = parse_section("decisions");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Section::Decisions);
    }

    /// parse_section rejette les chaînes inconnues avec HTTP 400.
    #[test]
    fn parse_section_rejects_bogus_with_400() {
        use axum::http::StatusCode;
        let result = parse_section("bogus");
        assert!(result.is_err(), "chaîne inconnue doit retourner Err");
        // Vérifier le status code de la Response d'erreur.
        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// Tags invalides → normalisés, pas de 400.
    #[test]
    fn parse_tags_normalizes_invalid() {
        let input: Vec<String> = vec![
            "todo".to_owned(),
            "status:OPEN".to_owned(),
            "v0.5.3".to_owned(),
            "status:OPEN".to_owned(), // doublon après normalisation
        ];
        let result = parse_tags(&input);
        let values: Vec<&str> = result.iter().map(|t| t.as_str()).collect();
        // "status:OPEN" → "status-open" (dédupliqué), "v0.5.3" → "v0-5-3"
        assert_eq!(values, vec!["todo", "status-open", "v0-5-3"]);
    }

    /// Tags déjà valides passent sans modification.
    #[test]
    fn parse_tags_valid_unchanged() {
        let input: Vec<String> = vec!["foo".to_owned(), "bar-baz".to_owned()];
        let result = parse_tags(&input);
        let values: Vec<&str> = result.iter().map(|t| t.as_str()).collect();
        assert_eq!(values, vec!["foo", "bar-baz"]);
    }

    /// Tags inrécupérables (résultat vide après normalisation) sont ignorés silencieusement.
    #[test]
    fn parse_tags_drops_irrecoverable() {
        let input: Vec<String> = vec!["valid".to_owned(), "___".to_owned(), "!!".to_owned()];
        let result = parse_tags(&input);
        let values: Vec<&str> = result.iter().map(|t| t.as_str()).collect();
        assert_eq!(values, vec!["valid"]);
    }

    /// Déduplication après normalisation : deux entrées → même valeur normalisée → une seule.
    #[test]
    fn parse_tags_deduplicates_after_normalize() {
        let input: Vec<String> = vec![
            "status:OPEN".to_owned(), // → "status-open"
            "STATUS:open".to_owned(), // → "status-open" (doublon)
            "other".to_owned(),
        ];
        let result = parse_tags(&input);
        let values: Vec<&str> = result.iter().map(|t| t.as_str()).collect();
        assert_eq!(values, vec!["status-open", "other"]);
    }

    /// Vecteur vide → résultat vide.
    #[test]
    fn parse_tags_empty_input() {
        let result = parse_tags(&[]);
        assert!(result.is_empty());
    }
}
