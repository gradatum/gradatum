//! `POST /api/v1/code_scope` — code-map read interface.
//!
//! Dedicated endpoint that intentionally bypasses the `vault_id ≠ main` 403 guard
//! used for cross-tenant isolation. It MUST therefore — security invariant #1 —
//! reject (400) any `vault` not starting with `code-`: otherwise it becomes a
//! read hole into `main` that cancels the single-vault mitigation. Validation
//! is performed HERE, before any index query ([`validate_code_vault_id`]).
//!
//! Security invariant #2 (G10-P1): code_scope reads source code indexed in `code_vault`
//! tables that carry NO `tenant` column (migrations 0017/0018). Only privileged
//! contexts (Studio, mTLS, main-agent) are authorized — a regular tenant token
//! MUST NOT read cross-tenant source code. Gated by the private `is_code_scope_privileged`.
//!
//! # Contract
//!
//! | Method | Path | Body | Response | Codes |
//! |--------|------|------|----------|-------|
//! | POST | `/api/v1/code_scope` | [`CodeScopeRequest`] | [`CodeScopeResponse`] | 200 / 400 / 401 / 403 / 404 / 500 |
//!
//! - `vault`: MUST start with `code-` + anti-traversal charset → **400** otherwise.
//! - `vault`: MUST have been ingested at least once (entry in `code_vault`) → **404** otherwise.
//! - `selector.kind` ∈ {`query`, `path`, `symbol`} → **400** otherwise.
//! - `budget_tokens`: default 800, clamped to `[1, 8000]`.
//! - Known vault + selector with no match → **200** `{entries:[], total_matched:0}`.
//!
//! # BM25-only
//!
//! No trust/decay/ANN scoring: code notes have neither embeddings nor trust in
//! the current version. Ranking = BM25 + structural cohesion bonus. Documented to
//! set accurate client expectations (ANN/embeddings deferred).
//!
//! # Token budget
//!
//! Descending rank order, then cut to K **whole** entries such that Σ tokens ≤ budget.
//! Intra-entry truncation is NEVER performed (accuracy over coverage). `truncated=true`
//! when entries are omitted.
//!
//! # Drift check before serving
//!
//! Before returning results, the current hash of the **distinct files** of the retained
//! entries is compared to the stored hash. Hash mismatch → `stale=true` + async
//! re-generation enqueued (never a blocking synchronous re-parse). Cost is bounded to
//! the files present in the result.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use axum::{Extension, Json, extract::State, http::StatusCode};
use gradatum_core::error::GradatumError;
use gradatum_core::index_store::{CodeScopeEntryRaw, CodeSelector};
use gradatum_core::trust::TrustContext;
use gradatum_dto::{
    CodeScopeEntry, CodeScopeRequest, CodeScopeResponse, DEFAULT_BODY_BUDGET_TOKENS,
    DEFAULT_BUDGET_TOKENS, MAX_CALLERS_PER_ENTRY, is_valid_selector_kind,
};
use sha2::{Digest, Sha256};

use crate::state::AppState;

/// Upper bound on the token budget (anti-abuse — an extremely large budget equals a full scan).
const MAX_BUDGET_TOKENS: u32 = 8_000;

/// Upper bound on the number of candidates fetched from the index before budget trimming.
/// The token budget then cuts to K entries — this cap protects memory.
const MAX_CANDIDATES: usize = 500;

/// Token estimation divisor: ~4 characters per token (GPT-like heuristic).
///
/// Approximation `tokens ≈ ceil(chars / 4)`. Conservative (slight over-count),
/// which serves the Σ tokens ≤ budget invariant (never an under-count).
const CHARS_PER_TOKEN: usize = 4;

