//! Module d'assemblage de contexte LLM — `vault_context` v0.7.0.
//!
//! Responsabilité : orchestrer le pipeline de récupération et d'assemblage
//! des notes pertinentes en un contexte prêt pour injection LLM.
//!
//! ## Sous-modules
//!
//! | Module | Rôle |
//! |---|---|
//! | [`render`] | Rendu Raw (`render_raw`) et structuré Markdown (`render_assembled`) |
//! | [`retrieval`] | Récupération RRF (BM25 + sémantique + timeout embed + ULID-direct) |
//! | [`select`] | Scoring composite pondéré + sélection budget-aware |
//! | [`tokens`] | Estimation du budget tokens (`HeuristicEstimator`) |
//!
//! ## Modes
//!
//! - [`ContextMode::Raw`]: bit-for-bit parity with the legacy v0.6.x handler.
//!   FTS BM25-only, joined with `"\n\n---\n\n"`, budget `chars/3`, char-safe truncation.
//! - [`ContextMode::Assembled`]: full pipeline — retrieval RRF →
//!   weighted composite scoring → budget-aware selection → structured Markdown rendering.
//!
//! ## Entrée publique
//!
//! [`assemble_context`] — dispatche sur `req.mode`, retourne [`VaultContextResponse`].

pub mod compact;
pub mod reference;
pub mod render;
pub mod retrieval;
pub mod select;
pub mod skills;
pub mod tokens;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use gradatum_core::error::GradatumError;
use gradatum_dto::{ContextMode, VaultContextRequest};
use gradatum_search::{ResolvedWeights, ScoringWeightsWire, resolve_weights};
use ulid::Ulid;

use crate::api_v1::dto::{
    ContextCounts, ContextDiagnostics, IncludedNote, StubDto, VaultContextResponse,
};
use crate::api_v1::handlers::build_fts_query;
use crate::context::tokens::TokenEstimator;
use crate::metrics::{AVG_STUB_TOKENS_SAVED, ContextAssemblyLabel, ContextEfficiencyLabel};
use crate::state::AppState;

/// Dispatche sur le [`ContextMode`] et produit la [`VaultContextResponse`].
///
/// # Dispatch
///
/// - `Raw` → `assemble_raw` (parité bit-pour-bit legacy).
/// - `Assembled` → `assemble_assembled` (pipeline complet v0.7.0 : retrieval RRF +
///   scoring composite pondéré + sélection budget-aware + rendu Markdown structuré).
///
/// # Telemetry
///
/// Instrumente systématiquement, quel que soit le mode :
/// - `gradatum_vault_context_duration_seconds{mode}` — latence totale en secondes.
/// - `gradatum_vault_context_candidates{mode}` — candidats considérés (succès uniquement).
/// - `gradatum_vault_context_included{mode}` — notes retenues (succès uniquement).
/// - `gradatum_vault_context_embed_fallback_total{mode}` — incrémenté si fallback BM25.
///
/// La durée est observée même en cas d'erreur (utile pour le diagnostic).
/// Les compteurs candidats/inclus/fallback ne sont observés que sur succès
/// (données disponibles uniquement depuis `resp.diagnostics`).
///
/// # Errors
///
/// Propagé depuis les assembleurs internes :
/// - [`GradatumError`] sur échec FTS (`search_fts_with_snippet`) ou lecture de note (`get_note`).
///
/// Les vérifications ACL et tenant sont effectuées en amont dans `vault_context_impl`
/// (logic.rs) — ce module n'en est pas responsable.
///
/// # Identity guard
///
/// `identity_privileged` est résolu en amont (`vault_context_impl`, accès à `trust`)
/// via `crate::api_v1::logic::is_identity_privileged`. Quand il vaut `false`, les
/// notes de section `identity` (âmes d'agents) sont exclues des candidats AVANT toute
/// hydratation de corps — parité avec le guard `vault_search_impl` (surface RAG
/// générique, exclusion simple sans matching par-agent).
pub async fn assemble_context(
    state: &AppState,
    tenant: &str,
    req: &VaultContextRequest,
    identity_privileged: bool,
) -> Result<VaultContextResponse, GradatumError> {
    let start = Instant::now();
    // Label statique : les valeurs sont des string literals 'static (cardinalité bornée).
    let mode: &'static str = match req.mode {
        ContextMode::Raw => "raw",
        ContextMode::Assembled => "assembled",
        ContextMode::Compact => "compact",
    };

    // Dispatch vers l'assembleur approprié.
    let result = match req.mode {
        ContextMode::Raw => assemble_raw(state, tenant, req, identity_privileged).await,
        // Task 7 : pipeline complet (retrieval RRF → scoring composite → rendu structuré).
        ContextMode::Assembled => assemble_assembled(state, tenant, req, identity_privileged).await,
        // Task 8 : vue foldée F-30 (reset cache, top-K inline, reste en stubs).
        ContextMode::Compact => {
            compact::assemble_compact(state, tenant, req, identity_privileged).await
        }
    };

    // ── Télémétrie (Task 8) ──────────────────────────────────────────────────
    // Durée observée inconditionnellement (même sur erreur — diagnostics latence).
    let elapsed = start.elapsed().as_secs_f64();
    let label = ContextAssemblyLabel { mode };
    state
        .metrics
        .vault_context_duration
        .get_or_create(&label)
        .observe(elapsed);

    // Candidats, inclus et fallback uniquement sur succès (données dans diagnostics).
    if let Ok(ref resp) = result {
        let diag = &resp.diagnostics;
        state
            .metrics
            .vault_context_candidates
            .get_or_create(&label)
            .observe(diag.candidates_considered as f64);
        state
            .metrics
            .vault_context_included
            .get_or_create(&label)
            .observe(diag.included_count as f64);
        if diag.embed_fallback {
            state
                .metrics
                .vault_context_embed_fallback
                .get_or_create(&label)
                .inc();
        }
    }

    result
}

