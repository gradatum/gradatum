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

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use gradatum_core::author::{AuthorKind, AuthorRef};
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::index::TemporalEntry;
use gradatum_core::index_store::CuratedLinks;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse,
};
use gradatum_vault::WriteResult;
use smallvec::SmallVec;
use toml::Value as TomlValue;
use tracing::{info, warn};
use ulid::Ulid;

use crate::api_v1::write::parse_sha256_hex;
use crate::state::AppState;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Résout le handle de read-back d'un handler `persist/*`, **gaté sur `multi_tenant`**
/// (évite le split-brain read-back : le mark était scopé mais le read/write-back
/// passait par le singleton `main`).
///
/// - `multi_tenant.enabled = false` (défaut LIVE) → singleton `state.vault` inchangé
///   (byte-identical ; le worker impose `tenant_id = "main"` à OFF).
/// - `enabled = true` → route via `state.vaults.resolve` sur le `tenant_id` du job (=
///   namespace vault, source de confiance interne → [`VaultId::new`], parité avec le
///   write-back `frontmatter.vault_id`). **Fail-closed** : vault inconnu → 500, jamais
///   un repli silencieux sur `main`.
#[allow(clippy::result_large_err)]
fn resolve_persist_reader(
    state: &AppState,
    tenant_id: &str,
) -> Result<Arc<dyn gradatum_vault::Registry>, Response> {
    if !state.server_config.multi_tenant.enabled {
        return Ok(Arc::clone(&state.vault));
    }
    state.vaults.resolve(&VaultId::new(tenant_id)).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("vault routing failed: {e}"),
        )
            .into_response()
    })
}

/// Résout le vault CIBLE d'une écriture curated (C1, F-63) : dissocie le `target_vault`
/// optionnel du principal (`tenant_id`), en maintenant l'INTERDICTION du writable cross-vault.
///
/// - `target` absent → `principal` (byte-identical : namespace == `tenant_id`, chemin LIVE
///   actuel où le worker impose `tenant_id = "main"` à flag OFF).
/// - `target == principal` → `principal`.
/// - `target != principal` → `403` : le 2e vault writable reste fermé (capacité de champ
///   seulement — aucune écriture cross-vault autorisée avant décision ultérieure).
///
/// ## Invariant partagé `INV-P1-3` (SSOT contrat, pas SSOT code — L4)
///
/// `INV-P1-3` : **la cible d'écriture est toujours le vault propre du principal** ;
/// aucune écriture cross-vault n'est autorisée avant le fix ACL C2. Cette fonction est
/// l'enforcement de `INV-P1-3` sur le **listener INTERNE loopback** (worker) : garde PURE
/// `(principal, target)`, sans grant, sans scope, sans flag — le grant a déjà été payé en
/// amont à la frontière publique (cf. commentaire C4-1b dans `handle_persist_curated`).
///
/// Le pendant sur la **frontière PUBLIQUE (JWT)** est `effective_write_vault`
/// (module `crate::api_v1::tenant_guard`) : il enforce le MÊME `INV-P1-3`
/// de façon **structurelle** (aucun paramètre `target` → la cible EST toujours le vault
/// propre) et y ajoute scope + grant (surensemble légitime). Les deux sites ne PARTAGENT
/// PAS de code : couches d'auth distinctes, types de refus distincts (`Response` ici vs
/// `TenantGuardRefusal` là-bas), grant/scope/flag EXCLUSIVEMENT côté frontière publique.
/// La convergence L4 est un **invariant nommé partagé**, pas un kernel commun (le kernel
/// partagé absorbant grant/scope/flag a été REJETÉ — P0 latent : divergence de sémantique
/// de refus entre les deux couches).
#[allow(clippy::result_large_err)]
fn resolve_write_namespace(principal: &str, target: Option<&VaultId>) -> Result<VaultId, Response> {
    match target {
        Some(t) if t.as_str() != principal => Err((
            StatusCode::FORBIDDEN,
            format!(
                "write target vault '{}' != principal '{}' — writable cross-vault forbidden",
                t.as_str(),
                principal
            ),
        )
            .into_response()),
        _ => Ok(VaultId::new(principal)),
    }
}

/// Exige que le vault CIBLE d'une écriture de note soit inscrit au registre de DONNÉES
/// (`tenants`) et n'appartienne pas au registre de CODE (lot REG).
///
/// ## Pourquoi ici, et pas à la création de vault
///
/// Un vault se crée rarement ; une note s'écrit en permanence. La divergence mesurée sur
/// le LIVE (5 notes portant `vault_id` ∈ {`default`, `test`}, absentes des DEUX registres)
/// est née de ce chemin-là, pas d'une création de vault. Le point d'enforcement de
/// l'invariant est donc l'écriture, et [`handle_persist_curated`] en est l'unique site de
/// naissance d'une note curated côté production (`resolve_write_namespace` n'a pas d'autre
/// appelant).
///
/// ## Ce que cette garde vérifie — et ce qu'elle ne vérifie pas
///
/// - Refuse un `vault_id` préfixé `code-` : une note de données n'atterrit jamais dans un
///   vault dérivé de git.
/// - Refuse un `vault_id` absent de `tenants` : c'est le barreau qui ferme le trou par
///   lequel `default` et `test` sont entrés.
///
/// Does **not** check the status: the invariant is REGISTRY MEMBERSHIP.
/// `TenantStatus` has three variants — `Active`, `Suspended`, `Deleted` — and
/// all three pass here; a soft-deleted vault therefore remains writable
/// (interaction with purge: audit REG 2026-07-29, P1-3, tracked follow-up).
/// Do not "align" this with `require_active_target` (403) without reading that
/// trace: the 500-vs-403 split is deliberate (retryable vs terminal).
///
/// Fail-closed : un lookup en échec refuse (500) — jamais une inscription implicite.
#[allow(clippy::result_large_err)]
async fn require_registered_data_vault(state: &AppState, vault: &str) -> Result<(), Response> {
    if vault.starts_with(super::CODE_VAULT_PREFIX) {
        warn!(
            vault = %vault,
            "persist: write refused — code vault, never the target of a data note"
        );
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "write vault '{vault}' belongs to the code registry \
                 ('{}') — never the target of a data note",
                super::CODE_VAULT_PREFIX
            ),
        )
            .into_response());
    }
    match state.search.get_tenant_status(vault).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => {
            warn!(
                vault = %vault,
                "persist: write refused — vault absent from the `tenants` data registry"
            );
            Err((
                StatusCode::FORBIDDEN,
                format!(
                    "write vault '{vault}' is not registered in any registry — \
                     provision the vault before writing to it"
                ),
            )
                .into_response())
        }
        Err(e) => {
            warn!(
                vault = %vault,
                err = %e,
                "persist: registry lookup failed — fail-closed, write refused"
            );
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("vault registry lookup failed: {e}"),
            )
                .into_response())
        }
    }
}