/// Builds the final `CodeScopeEntry` list from retained raw entries.
///
/// Shared helper between [`code_scope_impl`] and [`code_scope`] — eliminates ~40 lines
/// of duplication (A3). Applies `include_body` slicing, `callers_truncated` signalling
/// (A1), and body-budget accounting.
///
/// # Arguments
///
/// * `kept` — retained raw entries (already budget-trimmed, ranked).
/// * `file_data` — single-pass I/O map (source_path → (stale, bytes)).
/// * `stale_paths` — set of stale source paths (derived from `file_data`).
/// * `callers_map` — batch reverse-dep map (qualified_name → callers). Empty if
///   `include_callers=false`.
/// * `include_body` — whether to attach source spans.
/// * `body_budget` — per-request body token budget.
/// * `max_callers` — cap per entry (typically [`MAX_CALLERS_PER_ENTRY`]).
///
/// Returns `(entries, body_truncated)`.
#[allow(clippy::too_many_arguments)]
fn build_entries(
    kept: Vec<CodeScopeEntryRaw>,
    file_data: &HashMap<String, (bool, Vec<u8>)>,
    stale_paths: &HashSet<String>,
    mut callers_map: HashMap<String, Vec<String>>,
    include_body: bool,
    body_budget: usize,
    max_callers: usize,
) -> (Vec<CodeScopeEntry>, bool) {
    let mut body_tokens_used = 0usize;
    let mut body_truncated = false;
    let mut entries: Vec<CodeScopeEntry> = Vec::with_capacity(kept.len());

    for e in kept {
        let stale = stale_paths.contains(&e.source_path);
        let body = if include_body && !stale {
            extract_body_from_file_data(file_data, &e).and_then(|(body_str, cost)| {
                if body_tokens_used + cost <= body_budget {
                    body_tokens_used += cost;
                    Some(body_str)
                } else {
                    body_truncated = true;
                    None
                }
            })
        } else {
            None
        };

        // Extract callers from the batch map (O(1) remove — avoids clone).
        let raw_callers = callers_map.remove(&e.qualified_name).unwrap_or_default();
        // A1 — signal troncature : le flag est `true` si le batch a saturé `max_callers`
        // (la liste retournée contient exactement `max_callers` entrées = cap SQL atteint).
        let callers_truncated = raw_callers.len() >= max_callers;
        let callers: Vec<String> = raw_callers.into_iter().take(max_callers).collect();

        entries.push(CodeScopeEntry {
            note_id: e.note_id.0.to_string(),
            source_path: e.source_path,
            kind: e.kind,
            qualified_name: e.qualified_name,
            signature: e.signature.unwrap_or_default(),
            deps: e.deps,
            stale,
            body,
            callers,
            callers_truncated,
        });
    }

    (entries, body_truncated)
}