/// Assemblage brut — parité bit-pour-bit avec le handler legacy `vault_context_impl` v0.6.x.
///
/// ## Algorithme (fidèle à `logic.rs:1010-1093`)
///
/// 1. Si `req.query` est un ULID valide → note elle-même + backlinks (ULID-direct).
/// 2. Sinon → `build_fts_query` sanitize + `search_fts_with_snippet(limit=10)` → IDs.
/// 3. Pour chaque ID : `get_note` → calcul du budget résiduel (`chars/3`, per-note) →
///    troncature char-safe si débordement → accumulation.
/// 4. Jointure via [`render::render_raw`] (`"\n\n---\n\n"`).
/// 5. Budget final : `(assembled_text.chars().count() / 3)` (division entière, plancher aucun
///    — parité exacte avec le legacy `logic.rs:1087`).
///
/// ## Parité garantie
///
/// | Comportement | Legacy (`logic.rs`) | `assemble_raw` |
/// |---|---|---|
/// | Budget per-note | `note_chars.div_ceil(3).max(1)` | identique |
/// | Budget consommé | `body_part.chars().count().div_ceil(3).max(1)` | identique |
/// | Troncature | `char_indices().nth(char_limit).map(|(i, _)| i).unwrap_or(len)` | identique |
/// | Jointure | `context_parts.join("\n\n---\n\n")` | `render_raw(parts)` |
/// | Budget total | `(context.chars().count() / 3) as u32` | identique |
/// | Score sources | non calculé (`Vec<String>`) | `0.0` (IncludedNote) |
///
/// ## Mapping vers la nouvelle réponse
///
/// | Ancien champ | Nouveau champ |
/// |---|---|
/// | `context` | `assembled_text` |
/// | `estimated_tokens` | `budget_used` |
/// | `sources` (`Vec<String>`) | `included` (`Vec<IncludedNote>`, score=0.0) |
async fn assemble_raw(
    state: &AppState,
    tenant: &str,
    req: &VaultContextRequest,
    identity_privileged: bool,
) -> Result<VaultContextResponse, GradatumError> {
    let max_tokens = req.max_tokens.unwrap_or(2000).clamp(1, 8000) as usize;

    // ── Résolution des IDs candidats ─────────────────────────────────────────
    let top_note_ids: Vec<String> = if Ulid::from_string(&req.query).is_ok() {
        // Branche ULID-direct : la note elle-même + ses backlinks.
        let backlinks = state
            .search
            .backlinks(tenant, &req.query)
            .await
            .unwrap_or_default();
        let mut ids = vec![req.query.clone()];
        ids.extend(backlinks);
        ids
    } else {
        // Branche FTS : sanitize + search_fts_with_snippet(limit=10).
        let fts_q = build_fts_query(&req.query);
        if fts_q.trim_matches(['"', ' ']).is_empty() {
            // Requête vide après sanitization → contexte vide, aucune erreur.
            return Ok(VaultContextResponse {
                assembled_text: String::new(),
                included: vec![],
                budget_used: 0,
                diagnostics: ContextDiagnostics {
                    candidates_considered: 0,
                    included_count: 0,
                    embed_fallback: false,
                    skills_injected: 0,
                },
                references: vec![],
                counts: ContextCounts {
                    inline: 0,
                    stub: 0,
                    dropped: 0,
                },
                // budget_used=0 → seuil jamais atteint.
                cache_breakpoint_hint: false,
            });
        }
        let vault_id = crate::api_v1::tenant_guard::own_vault_checked(tenant);
        match state
            .search
            .search_fts_with_snippet(
                &vault_id,
                &fts_q,
                10,
                false,
                req.section.as_deref(),
                None,
                None,
                None, // no temporal filter for context retrieval
                None,
            )
            .await
        {
            Ok(hits) => hits.into_iter().map(|h| h.note_id.to_string()).collect(),
            Err(e) => return Err(e),
        }
    };

    let candidates_considered = top_note_ids.len() as u32;

    // ── Accumulation des parties de texte ────────────────────────────────────
    let mut context_parts: Vec<String> = Vec::new();
    let mut included: Vec<IncludedNote> = Vec::new();
    let mut used_tokens: usize = 0;

    for note_id in &top_note_ids {
        if used_tokens >= max_tokens {
            break;
        }
        // Task 14 (W3, iso-audit) : `get_note` scopé par argument sur le vault own (`tenant`),
        // pas le singleton — aucun split-brain read-back. `vault_context` est own-vault (pas de
        // `vault_id` requête), donc `tenant` EST le namespace cible. Rien à router ici.
        match state.search.get_note(tenant, note_id).await {
            Ok(Some(record)) => {
                // Guard identity F-34 : ne jamais hydrater le corps d'une âme dans le
                // contexte générique pour un caller non-privilégié (parité vault_search).
                if crate::api_v1::logic::identity_section_hidden(
                    identity_privileged,
                    &record.section,
                ) {
                    continue;
                }
                let note_chars = record.body_text.chars().count();
                let note_tokens = note_chars.div_ceil(3).max(1);
                let remaining = max_tokens.saturating_sub(used_tokens);
                let body_part = if note_tokens > remaining {
                    // Troncature char-safe : frontière codepoint garantie via char_indices.
                    let char_limit = remaining.saturating_mul(3);
                    let end = record
                        .body_text
                        .char_indices()
                        .nth(char_limit)
                        .map(|(i, _)| i)
                        .unwrap_or(record.body_text.len());
                    record.body_text[..end].to_string()
                } else {
                    record.body_text.clone()
                };
                let consumed = body_part.chars().count().div_ceil(3).max(1);

                // Conversion epoch ms → ISO 8601 UTC.
                let date = DateTime::<Utc>::from_timestamp_millis(record.created)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());

                included.push(IncludedNote {
                    ulid: record.id.clone(),
                    title: record.title.unwrap_or_else(|| record.id.clone()),
                    section: record.section.clone(),
                    date,
                    score: 0.0, // Mode Raw : pas de scoring composite.
                });
                context_parts.push(body_part);
                used_tokens = used_tokens.saturating_add(consumed);
            }
            Ok(None) => {
                tracing::debug!(
                    note_id = %note_id,
                    "assemble_raw: note missing, skipped"
                );
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    note_id = %note_id,
                    "assemble_raw: get_note failed"
                );
            }
        }
    }

    // ── Assemblage final ─────────────────────────────────────────────────────
    let assembled_text = render::render_raw(context_parts);
    // Parité exacte legacy : division entière (floor), PAS div_ceil.
    // Voir `logic.rs:1087` : `(context.chars().count() / 3) as u32`.
    let budget_used = (assembled_text.chars().count() / 3) as u32;
    let included_count = included.len() as u32;

    // Mode Raw : pas de split inline/stub (reference_mode non applicable).
    // `inline` = notes retenues, `stub` = 0 toujours, `dropped` = candidats - inline.
    let raw_inline = included.len();
    let raw_dropped = (candidates_considered as usize).saturating_sub(raw_inline);
    Ok(VaultContextResponse {
        assembled_text,
        included,
        budget_used,
        diagnostics: ContextDiagnostics {
            candidates_considered,
            included_count,
            embed_fallback: false,
            skills_injected: 0,
        },
        references: vec![],
        counts: ContextCounts {
            inline: raw_inline,
            stub: 0,
            dropped: raw_dropped,
        },
        cache_breakpoint_hint: budget_used > state.context.cache_breakpoint_threshold_tokens,
    })
}