/// Parse un ULID string → NoteId (400 si invalide).
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

/// Parse une section string → Section enum (400 si invalide).
///
/// Délègue à [`Section::from_canonical_str`] (SSOT : itère sur `Section::ALL`)
/// pour éviter tout match arm hardcodé. Toute nouvelle section dans l'enum
/// devient automatiquement acceptée sans patch supplémentaire ici.
#[allow(clippy::result_large_err)]
fn parse_section(s: &str) -> Result<Section, Response> {
    Section::from_canonical_str(s)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("invalid section: {s:?}")).into_response())
}

/// Parse un statut string → NoteStatus (400 si invalide).
#[allow(clippy::result_large_err)]
fn parse_status(s: &str) -> Result<NoteStatus, Response> {
    match s {
        "draft" => Ok(NoteStatus::Draft),
        "live" => Ok(NoteStatus::Live),
        "pending-review" => Ok(NoteStatus::PendingReview),
        "archived" => Ok(NoteStatus::Deprecated),
        _ => Err((StatusCode::BAD_REQUEST, format!("invalid status: {s:?}")).into_response()),
    }
}

/// Parse un author → [`AuthorRef`], **sans jamais fabriquer d'identité**.
///
/// Deux formes acceptées :
/// - **préfixée `"kind:id"`** — `kind` doit être l'une des quatre variantes canoniques
///   (`human` / `main-agent` / `sub-agent` / `system`), ex. `"main-agent:main"`,
///   `"human:alice"`. L'`id` peut contenir des `:` (seul le premier sépare `kind` de `id`) ;
/// - **nom nu** (sans `:`) — c'est l'identité résolue issue du credential (l'`owner` lu à
///   la frontière publique par `effective_author`) : l'`id` est le nom, le `kind` prend une
///   valeur par défaut documentée (voir le corps de la fonction).
///
/// ## R2 — aucune identité par défaut (Tâche 11)
///
/// R2 refuse d'*inventer une identité*, pas de défaulter une métadonnée d'audit. Sont donc
/// refusés : un `kind:` explicite mais **inconnu** (`"bogus:x"` — un `kind:id` malformé), et
/// la chaîne **vide ou blanche** (aucune identité). Un nom nu, lui, PORTE une identité
/// (l'`id`) et reste accepté — le refuser (état de 1d42c38c) cassait le chemin d'écriture
/// nominal, car un subject de credential ne peut jamais produire un `kind:id` (cf. corps).
///
/// # Errors
///
/// [`GradatumError::InvalidInput`] si la chaîne est vide/blanche, ou si elle est de la forme
/// `"kind:id"` avec un `kind` hors des quatre variantes reconnues.
fn parse_author(s: &str) -> Result<AuthorRef, GradatumError> {
    // Chaîne vide ou blanche : aucune identité à porter → refus (R2, pas de défaut).
    if s.trim().is_empty() {
        return Err(GradatumError::InvalidInput(
            "empty author — no identity resolved (R2)".to_string(),
        ));
    }

    match s.split_once(':') {
        // Forme préfixée `"kind:id"` : le `kind` DÉCLARÉ doit être positivement reconnu.
        // Un `kind:` explicite mais inconnu est un `kind:id` malformé — refusé (jamais
        // rabattu sur `MainAgent` comme l'ancien fourre-tout `_ => MainAgent`).
        Some((kind_str, id)) => {
            let kind = match kind_str {
                "human" => AuthorKind::Human,
                "main-agent" => AuthorKind::MainAgent,
                "sub-agent" => AuthorKind::SubAgent,
                "system" => AuthorKind::System,
                other => {
                    return Err(GradatumError::InvalidInput(format!(
                        "unknown author kind {other:?} — an explicit 'kind:id' must name a recognized kind (R2)"
                    )));
                }
            };
            Ok(AuthorRef {
                kind,
                id: id.to_string(),
                display_name: None,
            })
        }
        // Nom nu sans préfixe : identité RÉSOLUE issue du credential, acceptée.
        //
        // Pourquoi défaulter le `kind` ici est légitime — et n'enfreint PAS R2 :
        // le `kind` (`AuthorKind`) est une métadonnée DESCRIPTIVE d'audit ; il ne gouverne
        // AUCUNE décision d'autorisation (constat du plan Tâche 11, Step 0). C'est l'`id`
        // qui PORTE l'identité — ici le nom nu, présent et véridique. R2 (« aucune identité
        // par défaut ») est donc satisfait par l'`id`, jamais par le `kind` : défaulter le
        // `kind` ne fabrique aucune identité.
        //
        // Ce n'est pas un cas résiduel mais le chemin NOMINAL : `effective_author`
        // (api_v1/logic.rs) attribue la note au subject du credential, et le charset
        // d'`AgentId` (`a-z 0-9 -`, cf. scope.rs) INTERDIT le `:`. Un subject de credential
        // ne peut donc jamais produire un `kind:id` — refuser le nom nu (état 1d42c38c)
        // rejetait toute écriture de note neuve (400 → épuisement des retries → DLQ).
        //
        // `MainAgent` par défaut restaure le comportement d'avant 1d42c38c (moindre
        // surprise, aucune migration de données) ; il reste écrasable par une forme
        // préfixée explicite ou un `req.author` fourni.
        None => Ok(AuthorRef {
            kind: AuthorKind::MainAgent,
            id: s.to_string(),
            display_name: None,
        }),
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
                "normalized tag"
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

    // R2 : un author déclaré doit être un `"kind:id"` reconnu — sinon refus 400, jamais
    // un repli silencieux sur `MainAgent` (cf. `parse_author`). Absent (`None`) reste licite.
    let author_ref = match req.author.as_deref().map(parse_author).transpose() {
        Ok(a) => a,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("invalid author: {e}")).into_response(),
    };

    let tags = parse_tags(&req.tags);

    // C4-1b (P0 security review) : le tenant provient du job (CurateSpec.tenant_id), lui-même
    // dérivé du tenant ACL-vérifié côté `vault_write` (effective_write_vault) et propagé par le
    // worker sur ce listener loopback. À flag OFF le worker impose `main` (ensure_main_tenant),
    // donc byte-identical (req.tenant_id == "main" == ex-INTERNAL_TENANT_ID). Ex-hardcode = vecteur
    // write tiers→main (une écriture d'un tenant tiers atterrissait dans `main`).
    // C1 (T14) : namespace = `target_vault.unwrap_or(principal)`, writable cross-vault
    // INTERDIT (la garde refuse `target != principal`). À défaut de `target_vault` (chemin
    // LIVE actuel) → `VaultId::new(req.tenant_id)`, byte-identical.
    let write_vault =
        match resolve_write_namespace(req.tenant_id.as_str(), req.target_vault.as_ref()) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    // Lot REG — enforcement de l'invariant de registre au point de NAISSANCE de la note.
    if let Err(resp) = require_registered_data_vault(&state, write_vault.as_str()).await {
        return resp;
    }
    let checked_write =
        gradatum_core::scope::AclCheckedVaultId::for_system_task(write_vault.clone());

    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: write_vault,
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

    // 1. Vault write — BLOQUANT. Le champ `expected_sha256` discrimine DEUX modes (F-41) :
    //   • None → CREATE : ULID préalloué neuf, écriture INCONDITIONNELLE.
    //   • Some → RMW in-place sous lock optimiste (compare-and-swap sur le hash courant).
    // Un `expected_sha256` périmé sur une note vivante produit un job `Conflict` et laisse
    // la note INTACTE (WriteResult::Conflict → 409). Le cas fantôme + sha, et l'overwrite
    // sans sha, sont déjà refusés en amont par la garde de présence publique (logic.rs 409).
    let written_id: NoteId = if let Some(expected_hex) = req.expected_sha256.as_deref() {
        // Mode RMW — parse hex→[u8;32] (400 si malformé ; la garde publique valide déjà le
        // format, ce re-check garde le listener interne fail-closed contre un appelant direct).
        let expected = match parse_sha256_hex(expected_hex) {
            Some(h) => h,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("invalid expected_sha256 (64 hex expected): {expected_hex:?}"),
                )
                    .into_response();
            }
        };
        match state
            .vault
            .write_if_match_internal(
                &checked_write,
                frontmatter,
                req.body.clone(),
                note_id,
                expected,
            )
            .await
        {
            Ok(WriteResult::Written { .. }) => note_id,
            Ok(WriteResult::Conflict { current_sha256 }) => {
                // Hash attendu périmé → aucune écriture. Corps 409 JSON exploitable : le client
                // interne (loopback) parse `current_sha256` pour le propager dans le
                // WriteConflictDto (F-41 CAS) — un corps texte le priverait du hash gagnant.
                let current_hex = gradatum_core::identity::ContentHash(current_sha256).hex();
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({ "current_sha256": current_hex })),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("vault write failed: {e}"),
                )
                    .into_response();
            }
        }
    } else {
        // Mode CREATE — écriture inconditionnelle (l'arm `conflict: hash mismatch` reste
        // défensif : write_note_with_id_internal ne produit jamais de conflit).
        match state
            .vault
            .write_note_with_id_internal(&checked_write, frontmatter, req.body.clone(), note_id)
            .await
        {
            Ok(n) => n.id,
            Err(GradatumError::Storage(ref msg)) if msg.contains("conflict: hash mismatch") => {
                return (StatusCode::CONFLICT, msg.clone()).into_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("vault write failed: {e}"),
                )
                    .into_response();
            }
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
            // C4-1b : scopé au tenant du job (cohérent avec frontmatter.vault_id + index).
            vault_id: req.tenant_id.to_string(),
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
            &written_id,
            &req.title,
            temporal_entry.as_ref(),
            // F-147 : `authoritative` n'active la suppression des arêtes périmées que si
            // l'appelant déclare que `edges` est le jeu complet recalculé du corps courant.
            CuratedLinks {
                edges: &links,
                authoritative: req.links_authoritative,
            },
            req.trust,
            req.tenant_id.as_str(),
        )
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist/curated: index transaction failed (vault OK, re-runnable): {e}"),
        )
            .into_response();
    }

    // F-66 instrumentation: record the curator decision (path × outcome) exactly
    // once per persisted note. The worker owns the decision; the server owns the
    // Prometheus registry (:19091). Absent on the legacy dispatch path (`None`).
    if let Some(decision) = &req.curator_decision {
        state
            .metrics
            .curator_decisions
            .get_or_create(&crate::metrics::CuratorDecisionLabel {
                path: decision.path.clone(),
                outcome: decision.outcome.clone(),
            })
            .inc();
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

    // C4-1e (Slice B) EXPAND : `vault_id` optionnel du DTO, défaut "main" (payload d'un
    // worker antérieur, sans le champ, reste byte-identical). Sert de clé de partition ANN.
    let vault_id = req
        .vault_id
        .as_ref()
        .map(gradatum_core::scope::VaultId::as_str)
        .unwrap_or("main");

    match state
        .search
        .insert_note_embedding(vault_id, &note_id, &req.embedder_id, req.dim, &req.vector)
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
            format!("insert_note_embedding failed: {e}"),
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

    // Task 14 (W3) : read-back routé par le vault effectif (`req.tenant_id`) — ferme le
    // split-brain (le mark_forgotten ci-dessous est scopé, le read/write-back doit l'être aussi).
    let reader = match resolve_persist_reader(&state, req.tenant_id.as_str()) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // Lire la note existante pour construire le frontmatter mis à jour.
    let existing = match reader.read_note_by_id(&req.note_id).await {
        Ok(n) => n,
        Err(GradatumError::NoteNotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                format!("note not found: {}", req.note_id),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("note read failed: {e}"),
            )
                .into_response();
        }
    };

    let mut new_fm = existing.frontmatter.clone();
    new_fm.section = section;
    new_fm.forgotten = Some(true);
    new_fm.forgotten_at = Some(Utc::now());
    new_fm.forgotten_by = req.forgotten_by.clone();

    // C4-1b : témoin = vault de la note (préservé depuis le frontmatter existant, read main-bound).
    let checked_forget =
        gradatum_core::scope::AclCheckedVaultId::for_system_task(new_fm.vault_id.clone());

    // Vault write — BLOQUANT.
    if let Err(e) = state
        .vault
        .write_note_with_id_internal(&checked_forget, new_fm, req.body.clone(), note_id)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("vault forget write failed: {e}"),
        )
            .into_response();
    }

    // Mark_forgotten dans l'index — non bloquant.
    //
    // C4-1e (Slice E) : scopé sur le vault du job (`req.tenant_id`), plus jamais un
    // hardcode `"main"`. À flag OFF `req.tenant_id == "main"` (le worker impose main),
    // donc byte-identical. À ON, l'ex-hardcode marquait l'homonyme `main` de l'index au
    // lieu de la note du vault secondaire visé (classe forget-cross-vault).
    if let Err(e) = state
        .search
        .mark_forgotten(
            req.tenant_id.as_str(),
            &req.note_id,
            req.forgotten_by.as_deref(),
        )
        .await
    {
        warn!(
            note_id = %req.note_id,
            error = %e,
            "persist/forget: mark_forgotten failed (non-blocking)"
        );
    }

    Json(PersistOkResponse {
        note_id: req.note_id,
        status: "ok".to_string(),
    })
    .into_response()
}