/// Logique métier de `code_scope` réutilisable depuis le serveur MCP natif.
///
/// Retourne `Result<CodeScopeResponse, GradatumError>` au lieu de codes HTTP.
/// L'invariant de sécurité N°1 (vault DOIT commencer par `"code-"`) est vérifié
/// par l'appelant AVANT d'invoquer cette fonction.
///
/// # Errors
///
/// - [`GradatumError::Unauthorized`] si non authentifié.
/// - [`GradatumError::InvalidInput`] si selector invalide, value vide/trop longue,
///   ou vault inconnu (non ingéré).
/// - [`GradatumError::Storage`] si requête index échoue.
///
/// # Panics
///
/// Ne panique pas.
pub(crate) async fn code_scope_impl(
    state: &AppState,
    trust: &TrustContext,
    req: CodeScopeRequest,
) -> Result<CodeScopeResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }

    state
        .read_usage_accumulators
        .code_scope
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Validation selector.
    if !is_valid_selector_kind(&req.selector.kind) {
        return Err(GradatumError::InvalidInput(format!(
            "invalid selector.kind: {:?}",
            req.selector.kind
        )));
    }
    let value = req.selector.value.trim();
    if value.is_empty() || value.len() > 512 {
        return Err(GradatumError::InvalidInput(
            "selector.value empty or too long (max 512)".to_owned(),
        ));
    }
    let selector = match req.selector.kind.as_str() {
        "query" => CodeSelector::Query(value.to_string()),
        "path" => CodeSelector::Path(value.to_string()),
        "symbol" => CodeSelector::Symbol(value.to_string()),
        _ => {
            return Err(GradatumError::InvalidInput(format!(
                "unknown selector.kind: {:?}",
                req.selector.kind
            )));
        }
    };

    let budget = req
        .budget_tokens
        .unwrap_or(DEFAULT_BUDGET_TOKENS)
        .clamp(1, MAX_BUDGET_TOKENS) as usize;

    let body_budget = req
        .body_budget_tokens
        .unwrap_or(DEFAULT_BODY_BUDGET_TOKENS)
        .clamp(1, 32_000);

    // Existence du vault.
    match state
        .search
        .get_code_vault_repo_path(&req.vault)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, vault = %req.vault, "code_scope_impl: get_code_vault_repo_path failed");
            GradatumError::Storage("vault read failed".to_owned())
        })? {
        Some(_) => {}
        None => {
            tracing::debug!(vault = %req.vault, "code_scope_impl: nonexistent vault");
            return Err(GradatumError::InvalidInput(format!(
                "vault '{}' unknown (never ingested)",
                req.vault
            )));
        }
    }

    let raw: Vec<CodeScopeEntryRaw> = state
        .search
        .code_scope_query(&req.vault, &selector, MAX_CANDIDATES)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, vault = %req.vault, "code_scope_impl: code_scope_query failed");
            GradatumError::Storage("code scope query failed".to_owned())
        })?;

    let total_matched = raw.len() as u32;
    let ranked = rank_structure_aware(raw);

    let mut kept: Vec<CodeScopeEntryRaw> = Vec::new();
    let mut used_tokens = 0usize;
    let mut truncated = false;
    for entry in ranked {
        let cost = estimate_entry_tokens(&entry);
        if used_tokens + cost > budget {
            truncated = true;
            continue;
        }
        used_tokens += cost;
        kept.push(entry);
    }

    let file_data = read_files_single_pass(state, &req.vault, &kept).await;

    let stale_paths: HashSet<String> = file_data
        .iter()
        .filter_map(|(path, (stale, _))| if *stale { Some(path.clone()) } else { None })
        .collect();

    if !stale_paths.is_empty() {
        enqueue_regen(&req.vault, &stale_paths);
    }

    // A2 — Batch reverse-deps lookup (1 requête SQL pour tous les symboles).
    let callers_map = if req.include_callers {
        let names: Vec<&str> = kept.iter().map(|e| e.qualified_name.as_str()).collect();
        match state
            .search
            .code_scope_reverse_deps_batch(&req.vault, &names, MAX_CALLERS_PER_ENTRY)
            .await
        {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(
                    err = %err,
                    vault = %req.vault,
                    "code_scope_impl: code_scope_reverse_deps_batch failed — empty callers"
                );
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    // A3 — helper partagé build_entries.
    let (entries, body_truncated) = build_entries(
        kept,
        &file_data,
        &stale_paths,
        callers_map,
        req.include_body,
        body_budget,
        MAX_CALLERS_PER_ENTRY,
    );

    Ok(CodeScopeResponse {
        entries,
        truncated,
        total_matched,
        body_truncated,
    })
}

/// `POST /api/v1/code_scope` — see module documentation.
///
/// # Errors
///
/// - `401 Unauthorized`: unauthenticated request.
/// - `400 Bad Request`: `vault` is not `code-*` or has an invalid charset (security
///   invariant #1), `selector.kind` is out of vocabulary, or `value` is empty.
/// - `403 Forbidden`: authenticated caller is not a privileged context (G10-P1,
///   invariant #2 — not Studio, mTLS, or main-agent).
/// - `404 Not Found`: valid `code-*` vault but never ingested (absent from the
///   `code_vault` table). Criterion: presence of an entry in `code_vault` (populated
///   by `run_ingest` via `set_code_vault_repo_path`). If derived notes exist without
///   a `code_vault` entry (out-of-CLI usage), the vault is treated as non-existent
///   — unsupported scenario (any CLI ingest calls both).
/// - `500 Internal Server Error`: storage failure on the index side.
///
/// G10-P1: ACL 403 enforced via the private `is_code_scope_privileged` — only Studio, mTLS,
/// and main-agent contexts are authorized (invariant #2). The `code-` prefix validation
/// (invariant #1) remains as defense-in-depth. Any non-privileged authenticated caller
/// → 403 Forbidden. Known vault + selector with no match → 200 `{entries:[], total_matched:0}`.
/// Distinction: vault-does-not-exist (404) vs vault-exists-but-no-match (200).
pub async fn code_scope(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<CodeScopeRequest>,
) -> Result<Json<CodeScopeResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // ── INVARIANT SÉCU N°2 (BLOQUANT, G10-P1) ──────────────────────────────────
    // code_scope lit le code source indexé des projets (vaults `code-*`). Ces index
    // n'ont PAS de colonne `tenant` dans `code_vault` (migrations 0017/0018).
    // Seuls les contextes système (Studio) et le main-agent (propriétaire SSI)
    // sont autorisés — un token tenant ordinaire NE DOIT PAS accéder au code source
    // d'un projet cross-tenant. Sans cette garde, tout tenant avec un api-key + grant
    // peut lire le code source d'un vault `code-*` via POST /api/v1/code_scope.
    if !is_code_scope_privileged(&trust) {
        tracing::warn!(
            subject = ?trust.subject(),
            tenant = ?trust.tenant_id(),
            vault = %req.vault,
            "code_scope: access denied — not a privileged context (G10-P1)"
        );
        return Err(StatusCode::FORBIDDEN);
    }

    // Télémétrie usage read-path (v0.5.3 #4) — coût ~0 (AtomicU64 Relaxed, aucun I/O).
    state
        .read_usage_accumulators
        .code_scope
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // ── INVARIANT SÉCU N°1 (BLOQUANT) ─────────────────────────────────────────
    // code_scope contourne la garde 403 vault_id≠main de Slice 2a. Il DOIT donc
    // rejeter tout vault non-`code-` — sinon trou de lecture vers `main`.
    if !validate_code_vault_id(&req.vault) {
        tracing::warn!(
            vault = %req.vault,
            "code_scope: vault rejected (security invariant #1 — not `code-` or invalid charset)"
        );
        return Err(StatusCode::BAD_REQUEST);
    }

    // ── Validation selector ────────────────────────────────────────────────────
    if !is_valid_selector_kind(&req.selector.kind) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let value = req.selector.value.trim();
    if value.is_empty() || value.len() > 512 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let selector = match req.selector.kind.as_str() {
        "query" => CodeSelector::Query(value.to_string()),
        "path" => CodeSelector::Path(value.to_string()),
        "symbol" => CodeSelector::Symbol(value.to_string()),
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    let budget = req
        .budget_tokens
        .unwrap_or(DEFAULT_BUDGET_TOKENS)
        .clamp(1, MAX_BUDGET_TOKENS) as usize;

    let body_budget = req
        .body_budget_tokens
        .unwrap_or(DEFAULT_BODY_BUDGET_TOKENS)
        .clamp(1, 32_000);

    // ── Existence du vault ────────────────────────────────────────────────────
    match state
        .search
        .get_code_vault_repo_path(&req.vault)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, vault = %req.vault, "code_scope: get_code_vault_repo_path failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })? {
        Some(_) => {}
        None => {
            tracing::debug!(vault = %req.vault, "code_scope: nonexistent vault → 404");
            return Err(StatusCode::NOT_FOUND);
        }
    }

    // ── Requête index (BM25-only) ─────────────────────────────────────────────
    let raw: Vec<CodeScopeEntryRaw> = state
        .search
        .code_scope_query(&req.vault, &selector, MAX_CANDIDATES)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, vault = %req.vault, "code_scope: code_scope_query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let total_matched = raw.len() as u32;
    let ranked = rank_structure_aware(raw);

    let mut kept: Vec<CodeScopeEntryRaw> = Vec::new();
    let mut used_tokens = 0usize;
    let mut truncated = false;
    for entry in ranked {
        let cost = estimate_entry_tokens(&entry);
        if used_tokens + cost > budget {
            truncated = true;
            continue;
        }
        used_tokens += cost;
        kept.push(entry);
    }

    // ── Passe I/O unique (A1 council) ─────────────────────────────────────────────
    // Une seule `fs::read` par fichier distinct : sert à la fois le hash de fraîcheur
    // ET les bytes du corps (include_body). Garantit l'invariant 3 (≤1 read/fichier)
    // ET l'invariant 2 par construction (stale⟺bytes différents ⟹ spans exacts si !stale).
    // S1 (BLOQUANT) : validation anti-traversal avant toute lecture.
    let file_data = read_files_single_pass(&state, &req.vault, &kept).await;

    let stale_paths: HashSet<String> = file_data
        .iter()
        .filter_map(|(path, (stale, _))| if *stale { Some(path.clone()) } else { None })
        .collect();

    if !stale_paths.is_empty() {
        enqueue_regen(&req.vault, &stale_paths);
    }

    // ── A2 — Batch reverse-deps lookup ────────────────────────────────────────
    let callers_map = if req.include_callers {
        let names: Vec<&str> = kept.iter().map(|e| e.qualified_name.as_str()).collect();
        match state
            .search
            .code_scope_reverse_deps_batch(&req.vault, &names, MAX_CALLERS_PER_ENTRY)
            .await
        {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(
                    err = %err,
                    vault = %req.vault,
                    "code_scope: code_scope_reverse_deps_batch failed — empty callers"
                );
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    // ── A3 — Helper partagé ───────────────────────────────────────────────────
    let (entries, body_truncated) = build_entries(
        kept,
        &file_data,
        &stale_paths,
        callers_map,
        req.include_body,
        body_budget,
        MAX_CALLERS_PER_ENTRY,
    );

    Ok(Json(CodeScopeResponse {
        entries,
        truncated,
        total_matched,
        body_truncated,
    }))
}

/// Validates a `vault_id` for `code_scope` — security invariant #1.
#[must_use]
pub fn validate_code_vault_id(vault: &str) -> bool {
    if !vault.starts_with("code-") {
        return false;
    }
    if vault.len() > 128 {
        return false;
    }
    let suffix = &vault["code-".len()..];
    if suffix.is_empty() {
        return false;
    }
    vault
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn rank_structure_aware(mut entries: Vec<CodeScopeEntryRaw>) -> Vec<CodeScopeEntryRaw> {
    let present: HashSet<&str> = entries.iter().map(|e| e.qualified_name.as_str()).collect();
    let bonuses: Vec<f64> = entries
        .iter()
        .map(|e| {
            let n = e
                .deps
                .iter()
                .filter(|d| present.contains(d.as_str()))
                .count();
            0.5 * n as f64
        })
        .collect();
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    idx.sort_by(|&a, &b| {
        let sa = entries[a].bm25 - bonuses[a];
        let sb = entries[b].bm25 - bonuses[b];
        sa.partial_cmp(&sb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| entries[a].qualified_name.cmp(&entries[b].qualified_name))
    });
    let mut out = Vec::with_capacity(entries.len());
    let mut taken: Vec<Option<CodeScopeEntryRaw>> = entries.drain(..).map(Some).collect();
    for i in idx {
        if let Some(e) = taken[i].take() {
            out.push(e);
        }
    }
    out
}

fn estimate_entry_tokens(e: &CodeScopeEntryRaw) -> usize {
    let mut chars = e.source_path.len() + e.kind.len() + e.qualified_name.len();
    if let Some(sig) = &e.signature {
        chars += sig.len();
    }
    chars += e.deps.iter().map(|d| d.len()).sum::<usize>();
    chars += 40;
    chars.div_ceil(CHARS_PER_TOKEN).max(1)
}

/// Single I/O pass: reads each distinct file EXACTLY ONCE.
///
/// Returns `HashMap<source_path, (stale: bool, bytes: Vec<u8>)>`:
/// - `stale=true` when the hash differs from the stored value, the file is missing,
///   or the path is rejected by the anti-traversal check.
/// - `bytes` = raw file content (used for the body when `!stale`).
///
/// **Invariant A2**: `stale=false` ⟺ file is byte-identical to the ingest snapshot
/// (whole-file hash) ⟹ lines `[start_line..=end_line]` are exact.
/// No per-span hash re-validation needed.
/// This invariant comment is reproduced at the slice point in [`extract_body_from_file_data`].
///
/// **Invariant S1** (blocking): `source_path` is validated against path-traversal before
/// any read. `repo_abs.join(source_path).canonicalize()` then `starts_with(repo_abs_canonical)`.
/// Absolute paths / paths containing `..` / paths outside the repo →
/// `stale=true, bytes=[]` (no exfiltration).
///
/// Bounded cost: ≤ 1 `fs::read` per distinct file (≤ `MAX_CANDIDATES` files).
/// If the repo path is unknown → empty map (drift-detection skipped).
async fn read_files_single_pass(
    state: &AppState,
    vault: &str,
    entries: &[CodeScopeEntryRaw],
) -> HashMap<String, (bool, Vec<u8>)> {
    let mut result: HashMap<String, (bool, Vec<u8>)> = HashMap::new();
    if entries.is_empty() {
        return result;
    }

    let repo_path = match state.search.get_code_vault_repo_path(vault).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::debug!(vault = %vault, "code_scope: unknown repo path → drift SKIP");
            return result;
        }
        Err(e) => {
            tracing::warn!(err = %e, vault = %vault, "code_scope: get_code_vault_repo_path KO → drift SKIP");
            return result;
        }
    };

    let repo_abs = Path::new(&repo_path);
    let repo_canonical = match repo_abs.canonicalize() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(err = %e, repo = %repo_path, "code_scope: canonicalize repo KO → drift SKIP");
            return result;
        }
    };

    let distinct_paths: Vec<String> = entries
        .iter()
        .map(|e| e.source_path.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let stored = match state
        .search
        .code_freshness_hashes(vault, &distinct_paths)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(err = %e, vault = %vault, "code_scope: code_freshness_hashes KO → drift SKIP");
            return result;
        }
    };

    for path in &distinct_paths {
        // ── S1 anti-traversal : rejeter paths absolus, contenant .., ou hors repo ──
        // include_body transforme la lecture en exfiltration si non validé.
        if path.contains("..") || Path::new(path.as_str()).is_absolute() {
            tracing::warn!(path = %path, "code_scope: path rejected anti-traversal (S1)");
            result.insert(path.clone(), (true, Vec::new()));
            continue;
        }
        let candidate = repo_abs.join(path);
        let canonical = match candidate.canonicalize() {
            Ok(c) => c,
            Err(_) => {
                result.insert(path.clone(), (true, Vec::new()));
                continue;
            }
        };
        if !canonical.starts_with(&repo_canonical) {
            tracing::warn!(
                path = %path,
                canonical = %canonical.display(),
                repo = %repo_canonical.display(),
                "code_scope: path outside repo rejected (S1)"
            );
            result.insert(path.clone(), (true, Vec::new()));
            continue;
        }

        let Some(stored_hash) = stored.get(path) else {
            result.insert(path.clone(), (true, Vec::new()));
            continue;
        };
        match std::fs::read(&canonical) {
            Ok(bytes) => {
                let current = sha256_hex(&bytes);
                let is_stale = &current != stored_hash;
                result.insert(path.clone(), (is_stale, bytes));
            }
            Err(_) => {
                result.insert(path.clone(), (true, Vec::new()));
            }
        }
    }

    result
}