// ── Filtre incrémental session F-30 (Task 6) ────────────────────────────────

/// Longueur ULID Crockford base32 (26 caractères ASCII) — aligné sur `ULID_LEN`
/// du handler `POST /api/v1/session-log/trace` (C-SA6 `is_ulid_shape`).
const SESSION_ID_ULID_LEN: usize = 26;

/// Longueur du snippet capturé dans `mark_sent` pour les nouvelles notes inline.
///
/// Aligné sur `select::STUB_SNIPPET_CHARS` (120) pour cohérence entre les snippets
/// between selection snippets (stub references) and the sent-tracker (frozen snippet).
const MARK_SENT_SNIPPET_CHARS: usize = 120;

/// Valide le format d'un `session_id` — ULID Crockford base32.
///
/// Règle : exactement [`SESSION_ID_ULID_LEN`] caractères ASCII alphanumériques.
/// Aligné sur `is_ulid_shape` du handler `POST /api/v1/session-log/trace` (C-SA6).
///
/// Un `session_id` invalide retourne [`GradatumError::InvalidInput`] → HTTP 400.
fn is_session_id_valid(s: &str) -> bool {
    s.len() == SESSION_ID_ULID_LEN && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Assemblage mode Assembled — pipeline complet v0.7.0.
///
/// Câble le pipeline complet v0.7.0 :
/// - Retrieval RRF [`retrieval::retrieve_candidates`] (BM25 + sémantique + timeout P2-3).
/// - Sanitization FTS5 (P1-1) + ULID-direct (P1-4).
/// - Scoring composite pondéré [`select::select_budget_aware`] (P0-2 forme multiplicative).
/// - Lazy body fetch: note body loaded only for retained notes.
/// - Structured Markdown rendering via [`render::render_assembled`].
/// - Budget resolution: `budget_tokens` takes priority over `max_tokens`.
/// - Incremental session filter: no re-promotion + frozen snippet.
///
/// # Errors
///
/// Propagé depuis [`retrieval::retrieve_candidates`] sur échec SQL non récupérable.
/// Les erreurs de fetch individuelles (notes absentes / erreur) sont ignorées (warn log).
/// [`GradatumError::InvalidInput`] si `session_id` ne respecte pas le format ULID (P2-2).
async fn assemble_assembled(
    state: &AppState,
    tenant: &str,
    req: &VaultContextRequest,
    identity_privileged: bool,
) -> Result<VaultContextResponse, GradatumError> {
    // ── P2-2 : validation session_id (ULID, format strict) ──────────────────
    //
    // Validation AVANT le retrieval et AVANT l'early-return `candidates.is_empty()`.
    // Raison : un vault vide (aucun candidat) provoquerait un early-return 200 OK
    // avant d'atteindre la validation — un session_id invalide passerait silencieusement.
    // Le rejet 400 doit être cohérent quel que soit le contenu du vault.
    // If-let chain (stable Rust 1.88+) : session_id présent ET format invalide → 400.
    if let Some(sid) = &req.session_id
        && !is_session_id_valid(sid)
    {
        return Err(GradatumError::InvalidInput(
            "invalid session_id: expected ULID of 26 alphanumeric chars (Crockford base32)"
                .to_owned(),
        ));
    }

    // Budget : `budget_tokens` prioritaire sur `max_tokens` (rétrocompat legacy).
    let budget: u32 = req
        .budget_tokens
        .or(req.max_tokens)
        .unwrap_or(state.context.default_budget_tokens)
        .clamp(1, 8000);
    let vault_id = crate::api_v1::tenant_guard::own_vault_checked(tenant);

    // top_n élargi pour que le scoring/sélection ait suffisamment de candidats à trier.
    // La sélection budget-aware borne la liste finale — top_n est un plafond de retrieval.
    let top_n: usize = state.context.top_n_candidates;

    // Convertir Option<String> → Option<&[&str]> pour l'appel retrieve_candidates.
    //
    // Variable temporaire `single_section_buf` nécessaire : `sections` emprunte son contenu,
    // donc `buf` doit vivre aussi longtemps que l'appel à `retrieve_candidates`.
    let single_section_buf;
    let sections: Option<&[&str]> = if let Some(s) = req.section.as_deref() {
        single_section_buf = [s];
        Some(&single_section_buf)
    } else {
        None
    };

    // Retrieval RRF — embed_fallback et candidates_considered reflètent le vrai état.
    let outcome = retrieval::retrieve_candidates(
        state,
        &vault_id,
        &req.query,
        sections,
        top_n,
        state.context.embed_timeout_ms,
    )
    .await?;

    let candidates_considered = outcome.candidates.len() as u32;
    let embed_fallback = outcome.embed_fallback;

    if outcome.candidates.is_empty() {
        return Ok(VaultContextResponse {
            assembled_text: String::new(),
            included: vec![],
            budget_used: 0,
            diagnostics: ContextDiagnostics {
                candidates_considered: 0,
                included_count: 0,
                embed_fallback,
                skills_injected: 0,
            },
            references: vec![],
            counts: ContextCounts {
                inline: 0,
                stub: 0,
                dropped: 0,
            },
            // budget_used=0 → seuil jamais atteint.
            cache_breakpoint_hint: false,
        });
    }

    // Résoudre les poids de scoring depuis la requête (conversion wire/lib découplée).
    let wire: Option<ScoringWeightsWire> = req.scoring.as_ref().map(|sw| ScoringWeightsWire {
        recency: sw.recency,
        pagerank: sw.pagerank,
        trust: sw.trust,
    });
    let weights: ResolvedWeights = resolve_weights(wire.as_ref());

    // now_ms passé en paramètre (testabilité : pas de Utc::now() dans la lib).
    let now_ms = Utc::now().timestamp_millis();
    let estimator = tokens::HeuristicEstimator;

    // ── Task 6 — Filtre incrémental session (F-30 régime normal) ─────────────
    //
    // Charge la carte des ULIDs déjà envoyés dans cette session avant le split.
    // `None` : pas de session_id OU store absent (dégradation F-29-pur, P2-4).
    // `Some(map)` : store disponible + get_sent réussi → filtre actif.
    //
    // P2-2 déjà validé en tête de fonction (avant early-return candidates empty).
    // P2-4 : `state.session_trace.is_none()` → skip silencieux, pas de panic.
    let session_sent = if let Some(ref sid) = req.session_id {
        match &state.session_trace {
            Some(store) => match store.get_sent(tenant, sid).await {
                Ok(map) => Some(map),
                Err(e) => {
                    tracing::warn!(
                        err = %e,
                        session_id = %sid,
                        "get_sent failed — F-29-pure degradation (session filter skipped)"
                    );
                    None
                }
            },
            None => {
                tracing::debug!(
                    session_id = %sid,
                    "session_trace absent — F-29-pure degradation (P2-4)"
                );
                None
            }
        }
    } else {
        None
    };

    // Sélection budget-aware : scoring composite + tri (score↓, ULID tiebreaker) + split.
    // Phase 2A inline : lazy body fetch jusqu'au budget.
    // Phase 2B stubs : candidats hors budget inline → stubs (F-29, Task 2).
    // Le budget_inline_used retourné ici sert uniquement à la sélection interne.
    // Le budget_used reporté dans la réponse est recalculé après rendu complet (P2-b).
    //
    // `reference_mode=false` (défaut F-35) : stub_budget effectif = 0 → aucun stub produit,
    // comportement inchangé pour les consommateurs existants.
    // `reference_mode=true` (F-29) : stub_budget de la config → stubs exposés dans `references`.
    let effective_stub_budget = if req.reference_mode {
        state.context.stub_budget_tokens
    } else {
        0
    };
    let (selected, stubs, _) = select::select_budget_aware(
        state,
        tenant,
        outcome.candidates,
        &weights,
        &estimator,
        budget,
        effective_stub_budget,
        now_ms,
    )
    .await?;

    // ── Task 6 — Post-traitement session : no-re-promotion + mark_sent ───────
    //
    // Les ULIDs présents dans `session_sent` ont déjà été envoyés inline dans un
    // tour précédent de cette session. Constraint 4 : ils sont INTERDITS inline
    // et forcés en stub avec snippet figé (Constraint 5 : snippet = SentEntry.snippet,
    // jamais ré-extrait depuis le body courant).
    //
    // Technique : shadowing de `selected` et `stubs` → tout le code aval utilise
    // automatiquement les valeurs post-filtre sans modification.
    let (selected, stubs) = if let Some(ref sent_map) = session_sent {
        let mut new_inline = Vec::with_capacity(selected.len());
        let mut forced: Vec<reference::Stub> = Vec::new();
        for s in selected {
            if let Some(entry) = sent_map.get(&s.note_id) {
                // Déjà-sent : forcer en stub avec snippet figé (Constraint 4 + 5).
                // No-re-promotion : cet ULID ne sera JAMAIS inline cette session.
                forced.push(reference::Stub {
                    ulid: s.note_id,
                    title: s.title,
                    section: s.section,
                    snippet: entry.snippet.clone(),
                });
            } else {
                new_inline.push(s);
            }
        }
        // Task 7 — snippet figé pour stubs BM25 déjà-sent + dedup ULID (Constraint 5).
        //
        // Les stubs produits par select_budget_aware (Phase 2B) ont leur snippet extrait
        // du body courant via `stub_from_selected`. Si un ULID figure dans `sent_map`,
        // son snippet DOIT être figé au 1er mark_sent (pas ré-extrait du body courant) :
        // Constraint 5 garantit la stabilité du snippet entre les tours d'une session.
        //
        // Dedup par ULID via BTreeMap : clé = ulid String → ordre lexicographique croissant,
        // cohérent avec le tiebreaker ULID Tasks 2/3. Priorité : forced (session, toujours
        // figé) > stubs BM25 (snippet figé si ULID dans sent_map, sinon ré-extrait valide).
        // Sémantique `or_insert` : le premier insert gagne → forced en premier.
        let mut stub_map: BTreeMap<String, reference::Stub> = BTreeMap::new();
        // Priorité 1 : forced — snippets toujours figés depuis sent_map (Constraint 5).
        for s in forced {
            stub_map.entry(s.ulid.clone()).or_insert(s);
        }
        // Priorité 2 : stubs BM25 — figer le snippet si l'ULID est dans sent_map.
        for mut s in stubs {
            if let Some(entry) = sent_map.get(&s.ulid) {
                // ULID déjà-sent tombé dans la zone stub (budget inline plein) :
                // remplacer le snippet ré-extrait par le snippet figé du 1er mark_sent.
                s.snippet = entry.snippet.clone();
            }
            stub_map.entry(s.ulid.clone()).or_insert(s);
        }
        // BTreeMap::into_values() garantit l'ordre ULID croissant (clé String lexicographique).
        let all_stubs: Vec<reference::Stub> = stub_map.into_values().collect();
        (new_inline, all_stubs)
    } else {
        // Pas de session active (no session_id, store absent, ou get_sent erreur) :
        // comportement F-29 pur inchangé.
        (selected, stubs)
    };

    // ── Guard identity F-34 (parité vault_search_impl L589-608) ──────────────
    //
    // Exclure les âmes d'agents (`section == "identity"`) pour un caller non-privilégié.
    // Placé APRÈS le split session mais AVANT `mark_sent`, le rendu et la construction
    // de `included`/`references` → aucune fuite de corps (`assembled_text`) ni de
    // titre/snippet (stubs). Les sections proviennent de `get_note` (jamais vides ici),
    // donc le volet fail-closed du helper n'a pas d'effet de bord sur ce chemin.
    // No-op pour les callers privilégiés (Studio / main-agent).
    let (selected, stubs): (Vec<_>, Vec<_>) = if identity_privileged {
        (selected, stubs)
    } else {
        (
            selected
                .into_iter()
                .filter(|s| !crate::api_v1::logic::identity_section_hidden(false, &s.section))
                .collect(),
            stubs
                .into_iter()
                .filter(|s| !crate::api_v1::logic::identity_section_hidden(false, &s.section))
                .collect(),
        )
    };

    // Marquer les nouveaux inline comme `sent` pour bloquer leur re-promotion
    // lors des tours suivants de la même session (Constraint 4).
    // Erreur non-critique : le contexte est retourné même si mark_sent échoue.
    // Guard `session_sent.is_some()` : skip si store absent ou get_sent avait échoué.
    // If-let chain (stable Rust 1.88+) : session active + store présent → mark_sent.
    if session_sent.is_some()
        && let (Some(sid), Some(store)) = (&req.session_id, &state.session_trace)
    {
        for sel in &selected {
            let snippet = reference::stub_from_selected(sel, MARK_SENT_SNIPPET_CHARS).snippet;
            if let Err(e) = store
                .mark_sent(tenant, sid, &sel.note_id, &snippet, now_ms)
                .await
            {
                tracing::warn!(
                    err = %e,
                    note_id = %sel.note_id,
                    "mark_sent failed — non-critical, context returned"
                );
            }
        }
    }

    // Compteurs de répartition F-29 (calculés avant consommation de `stubs` et `selected`).
    // `inline + stub + dropped == candidates_considered` est l'invariant de cohérence.
    let inline_count_usize = selected.len();
    let stub_count_usize = stubs.len();
    let dropped_count_usize =
        (candidates_considered as usize).saturating_sub(inline_count_usize + stub_count_usize);

    // Miroir sérialisable des stubs (StubDto = Stub sans méthodes, champs identiques).
    // Vide si `reference_mode=false` car `stubs` est vide dans ce cas (effective_stub_budget=0).
    let references: Vec<StubDto> = stubs
        .iter()
        .map(|s| StubDto {
            ulid: s.ulid.clone(),
            title: s.title.clone(),
            section: s.section.clone(),
            snippet: s.snippet.clone(),
        })
        .collect();

    // Convertir Vec<Selected> → IncludedNote (métadonnées pour la réponse JSON).
    // Le corps est délégué à render_assembled qui produit le Markdown structuré.
    let included: Vec<IncludedNote> = selected
        .iter()
        .map(|s| IncludedNote {
            ulid: s.note_id.clone(),
            title: s.title.clone(),
            section: s.section.clone(),
            date: s.date.clone(),
            score: s.score,
        })
        .collect();

    // Rendu Markdown structuré (spec §2.3, Task 7) — bloc inline seul.
    // `### <titre> · <section> · <date> · score=<X.XX>\n<corps>\n\n— source: [[<ULID>]]`
    //
    // Split inline / références (F-29, Task 3) :
    // assembled_text est d'abord construit sans le bloc References pour permettre
    // l'estimation de budget honest (P2-b). Le bloc References est appendé APRÈS
    // l'estimation, car il n'est pas imputé au budget inline (pointeurs compacts).
    let mut assembled_text = render::render_assembled(&req.query, &selected, &[]);
    let included_count = included.len() as u32;

    // ── F-58 Task 9 : injection de skills opt-in ─────────────────────────────
    //
    // Conditions : `inject_skills=true` ET `query_embedding` disponible (non-Noop,
    // pas de timeout embed). Le cas `inject_skills=false` (défaut) est COÛT NUL :
    // aucun scan, aucun appel embed, aucun lock.
    let mut skills_injected = 0u32;
    if req.inject_skills {
        if let Some(ref qemb) = outcome.query_embedding {
            let max_skills = state.context.max_skills;
            // Sous-budget skills : fraction configurable du budget principal, plancher 64 tokens.
            // Défaut : 0.15 × budget (ex : 0.15 × 2000 = 300 tokens), cohérent avec 1-3 blocs
            // de skill typiques sans empiéter sur le contexte principal.
            let sub_budget =
                ((budget as f64 * state.context.skills_budget_fraction) as u32).max(64);

            if let Some(idx) = get_or_build_skill_index(state, tenant).await {
                let ranked = skills::rank_skills(&idx, qemb, max_skills);
                if !ranked.is_empty() {
                    let (header, _toks) =
                        skills::inject_skills_header(&ranked, sub_budget, &estimator);
                    if !header.is_empty() {
                        assembled_text = format!("{header}\n\n{assembled_text}");
                        // `u32::try_from` sûr : ranked.len() ≤ max_skills = 3 ≪ u32::MAX.
                        skills_injected = ranked.len() as u32;
                    }
                }
            }
        } else {
            // Pas d'embedding disponible (Noop / timeout / fallback) → skip silencieux.
            tracing::debug!(
                "inject_skills=true but query_embedding=None (embed fallback) — injection skipped"
            );
        }
    }

    // P2-b : mesure honnête du budget consommé — inline + skills (sans bloc References).
    // Le bloc References (F-29) est appendé APRÈS pour ne pas gonfler budget_used :
    // les stubs sont des pointeurs compacts délivrés en bonus, non imputés au budget inline.
    // Sans ce recalcul, budget_used = somme(estimate(body)) des seules notes → sous-estime
    // de ~15-25% le vrai coût d'injection (en-têtes H3, séparateurs, source markers).
    let budget_used = estimator.estimate(&assembled_text);

    // Append du bloc References (F-29, Task 3) APRÈS l'estimation de budget.
    assembled_text.push_str(&render::render_references_block(&stubs));

    // ── Task 11 v0.7.2 : métriques context efficiency (F-29) ────────────────
    //
    // Observé sur le chemin nominal uniquement (counts calculés ci-dessus).
    // Les early-returns (candidates vides, session_id invalide) incrémentent 0 → skip.
    // inc_by : valeurs bornées par top_n_candidates (≤ 500) ≪ u64::MAX, cast as u64 sûr.
    {
        let ctx_label = ContextEfficiencyLabel { mode: "assembled" };
        state
            .metrics
            .context_inline_total
            .get_or_create(&ctx_label)
            .inc_by(inline_count_usize as u64);
        state
            .metrics
            .context_stub_total
            .get_or_create(&ctx_label)
            .inc_by(stub_count_usize as u64);
        state
            .metrics
            .context_dropped_total
            .get_or_create(&ctx_label)
            .inc_by(dropped_count_usize as u64);
        // Estimation tokens économisés : stub_count × AVG_STUB_TOKENS_SAVED (200 tokens/stub).
        state
            .metrics
            .context_tokens_saved
            .observe(stub_count_usize as f64 * AVG_STUB_TOKENS_SAVED);
    }

    Ok(VaultContextResponse {
        assembled_text,
        included,
        budget_used,
        diagnostics: ContextDiagnostics {
            candidates_considered,
            included_count,
            embed_fallback,
            skills_injected,
        },
        references,
        counts: ContextCounts {
            inline: inline_count_usize,
            stub: stub_count_usize,
            dropped: dropped_count_usize,
        },
        cache_breakpoint_hint: budget_used > state.context.cache_breakpoint_threshold_tokens,
    })
}

/// Retourne l'index skills depuis le cache ou le construit si absent (lazy build).
///
/// ## Pattern double-checked locking
///
/// 1. **Fast path** : read lock → si `Some`, clone `Arc<SkillIndex>` et retourne.
/// 2. **Slow path** : drop read lock → write lock → re-vérification → build.
///    En cas d'échec du build, log warn et retourne `None` (le cache reste `None`,
///    le retry est possible à la prochaine requête).
///
/// ## Concurrence
///
/// `tokio::sync::RwLock` (async-safe, conforme anti-pattern `anti-lock-across-await`).
/// Le write lock est tenu le temps du build (appels SQL + embed_batch) — acceptable
/// car le build ne se produit qu'une fois (cache persiste ensuite).
async fn get_or_build_skill_index(
    state: &AppState,
    tenant: &str,
) -> Option<Arc<skills::SkillIndex>> {
    // Fast path : cache chaud → clone Arc (zéro copie des vecteurs).
    {
        let guard = state.skills_index.read().await;
        if guard.is_some() {
            return guard.clone();
        }
    }
    // Slow path : write lock + double-checked locking.
    let mut wguard = state.skills_index.write().await;
    if wguard.is_none() {
        match skills::build_skill_index(
            tenant,
            &*state.search,
            &*state.embedder,
            state.context.embed_timeout_ms,
        )
        .await
        {
            Ok(idx) => {
                *wguard = Some(Arc::new(idx));
            }
            Err(e) => {
                tracing::warn!(err = %e, "skills: index build failed, injection skipped");
            }
        }
    }
    wguard.clone()
}