/// `POST /internal/v1/note/{ulid}/forget-resync?vault_id=<vault_id>` — répare la marque
/// d'oubli de l'index sans toucher au frontmatter ni aux colonnes d'audit (A7-bis).
///
/// ## Pourquoi une route distincte de `persist/forget`
///
/// `persist/forget` réécrit le frontmatter et estampille `forgotten_at`/`forgotten_by` à
/// l'instant de l'appel : le rejouer sur une note déjà oubliée détruit la piste d'audit du
/// PREMIER oubli. Cette route ne fait que remettre `forgotten = 1` dans l'index — elle
/// répare la désynchronisation sans rien coûter en auditabilité.
///
/// ## Désynchronisations réparées
///
/// Ce n'est pas une fenêtre de course : deux chemins de production ordinaires la
/// produisent — `vault_unforgot` efface la marque d'index sans toucher au `.md`, et
/// `handle_persist_forget` ci-dessus rend 200 best-effort quand son `mark_forgotten`
/// échoue après un write vault réussi.
///
/// ## Réponses
///
/// - **204 No Content** — marque ré-affirmée (idempotent).
/// - **400 Bad Request** — ULID malformé ou `vault_id` hors borne.
/// - **404 Not Found** — aucune ligne d'index pour ce couple (ULID, `vault_id`).
/// - **500 Internal Server Error** — échec SQLite.
pub(crate) async fn handle_note_forget_resync(
    State(state): State<AppState>,
    Path(ulid): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Valider le format ULID d'abord (400 avant toute I/O).
    if let Err(e) = parse_ulid(&ulid) {
        return e;
    }

    // Vault cible optionnel, défaut "main" — même contrat que les routes voisines
    // `/note/{ulid}` et `/note/{ulid}/status`.
    let vault_id = params.get("vault_id").map(String::as_str).unwrap_or("main");
    if let Err(r) = super::reads::validate_param_len(vault_id, 256, "vault_id") {
        return r;
    }

    match state.search.reassert_forgotten(vault_id, &ulid).await {
        Ok(()) => {
            info!(
                note_id = %ulid,
                vault_id = %vault_id,
                "persist/forget-resync: index mark re-asserted"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(GradatumError::NoteNotFound(_)) => {
            (StatusCode::NOT_FOUND, format!("note not found: {ulid}")).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("reassert_forgotten failed: {e}"),
        )
            .into_response(),
    }
}

// ── Cascade delete partagée (F-100) ────────────────────────────────────────────

/// Résultat de la cascade physique de suppression d'une note (F-100).
///
/// La suppression vault (`.md` + `.history`) est fatale et propagée par
/// [`cascade_delete_note`]. Les purges index et redirect sont **retournées** ici
/// pour que l'appelant choisisse sa politique : best-effort (handler interne de
/// purge) ou stricte (endpoint public `vault_delete`).
#[derive(Debug)]
pub(crate) struct CascadeDeleteOutcome {
    /// Erreur non fatale de la purge index SQLite, le cas échéant.
    pub index_error: Option<GradatumError>,
    /// Erreur non fatale de la purge des redirections wikilink, le cas échéant.
    pub redirect_error: Option<GradatumError>,
    /// Chemin de l'archive `.md` (relatif au vault) si disposition = archivage.
    pub archive_path: Option<String>,
}

/// Disposition physique du `.md` + `.history` au moment de la cascade (F-100 1.6).
///
/// Le choke point [`cascade_delete_note`] applique la même garde PROTECTED_DELETE puis
/// la même cascade index quelle que soit la disposition — seul le sort des fichiers
/// change : destruction (job Purge sur garbage) vs archivage réversible (delete admin).
#[derive(Debug, Clone)]
pub(crate) enum VaultDisposition {
    /// Détruit physiquement le `.md` + `.history` (job Purge / GC tier).
    Destroy,
    /// Déplace le `.md` + `.history` sous `.archive/` + inscrit le registre (delete admin).
    Archive {
        /// `sub` du token à l'origine de l'archivage (traçabilité registre).
        archived_by: Option<String>,
        /// Échéance de rétention (epoch ms) au-delà de laquelle le GC détruit.
        gc_due_ms: i64,
    },
}

/// Supprime physiquement une note en cascade : vault (`.md` + `.history`, BLOQUANT)
/// puis index SQLite (cascade + FTS + temporal + ANN) puis redirections wikilink.
///
/// **Source unique** de la cascade de suppression (F-100 1.2) — partagée par le
/// handler interne de purge [`handle_delete_note`] (best-effort) et l'endpoint
/// public `vault_delete` (strict). Zéro duplication de cascade.
///
/// # Garde system-wide (F-100 P1-1)
///
/// La section de la note est résolue **côté serveur** ([`Section::is_protected_delete`])
/// AVANT toute mutation. Si elle appartient à [`Section::PROTECTED_DELETE`], la cascade
/// est refusée ([`GradatumError::Forbidden`]) sans toucher au vault ni à l'index. Comme
/// c'est le **choke point unique** de toute suppression physique, cette garde protège
/// à la fois l'endpoint `vault_delete` ET le job Purge (qui atteint cette cascade via
/// l'endpoint interne). L'erreur `Forbidden` est **distincte** d'un échec technique :
/// l'appelant Purge la reconnaît (HTTP 403) et journalise un SKIP sans faire échouer le
/// batch. Une note protégée en `garbage` reste donc non purgée.
///
/// # Errors
///
/// - [`GradatumError::Forbidden`] si la note est dans une section protégée — aucune
///   mutation n'est effectuée.
/// - [`GradatumError::NoteNotFound`] si le `.md` est absent (signal d'idempotence).
/// - [`GradatumError::Storage`] (ou autre) sur échec I/O vault — fatal, la cascade
///   s'arrête avant de toucher l'index.
///
/// Les erreurs index et redirect ne sont **pas** propagées comme `Err` : elles
/// remontent dans [`CascadeDeleteOutcome`] pour que chaque appelant décide de leur
/// caractère fatal (best-effort vs strict).
pub(crate) async fn cascade_delete_note(
    state: &AppState,
    vault_id: &str,
    ulid: &str,
    note_id: NoteId,
    disposition: VaultDisposition,
) -> Result<CascadeDeleteOutcome, GradatumError> {
    // 0. Garde PROTECTED_DELETE system-wide — refus AVANT toute mutation.
    //
    // Résolution de la section côté serveur (autorité unique de la protection),
    // indépendante de ce que l'appelant a — ou n'a pas — vérifié. Protège l'API
    // et le Purge par le même point de passage. Une note absente de l'index
    // (section = None) n'est pas protégeable ici : la suppression vault suivante
    // tranchera (NoteNotFound si le `.md` est absent).
    if let Some(section) = state.search.get_note_section(vault_id, ulid).await?
        && Section::is_protected_delete(&section)
    {
        return Err(GradatumError::Forbidden(format!(
            "protected section: '{section}' can never be hard-deleted (PROTECTED_DELETE, no bypass)"
        )));
    }
    // 1. Vault (.md + .history) — BLOQUANT (propage NoteNotFound / I/O).
    //    Destroy = destruction physique ; Archive = déplacement sous .archive/ + registre.
    //    Variantes `_in` (C3a, P2-2) : les chemins disque sont résolus sous le vault
    //    PROPRIÉTAIRE de la note (`vault_id`), pas sous le vault racine de l'instance —
    //    sinon les `.md` d'un vault secondaire survivraient à la purge (résidu orphelin).
    let archive_path = match disposition {
        VaultDisposition::Destroy => {
            state.vault.delete_note_by_id_in(vault_id, note_id).await?;
            None
        }
        VaultDisposition::Archive {
            archived_by,
            gc_due_ms,
        } => {
            let outcome = state
                .vault
                .archive_note_by_id_in(vault_id, note_id, archived_by, gc_due_ms)
                .await?;
            Some(outcome.archive_path)
        }
    };
    // 2. Index SQLite (cascade FK + FTS + temporal + ANN) — erreur retournée.
    //
    // CONTRAINTE D'ORDRE (crash-safety, P2-1) : le déplacement `.md`+registre (étape 1)
    // précède OBLIGATOIREMENT la dé-indexation (étape 2). `archive_note` n'est pas atomique
    // (read→write→delete + insert registre) ; un crash entre 1 et 2 laisse la note encore
    // indexée pointant sur un `.md` déplacé (drift `read_note`=NoteNotFound) MAIS l'archive
    // et sa ligne registre active existent → récupérable via `restore` (lit `archive_path`).
    // L'ordre inverse (dé-indexer d'abord) rendrait un crash IRRÉCUPÉRABLE (note absente de
    // l'index ET `.md` non encore archivé). Ne jamais inverser 1 et 2.
    let index_error = state
        .search
        .delete_note_from_index(vault_id, ulid)
        .await
        .err();
    // 3. Redirections wikilink — erreur retournée.
    let redirect_error = state
        .search
        .delete_redirect_by_ulid(vault_id, ulid)
        .await
        .err();
    Ok(CascadeDeleteOutcome {
        index_error,
        redirect_error,
        archive_path,
    })
}

/// `DELETE /internal/v1/note/:ulid` — suppression d'une note.
///
/// ## Séquence
///
/// 1. Suppression vault (fichier .md) — BLOQUANT (404/500 si erreur).
/// 2. Suppression index SQLite (`delete_note_from_index`) — WARN si erreur (non bloquant).
///
/// ## Scope multi-vault (C4-1e, Slice E) — EXPAND
///
/// Le vault CIBLE provient d'un query param OPTIONNEL `?vault_id=` (défaut `"main"`),
/// exactement comme `handle_note_trust` / `handle_note_embedding` (Slice B). À flag OFF
/// le worker n'émet pas le param (ou émet `main`) → défaut `"main"` → byte-identical.
/// À ON, l'ex-hardcode `"main"` supprimait l'homonyme `main` d'un ULID au lieu de la note
/// du vault secondaire visé (clobber `main` + no-op cible : classe delete-cross-vault).
/// `cascade_delete_note` est déjà scopé par ce `vault_id` (garde PROTECTED_DELETE, purge
/// `.md`/index/redirect) — seule la SOURCE du tenant change.
///
/// ## Limite transactionnelle
///
/// Le vault et l'index sont purgés séquentiellement (non atomique).
pub(crate) async fn handle_delete_note(
    State(state): State<AppState>,
    Path(ulid): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let note_id = match parse_ulid(&ulid) {
        Ok(id) => id,
        Err(e) => return e,
    };

    // C4-1e (Slice E) EXPAND : vault cible optionnel, défaut "main" (un worker antérieur,
    // sans le param, reste byte-identical). Le contract final (param requis) relève d'une
    // slice ultérieure.
    let vault_id = params.get("vault_id").map(String::as_str).unwrap_or("main");

    // Job Purge (garbage) = DESTRUCTION physique (tier GC), pas archivage : une note
    // en garbage a déjà traversé le cycle downgrade → l'archivage est réservé au delete
    // admin on-demand (F-100 1.6, frontière « archive(=delete) < GC physique »).
    match cascade_delete_note(&state, vault_id, &ulid, note_id, VaultDisposition::Destroy).await {
        Ok(outcome) => {
            info!(note_id = %ulid, "DELETE note vault : OK");
            // Purges index/redirect best-effort (non bloquant pour la purge interne).
            if let Some(e) = outcome.index_error {
                warn!(
                    note_id = %ulid,
                    error = %e,
                    "DELETE note: delete_note_from_index failed (non-blocking)"
                );
            }
            if let Some(e) = outcome.redirect_error {
                warn!(
                    note_id = %ulid,
                    error = %e,
                    "DELETE note: delete_redirect_by_ulid failed (non-blocking)"
                );
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(GradatumError::NoteNotFound(_)) => {
            (StatusCode::NOT_FOUND, format!("note not found: {ulid}")).into_response()
        }
        // Section protégée (garde system-wide) : 403 distinct d'un échec technique.
        // Le Purge reconnaît ce statut et journalise un SKIP sans échouer le batch.
        Err(GradatumError::Forbidden(msg)) => (StatusCode::FORBIDDEN, msg).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("delete_note failed: {e}"),
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

    // Task 14 (W3) : read-back routé par le vault effectif (`req.tenant_id`) — le write-back
    // (`VaultId::new(req.tenant_id)` sur le frontmatter neuf) doit lire le MÊME vault.
    let reader = match resolve_persist_reader(&state, req.tenant_id.as_str()) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // Lire la note existante pour conserver le frontmatter canonique.
    // Si la note est absente (nouvelle note de synthèse), créer un frontmatter PendingReview.
    let mut new_fm = match reader.read_note_by_id(&req.note_id).await {
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
                vault_id: gradatum_core::scope::VaultId::new(req.tenant_id.as_str()),
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
                format!("note read failed: {e}"),
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

    // C4-1b : témoin = vault de la note (frontmatter existant conservé, ou VaultId(req.tenant_id)
    // pour une note de synthèse neuve — cf. construction de new_fm ci-dessus).
    let checked_distill =
        gradatum_core::scope::AclCheckedVaultId::for_system_task(new_fm.vault_id.clone());

    // Vault write — BLOQUANT.
    let written = state
        .vault
        .write_note_with_id_internal(&checked_distill, new_fm, req.body.clone(), note_id)
        .await;

    let note = match written {
        Ok(n) => n,
        Err(GradatumError::Storage(ref msg)) if msg.contains("conflict: hash mismatch") => {
            return (StatusCode::CONFLICT, msg.clone()).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("vault distill write failed: {e}"),
            )
                .into_response();
        }
    };

    // Upsert title — non bloquant. Scopé sur le vault de la note écrite (C4-1e A2).
    if let Err(e) = state
        .search
        .upsert_note_title(note.frontmatter.vault_id.as_str(), &note.id, &req.title)
        .await
    {
        warn!(
            note_id = %req.note_id,
            error = %e,
            "persist/distill: upsert_note_title failed (non-blocking)"
        );
    }

    // Trust — non bloquant. Scopé sur le vault de la note écrite (C4-1e A3).
    if let Some(trust) = req.trust
        && let Err(e) = state
            .search
            .set_note_trust(note.frontmatter.vault_id.as_str(), &note.id, trust)
            .await
    {
        warn!(
            note_id = %req.note_id,
            trust,
            error = %e,
            "persist/distill: set_note_trust failed (non-blocking)"
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
    use super::{parse_author, parse_section, parse_tags, resolve_write_namespace};
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;

    /// C1 (T14) : `target_vault` absent → namespace = principal (byte-identical).
    #[test]
    fn write_target_absent_falls_back_to_principal() {
        let ns = resolve_write_namespace("main", None).expect("target absent → principal");
        assert_eq!(ns.as_str(), "main");
    }

    /// C1 (T14) : `target_vault == principal` → accepté (namespace = principal).
    #[test]
    fn write_target_equal_principal_ok() {
        let t = VaultId::new("main");
        let ns = resolve_write_namespace("main", Some(&t)).expect("target == principal → ok");
        assert_eq!(ns.as_str(), "main");
    }

    /// C1 (T14) : `target_vault != principal` → 403 (writable cross-vault INTERDIT).
    #[test]
    fn write_guard_rejects_target_neq_principal() {
        use axum::http::StatusCode;
        let t = VaultId::new("vault-b");
        let resp = resolve_write_namespace("main", Some(&t))
            .expect_err("target != principal → refus (writable cross-vault interdit)");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// L4 — `INV-P1-3` sur la couche INTERNE uniquement (`resolve_write_namespace`,
    /// listener loopback worker) : la cible d'écriture est toujours le vault propre.
    ///
    /// `resolve_write_namespace` enforce `INV-P1-3` (cible d'écriture == vault propre du
    /// principal) : la SEULE cible acceptée est le principal lui-même (absent ou égal) ;
    /// toute cible tierce → 403 avec le message EXACT `write target vault '{}' != principal
    /// '{}' — writable cross-vault forbidden`. Ce test balaie plusieurs principaux/cibles
    /// ET vérifie le **corps EXACT** de la réponse 403 — surface de sécurité observable à
    /// préserver contre un refactor futur — pour prouver que la sémantique de refus est
    /// cohérente quel que soit le nom de vault.
    ///
    /// Portée : ce test prouve la couche INTERNE UNIQUEMENT. La couche PUBLIQUE
    /// (`effective_write_vault`, module `crate::api_v1::tenant_guard`, JWT) tient le MÊME
    /// `INV-P1-3` par **construction structurelle** (aucun paramètre `target` → la cible
    /// EST toujours le vault propre) : le cas `target != principal` y est **inatteignable**
    /// (impossible à construire), l'invariant y est donc tenu structurellement, PAS par ce
    /// test. Il n'y a **pas d'équivalence mécanique** entre les deux sites : couches d'auth
    /// distinctes, types de refus distincts (`Response` ici vs `TenantGuardRefusal`
    /// là-bas), grant/scope/flag EXCLUSIVEMENT côté frontière publique — le kernel partagé
    /// absorbant grant/scope/flag a été REJETÉ (P0 latent). La convergence L4 est un
    /// **invariant nommé partagé**, pas un kernel commun.
    #[tokio::test]
    async fn write_namespace_inv_p1_3_internal_layer_own_vault_only() {
        use axum::http::StatusCode;

        // Cible == principal (explicite ou absente) → toujours acceptée == principal.
        for principal in ["main", "vault-b", "tenant-42"] {
            let own = VaultId::new(principal);
            assert_eq!(
                resolve_write_namespace(principal, Some(&own))
                    .expect("target == principal → ok")
                    .as_str(),
                principal,
            );
            assert_eq!(
                resolve_write_namespace(principal, None)
                    .expect("target absent → principal")
                    .as_str(),
                principal,
            );
        }

        // Cible tierce (≠ principal) → 403 systématique, corps EXACT préservé
        // (SSOT du message de sécurité observable, verrouillé contre tout refactor).
        let cases = [
            ("main", "vault-b"),
            ("vault-b", "main"),
            ("tenant-42", "main"),
        ];
        for (principal, foreign) in cases {
            let t = VaultId::new(foreign);
            let resp = resolve_write_namespace(principal, Some(&t))
                .expect_err("target != principal → refus INV-P1-3");
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);

            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("lecture du corps de la réponse 403");
            let body = String::from_utf8(body.to_vec()).expect("corps 403 en UTF-8");
            let expected = format!(
                "write target vault '{foreign}' != principal '{principal}' — writable cross-vault forbidden"
            );
            assert_eq!(body, expected, "corps 403 EXACT INV-P1-3 (couche interne)");
        }
    }

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

    // ── parse_author (Tâche 11 — R2 : aucune identité/kind par défaut) ─────────────

    /// Les quatre kinds canoniques `"kind:id"` continuent de parser (variantes légitimes).
    #[test]
    fn parse_author_accepts_recognized_kinds() {
        use gradatum_core::author::AuthorKind;
        let cases = [
            ("human:alice", AuthorKind::Human, "alice"),
            ("main-agent:main", AuthorKind::MainAgent, "main"),
            ("sub-agent:backend", AuthorKind::SubAgent, "backend"),
            ("system:cron-decay", AuthorKind::System, "cron-decay"),
        ];
        for (input, kind, id) in cases {
            let a = parse_author(input).expect("un kind reconnu doit parser");
            assert_eq!(a.kind, kind, "kind de {input:?}");
            assert_eq!(a.id, id, "id de {input:?}");
        }
    }

    /// Un `kind:` inconnu (ex-fourre-tout `_ => MainAgent`) est refusé par une erreur
    /// typée, jamais rabattu sur `MainAgent`.
    #[test]
    fn parse_author_rejects_unknown_kind() {
        use gradatum_core::error::GradatumError;
        let err = parse_author("bogus:x").expect_err("un kind inconnu doit être refusé (R2)");
        assert!(
            matches!(err, GradatumError::InvalidInput(_)),
            "erreur typée InvalidInput attendue, obtenu : {err:?}"
        );
    }

    /// Un nom nu sans préfixe est une **identité résolue** (l'`owner` du credential,
    /// lu à la frontière publique via `effective_author`) : il est accepté, avec l'`id`
    /// égal au nom et le `kind` par défaut documenté. R2 est satisfait par l'`id`, pas
    /// par le `kind` (métadonnée descriptive sans effet d'autorisation).
    #[test]
    fn parse_author_accepts_bare_name() {
        use gradatum_core::author::AuthorKind;
        let a = parse_author("agent-buzz").expect("un nom nu (identité de credential) doit parser");
        assert_eq!(a.id, "agent-buzz", "l'id porte l'identité du credential");
        assert_eq!(
            a.kind,
            AuthorKind::MainAgent,
            "kind par défaut documenté sur nom nu"
        );
    }

    /// Une chaîne vide ou blanche ne porte aucune identité → refus (R2, pas de défaut).
    #[test]
    fn parse_author_rejects_empty_or_blank() {
        use gradatum_core::error::GradatumError;
        for input in ["", "   "] {
            let err = parse_author(input)
                .expect_err("une chaîne vide/blanche ne porte aucune identité (R2)");
            assert!(
                matches!(err, GradatumError::InvalidInput(_)),
                "erreur typée InvalidInput attendue pour {input:?}, obtenu : {err:?}"
            );
        }
    }

    // ── Test de FRONTIÈRE — la vraie leçon de l'incident 1d42c38c ──────────────────
    //
    // Deux tests unitaires verts se contredisaient sans que rien ne le détecte :
    // `effective_author` (api_v1/logic.rs) dérive l'author du subject de credential —
    // un nom NU (charset `AgentId` = `a-z 0-9 -`, cf. scope.rs, JAMAIS de `:`) — tandis
    // que le `parse_author` de 1d42c38c refusait tout nom nu. Chaque unité passait ; le
    // chemin d'écriture nominal était pourtant cassé (400 → épuisement des retries → DLQ).
    //
    // Ce test traverse la frontière `effective_author → author de la requête →
    // parse_author` : il aurait été ROUGE dès 1d42c38c et rend la réintroduction du
    // défaut impossible. Il vit dans ce module `#[cfg(test)]` (et non dans `tests/`) car
    // `effective_author` est `pub(crate)` et `parse_author` privée — les rendre publiques
    // pour un test externe exigerait de toucher `logic.rs`, hors périmètre.

    /// Un author dérivé du subject de credential (nom nu) traverse `parse_author` sans
    /// être refusé, et l'identité (`id`) est préservée bout-en-bout.
    #[test]
    fn effective_author_bare_subject_survives_parse_author() {
        use crate::api_v1::logic::effective_author;
        use gradatum_core::scope::AgentId;

        // 1. Frontière publique : credential sans `req.author` → author dérivé du subject.
        //    (v2.0.0 Task 10 : `effective_author` renvoie `Result` — un author fourni ou une
        //    absence d'identité serait un `Err` ; ici le sujet est résolu, donc `Ok(nom nu)`.)
        let subject = AgentId::new("agent-buzz");
        let author_str = effective_author(&None, Some(&subject))
            .expect("un nom nu (identité de credential) doit être dérivé du sujet");
        assert_eq!(
            author_str, "agent-buzz",
            "author dérivé = subject de credential, un nom nu sans ':'"
        );

        // 2. Ce même author voyage dans la requête et atteint `parse_author` (persist).
        let parsed = parse_author(&author_str)
            .expect("un nom nu issu d'un credential est une identité résolue — il doit parser");

        // 3. L'identité (`id`) est préservée telle quelle bout-en-bout.
        assert_eq!(
            parsed.id, "agent-buzz",
            "l'id porte l'identité réelle issue du credential"
        );
    }
}

// ── Tests garde PROTECTED_DELETE system-wide (F-100 P1-1) ─────────────────────
//
// Le choke point `cascade_delete_note` est le point de passage unique de toute
// suppression physique (endpoint `vault_delete` ET job Purge). Ces tests prouvent
// que la garde y refuse une section protégée AVANT toute mutation, indépendamment
// de ce que l'appelant a vérifié — c'est la protection porteuse du scénario audit
// (note council PATCH→garbage → Purge → note toujours présente).
#[cfg(test)]
mod guard_tests {
    use super::{AppState, GradatumError, VaultDisposition, cascade_delete_note};
    use gradatum_acl_policy::AclEngine;
    use gradatum_auth::jwt::JwtService;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::index::Index;
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;
    use gradatum_vault::{Registry, Vault};
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Construit un `AppState` de test avec un vrai `Vault` + index partagé.
    async fn build_state() -> (AppState, Arc<Vault>, TempDir) {
        let tmp = TempDir::new().expect("TempDir guard_tests");
        let vault = Arc::new(
            Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
                .await
                .expect("Vault::create guard_tests"),
        );
        let idx = vault.index().clone();
        let jwt = JwtService::new_ephemeral();
        let acl = AclEngine::from_preset_str("").expect("AclEngine guard_tests");
        let registry: Arc<dyn Registry> = vault.clone();
        let mut state = AppState::with_jwt_and_acl(jwt, acl).with_vault_arc(registry);
        state.search = Arc::clone(&idx) as Arc<dyn Index>;
        (state, vault, tmp)
    }

    /// Frontmatter minimal pour une note de la section donnée, en `garbage`.
    fn garbage_frontmatter(section: Section) -> Frontmatter {
        let now = chrono::Utc::now();
        Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus: None,
            section,
            status: NoteStatus::Live,
            status_reason: None,
            status_changed: Some(now),
            tags: Default::default(),
            author: None,
            created: now,
            updated: None,
            extra: ExtraFields::empty(),
            provenance: None,
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        }
    }

    /// Scénario audit : une note `council` en garbage atteignant la cascade est
    /// refusée (`Forbidden`) AVANT toute mutation — index ET `.md` préservés.
    #[tokio::test]
    async fn cascade_refuses_protected_section_and_preserves_note() {
        let (state, vault, _tmp) = build_state().await;

        let note = vault
            .write_note(
                garbage_frontmatter(Section::Council),
                "verdict council".to_string(),
            )
            .await
            .expect("write_note council");
        let id = note.id;
        vault
            .update_status(id, NoteStatus::Garbage, None)
            .await
            .expect("update_status Live→Garbage");

        let res = cascade_delete_note(
            &state,
            "main",
            &id.to_string(),
            id,
            VaultDisposition::Destroy,
        )
        .await;
        assert!(
            matches!(res, Err(GradatumError::Forbidden(_))),
            "une note council doit être refusée par la garde : {res:?}"
        );

        // Aucune mutation : la note reste indexée ET son `.md` est intact.
        let section = state
            .search
            .get_note_section("main", &id.to_string())
            .await
            .expect("get_note_section");
        assert_eq!(
            section.as_deref(),
            Some("council"),
            "la note council doit rester présente dans l'index"
        );
        assert!(
            vault.read_note(id).await.is_ok(),
            "le `.md` council ne doit pas avoir été supprimé"
        );
    }

    /// Non-régression : une section NON protégée passe la garde et est supprimée.
    #[tokio::test]
    async fn cascade_allows_non_protected_section() {
        let (state, vault, _tmp) = build_state().await;

        let note = vault
            .write_note(
                garbage_frontmatter(Section::Feedback),
                "note feedback".to_string(),
            )
            .await
            .expect("write_note feedback");
        let id = note.id;
        vault
            .update_status(id, NoteStatus::Garbage, None)
            .await
            .expect("update_status Live→Garbage");

        let res = cascade_delete_note(
            &state,
            "main",
            &id.to_string(),
            id,
            VaultDisposition::Destroy,
        )
        .await;
        assert!(
            res.is_ok(),
            "une note feedback doit être supprimable : {res:?}"
        );

        let section = state
            .search
            .get_note_section("main", &id.to_string())
            .await
            .expect("get_note_section");
        assert!(
            section.is_none(),
            "la note feedback doit avoir été purgée de l'index"
        );
    }
}