/// Extracts the body of a symbol from the bytes read during the single I/O pass.
///
/// Returns `Some((body_str, cost_tokens))` or `None` if not extractable.
///
/// **Invariant A2**: called ONLY when `!stale` (enforced by the caller).
/// `stale=false` ⟺ file is byte-identical to the ingest snapshot ⟹
/// lines `[start_line..=end_line]` are exact. No re-validation needed.
///
/// Degenerate span (`start > end` or `start > line_count`) → `None` (accuracy over coverage).
fn extract_body_from_file_data(
    file_data: &HashMap<String, (bool, Vec<u8>)>,
    entry: &CodeScopeEntryRaw,
) -> Option<(String, usize)> {
    let (stale, bytes) = file_data.get(&entry.source_path)?;
    // Double-guard A2 : ne pas servir de corps sur une entrée stale.
    // L'appelant filtre déjà, mais défense en profondeur.
    if *stale || bytes.is_empty() {
        return None;
    }

    let (start_line, end_line) = entry.span?;
    if start_line == 0 || start_line > end_line {
        return None;
    }

    let content = std::str::from_utf8(bytes).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let nb_lines = lines.len() as u32;

    if start_line > nb_lines {
        return None;
    }

    let start_idx = (start_line - 1) as usize;
    let end_idx = (end_line.min(nb_lines) - 1) as usize;

    let body_lines = &lines[start_idx..=end_idx];
    let body_str = body_lines.join("\n");

    let cost = body_str.len().div_ceil(CHARS_PER_TOKEN).max(1);

    Some((body_str, cost))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let hash: [u8; 32] = Sha256::digest(bytes).into();
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

fn enqueue_regen(vault: &str, paths: &HashSet<String>) {
    tracing::info!(
        vault = %vault,
        stale_count = paths.len(),
        stale_paths = ?paths,
        "code_scope: stale files detected — regen required (re-run `code update`)"
    );
}

/// Vrai si l'appelant est autorisé à interroger un vault `code-*` via
/// `POST /api/v1/code_scope`. Lecture de code source — privilège système.
///
/// Callers autorisés :
///   - session Studio (admin UI) ;
///   - session mTLS (service interne) ;
///   - propriétaire SSI `main-agent`.
///
/// G10-P1 : un token tenant ordinaire NE DOIT PAS lire le code source d'un
/// vault `code-*` (pas de colonne `tenant` dans `code_vault` — migrations
/// 0017/0018). Sans cette garde, tout tenant avec un api-key + grant peut
/// lire cross-tenant via `POST /api/v1/code_scope`.
#[must_use]
fn is_code_scope_privileged(trust: &TrustContext) -> bool {
    matches!(
        trust,
        TrustContext::Studio { .. } | TrustContext::Mtls { .. }
    ) || trust
        .subject()
        .map(|a| a.as_str() == "main-agent")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_id_validation_rejects_main_and_traversal() {
        assert!(!validate_code_vault_id("main"), "main refusé");
        assert!(!validate_code_vault_id(""), "vide refusé");
        assert!(!validate_code_vault_id("code-"), "préfixe seul refusé");
        assert!(!validate_code_vault_id("notcode-x"), "mauvais préfixe");
        assert!(!validate_code_vault_id("code-../main"), "traversal refusé");
        assert!(!validate_code_vault_id("code-/etc/passwd"), "slash refusé");
        assert!(!validate_code_vault_id("code-a.b"), "point refusé");
        assert!(!validate_code_vault_id("code-MAIN"), "majuscule refusée");
        assert!(!validate_code_vault_id("code-a b"), "espace refusé");
        assert!(validate_code_vault_id("code-gradatum"));
        assert!(validate_code_vault_id("code-my-project-2"));
    }

    #[test]
    fn token_estimate_conservative() {
        let e = CodeScopeEntryRaw {
            note_id: gradatum_core::identity::NoteId::new(),
            source_path: "src/a.rs".into(),
            kind: "fn".into(),
            qualified_name: "foo".into(),
            signature: Some("(x: u32) -> u32".into()),
            deps: vec!["bar".into()],
            bm25: -1.0,
            span: None,
        };
        let t = estimate_entry_tokens(&e);
        assert!(t >= 1, "au moins 1 token");
        assert!(t < 50, "estimation raisonnable, got {t}");
    }

    #[test]
    fn rank_promotes_cohesion() {
        let a = CodeScopeEntryRaw {
            note_id: gradatum_core::identity::NoteId::new(),
            source_path: "src/a.rs".into(),
            kind: "fn".into(),
            qualified_name: "alpha".into(),
            signature: None,
            deps: vec!["beta".into()],
            bm25: -1.0,
            span: None,
        };
        let b = CodeScopeEntryRaw {
            note_id: gradatum_core::identity::NoteId::new(),
            source_path: "src/b.rs".into(),
            kind: "fn".into(),
            qualified_name: "beta".into(),
            signature: None,
            deps: vec![],
            bm25: -1.0,
            span: None,
        };
        let ranked = rank_structure_aware(vec![b, a]);
        assert_eq!(ranked[0].qualified_name, "alpha", "cohésion promeut alpha");
    }

    // ── A1+A3 : build_entries — callers_truncated ─────────────────────────────

    fn make_raw(qualified_name: &str) -> CodeScopeEntryRaw {
        CodeScopeEntryRaw {
            note_id: gradatum_core::identity::NoteId::new(),
            source_path: "src/lib.rs".into(),
            kind: "fn".into(),
            qualified_name: qualified_name.to_string(),
            signature: None,
            deps: vec![],
            bm25: -1.0,
            span: None,
        }
    }

    #[test]
    fn build_entries_no_callers() {
        let raw = vec![make_raw("foo"), make_raw("bar")];
        let file_data: HashMap<String, (bool, Vec<u8>)> = HashMap::new();
        let stale_paths: HashSet<String> = HashSet::new();
        let callers_map: HashMap<String, Vec<String>> = HashMap::new();
        let (entries, body_trunc) =
            build_entries(raw, &file_data, &stale_paths, callers_map, false, 4000, 50);
        assert_eq!(entries.len(), 2);
        assert!(!body_trunc);
        for e in &entries {
            assert!(e.callers.is_empty(), "callers vide quand non demandé");
            assert!(!e.callers_truncated, "truncated=false quand vide");
        }
    }

    #[test]
    fn build_entries_callers_not_truncated() {
        let raw = vec![make_raw("target")];
        let file_data: HashMap<String, (bool, Vec<u8>)> = HashMap::new();
        let stale_paths: HashSet<String> = HashSet::new();
        let mut callers_map: HashMap<String, Vec<String>> = HashMap::new();
        callers_map.insert(
            "target".to_string(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        );
        let (entries, _) =
            build_entries(raw, &file_data, &stale_paths, callers_map, false, 4000, 50);
        assert_eq!(entries[0].callers.len(), 3);
        assert!(!entries[0].callers_truncated, "3 < 50 → non tronqué");
    }

    #[test]
    fn build_entries_callers_truncated_at_cap() {
        let cap = 5usize;
        let raw = vec![make_raw("popular")];
        let file_data: HashMap<String, (bool, Vec<u8>)> = HashMap::new();
        let stale_paths: HashSet<String> = HashSet::new();
        let mut callers_map: HashMap<String, Vec<String>> = HashMap::new();
        // Le batch retourne exactement `cap` callers (SQL LIMIT = cap).
        let callers: Vec<String> = (0..cap).map(|i| format!("caller_{i}")).collect();
        callers_map.insert("popular".to_string(), callers);
        let (entries, _) =
            build_entries(raw, &file_data, &stale_paths, callers_map, false, 4000, cap);
        assert_eq!(entries[0].callers.len(), cap);
        assert!(
            entries[0].callers_truncated,
            "len==cap → callers_truncated=true"
        );
    }
}
