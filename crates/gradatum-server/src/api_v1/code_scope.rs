//! `POST /api/v1/code_scope` — code-map read interface.
//!
//! Dedicated endpoint that intentionally bypasses the `vault_id ≠ main` 403 guard
//! used for cross-tenant isolation. It MUST therefore — security invariant #1 —
//! reject (400) any `vault` not starting with `code-`: otherwise it becomes a
//! read hole into `main` that cancels the single-vault mitigation. Validation
//! is performed HERE, before any index query ([`validate_code_vault_id`]).
//!
//! # Contract
//!
//! | Method | Path | Body | Response | Codes |
//! |--------|------|------|----------|-------|
//! | POST | `/api/v1/code_scope` | [`CodeScopeRequest`] | [`CodeScopeResponse`] | 200 / 400 / 401 / 404 / 500 |
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

use axum::{extract::State, http::StatusCode, Extension, Json};
use gradatum_core::index_store::{CodeScopeEntryRaw, CodeSelector};
use gradatum_core::trust::TrustContext;
use gradatum_dto::{
    is_valid_selector_kind, CodeScopeEntry, CodeScopeRequest, CodeScopeResponse,
    DEFAULT_BODY_BUDGET_TOKENS, DEFAULT_BUDGET_TOKENS,
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

/// `POST /api/v1/code_scope` — see module documentation.
///
/// # Errors
///
/// - `401 Unauthorized`: unauthenticated request.
/// - `400 Bad Request`: `vault` is not `code-*` or has an invalid charset (security
///   invariant #1), `selector.kind` is out of vocabulary, or `value` is empty.
/// - `404 Not Found`: valid `code-*` vault but never ingested (absent from the
///   `code_vault` table). Criterion: presence of an entry in `code_vault` (populated
///   by `run_ingest` via `set_code_vault_repo_path`). If derived notes exist without
///   a `code_vault` entry (out-of-CLI usage), the vault is treated as non-existent
///   — unsupported scenario (any CLI ingest calls both).
/// - `500 Internal Server Error`: storage failure on the index side.
///
/// No ACL 403 here — `code_scope` is a dedicated endpoint that bypasses the
/// single-vault guard. Security relies ENTIRELY on `code-` prefix validation (invariant #1).
/// Known vault + selector with no match → 200 `{entries:[], total_matched:0}`.
/// Distinction: vault-does-not-exist (404) vs vault-exists-but-no-match (200).
pub async fn code_scope(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<CodeScopeRequest>,
) -> Result<Json<CodeScopeResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // ── INVARIANT SÉCU N°1 (BLOQUANT) ─────────────────────────────────────────
    // code_scope contourne la garde 403 vault_id≠main de Slice 2a. Il DOIT donc
    // rejeter tout vault non-`code-` — sinon trou de lecture vers `main`.
    if !validate_code_vault_id(&req.vault) {
        tracing::warn!(
            vault = %req.vault,
            "code_scope: vault refusé (invariant sécu N°1 — non `code-` ou charset invalide)"
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
        _ => return Err(StatusCode::BAD_REQUEST), // déjà filtré, défense en profondeur
    };

    let budget = req
        .budget_tokens
        .unwrap_or(DEFAULT_BUDGET_TOKENS)
        .clamp(1, MAX_BUDGET_TOKENS) as usize;

    let body_budget = req
        .body_budget_tokens
        .unwrap_or(DEFAULT_BODY_BUDGET_TOKENS)
        .clamp(1, 32_000);

    // ── Existence du vault (§3.3bis M1 — contrat gelé 2026-06-13) ─────────────
    // Critère : présence dans la table `code_vault` (peuplée par `set_code_vault_repo_path`
    // à chaque `code ingest`). `None` → jamais ingéré → 404 (« vault inexistant »).
    // Distingué de « vault existant mais 0 match » → 200 vide (cf. fin du handler).
    // Note : réutilise la même requête SQL que detect_stale_paths → zéro coût additionnel
    // en lecture (la valeur sera ignorée ici, la vraie lecture servant la drift-detection).
    match state
        .search
        .get_code_vault_repo_path(&req.vault)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, vault = %req.vault, "code_scope: get_code_vault_repo_path failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })? {
        Some(_) => {} // vault connu → continuer
        None => {
            tracing::debug!(vault = %req.vault, "code_scope: vault inexistant → 404");
            return Err(StatusCode::NOT_FOUND);
        }
    }

    // ── Requête index (BM25-only, sans garde mono-vault) ───────────────────────
    let raw: Vec<CodeScopeEntryRaw> = state
        .search
        .code_scope_query(&req.vault, &selector, MAX_CANDIDATES)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, vault = %req.vault, "code_scope: code_scope_query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let total_matched = raw.len() as u32;

    // ── Ranking structure-aware (§3.3bis) ──────────────────────────────────────
    // BM25 d'abord (meilleur = plus proche de 0, donc tri ASC sur bm25), PUIS bonus
    // de cohésion : un symbole dont des deps sont AUSSI dans le résultat est promu.
    let ranked = rank_structure_aware(raw);

    // ── Budget : coupe à K entrées ENTIÈRES telles que Σ tokens ≤ budget ───────
    let mut kept: Vec<CodeScopeEntryRaw> = Vec::new();
    let mut used_tokens = 0usize;
    let mut truncated = false;
    for entry in ranked {
        let cost = estimate_entry_tokens(&entry);
        if used_tokens + cost > budget {
            // Entrée omise → réponse tronquée. On NE coupe PAS l'entrée (accuracy>coverage).
            truncated = true;
            // On continue à scanner : une entrée plus petite plus loin pourrait tenir.
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

    // Collecter les paths stale pour régen async.
    let stale_paths: HashSet<String> = file_data
        .iter()
        .filter_map(|(path, (stale, _))| if *stale { Some(path.clone()) } else { None })
        .collect();

    // Si des fichiers sont stale → enqueue régen async (R6, jamais synchrone bloquant).
    if !stale_paths.is_empty() {
        enqueue_regen(&req.vault, &stale_paths);
    }

    // ── Construction des entrées + corps (include_body) ────────────────────────────
    let mut body_tokens_used = 0usize;
    let mut body_truncated = false;

    let entries: Vec<CodeScopeEntry> = kept
        .into_iter()
        .map(|e| {
            let stale = stale_paths.contains(&e.source_path);
            // Corps : slice SEULEMENT si !stale CAR stale=false prouve byte-identité
            // fichier (hash fichier-entier A1) ⟹ lignes [start..=end] sont exactes (A2).
            let body = if req.include_body && !stale {
                extract_body_from_file_data(&file_data, &e).and_then(|(body_str, cost)| {
                    if body_tokens_used + cost <= body_budget {
                        body_tokens_used += cost;
                        Some(body_str)
                    } else {
                        // Budget corps dépassé → corps omis pour cette entrée.
                        body_truncated = true;
                        None
                    }
                })
            } else {
                None
            };
            CodeScopeEntry {
                note_id: e.note_id.0.to_string(),
                source_path: e.source_path,
                kind: e.kind,
                qualified_name: e.qualified_name,
                signature: e.signature.unwrap_or_default(),
                deps: e.deps,
                stale,
                body,
            }
        })
        .collect();

    Ok(Json(CodeScopeResponse {
        entries,
        truncated,
        total_matched,
        body_truncated,
    }))
}

/// Validates a `vault_id` for `code_scope` — security invariant #1.
///
/// All rules are cumulative (any violation → rejection):
/// 1. `code-` prefix is mandatory (never `main` or an arbitrary vault).
/// 2. Anti-traversal charset — `[a-z0-9-]` only after the prefix:
///    no `/`, `.`, `\`, space, or `..` (path traversal is forbidden).
/// 3. Length ≤ 128.
///
/// Returns `true` iff the vault is a valid and safe code vault.
#[must_use]
pub fn validate_code_vault_id(vault: &str) -> bool {
    if !vault.starts_with("code-") {
        return false;
    }
    if vault.len() > 128 {
        return false;
    }
    // Suffixe après "code-" non vide.
    let suffix = &vault["code-".len()..];
    if suffix.is_empty() {
        return false;
    }
    // Charset strict : minuscules, chiffres, tiret. Interdit slash/point/backslash/espace
    // → bloque le path traversal (`code-../main`, `code-/etc/passwd`, etc.).
    vault
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Sorts entries BM25 ASC (best match first), then applies a structural cohesion bonus —
/// a symbol whose dependencies also appear in the result set is promoted. The bonus is
/// a bounded score offset.
///
/// Implementation: computes the set of `qualified_name` values present, then sorts by
/// `(bm25 - cohesion_bonus)` ASC where `cohesion_bonus` = 0.5 × (number of deps present).
/// For `path`/`symbol` selectors (bm25=0), the bonus alone determines cohesion order.
fn rank_structure_aware(mut entries: Vec<CodeScopeEntryRaw>) -> Vec<CodeScopeEntryRaw> {
    let present: HashSet<&str> = entries.iter().map(|e| e.qualified_name.as_str()).collect();
    // Pré-calcul du bonus par index (clonage des deps évité — on compte sur place).
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
    // Score effectif = bm25 - bonus (plus petit = mieux, BM25 négatif). Tri stable.
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    idx.sort_by(|&a, &b| {
        let sa = entries[a].bm25 - bonuses[a];
        let sb = entries[b].bm25 - bonuses[b];
        sa.partial_cmp(&sb)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Départage déterministe par qualified_name (rebuild stable).
            .then_with(|| entries[a].qualified_name.cmp(&entries[b].qualified_name))
    });
    let mut out = Vec::with_capacity(entries.len());
    // Réordonner sans cloner : drain via take.
    let mut taken: Vec<Option<CodeScopeEntryRaw>> = entries.drain(..).map(Some).collect();
    for i in idx {
        if let Some(e) = taken[i].take() {
            out.push(e);
        }
    }
    out
}

/// Estimates the token cost of an entry (heuristic `chars / 4`, see [`CHARS_PER_TOKEN`]).
///
/// Accounts for the fields serialised to the client: `source_path` + `kind` +
/// `qualified_name` + `signature` + `deps`. Conservative (slight over-count) —
/// guarantees Σ tokens ≤ budget.
fn estimate_entry_tokens(e: &CodeScopeEntryRaw) -> usize {
    let mut chars = e.source_path.len() + e.kind.len() + e.qualified_name.len();
    if let Some(sig) = &e.signature {
        chars += sig.len();
    }
    chars += e.deps.iter().map(|d| d.len()).sum::<usize>();
    // Overhead JSON par entrée (clés, accolades) — forfait conservateur.
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

    // Repo path pour localiser les fichiers sur disque.
    let repo_path = match state.search.get_code_vault_repo_path(vault).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::debug!(vault = %vault, "code_scope: repo path inconnu → drift SKIP");
            return result;
        }
        Err(e) => {
            tracing::warn!(err = %e, vault = %vault, "code_scope: get_code_vault_repo_path KO → drift SKIP");
            return result;
        }
    };

    // ── S1 (BLOQUANT) : canonicaliser le repo_abs pour la validation anti-traversal ──
    let repo_abs = Path::new(&repo_path);
    let repo_canonical = match repo_abs.canonicalize() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(err = %e, repo = %repo_path, "code_scope: canonicalize repo KO → drift SKIP");
            return result;
        }
    };

    // Fichiers distincts du résultat.
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
            tracing::warn!(path = %path, "code_scope: path rejeté anti-traversal (S1)");
            result.insert(path.clone(), (true, Vec::new()));
            continue;
        }
        let candidate = repo_abs.join(path);
        let canonical = match candidate.canonicalize() {
            Ok(c) => c,
            Err(_) => {
                // Fichier inexistant ou lien brisé → stale (jamais d'exfiltration silencieuse).
                result.insert(path.clone(), (true, Vec::new()));
                continue;
            }
        };
        if !canonical.starts_with(&repo_canonical) {
            tracing::warn!(
                path = %path,
                canonical = %canonical.display(),
                repo = %repo_canonical.display(),
                "code_scope: path hors repo rejeté (S1)"
            );
            result.insert(path.clone(), (true, Vec::new()));
            continue;
        }

        // ── Lecture unique : hash ET bytes en une seule passe (A1) ──────────────────
        let Some(stored_hash) = stored.get(path) else {
            // Pas de hash stocké → incertitude → stale (accuracy>coverage).
            result.insert(path.clone(), (true, Vec::new()));
            continue;
        };
        match std::fs::read(&canonical) {
            Ok(bytes) => {
                let current = sha256_hex(&bytes);
                let is_stale = &current != stored_hash;
                // bytes conservés pour include_body (slice SEULEMENT si !is_stale — A2).
                result.insert(path.clone(), (is_stale, bytes));
            }
            Err(_) => {
                // Fichier disparu / illisible → stale.
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
    // B3 : span dégénéré.
    if start_line == 0 || start_line > end_line {
        return None;
    }

    let content = std::str::from_utf8(bytes).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let nb_lines = lines.len() as u32;

    // B3 : start hors fichier.
    if start_line > nb_lines {
        return None;
    }

    // 1-based → 0-based index. end_line clampé au nb de lignes.
    let start_idx = (start_line - 1) as usize;
    let end_idx = (end_line.min(nb_lines) - 1) as usize;

    // Slice SEULEMENT si !stale CAR stale=false prouve byte-identité (A2).
    let body_lines = &lines[start_idx..=end_idx];
    let body_str = body_lines.join("\n");

    // Estimation coût tokens (même heuristique que les signatures).
    let cost = body_str.len().div_ceil(CHARS_PER_TOKEN).max(1);

    Some((body_str, cost))
}

/// Computes the hex SHA-256 digest for drift detection. Matches `gradatum_ingest::content_hash_source`.
fn sha256_hex(bytes: &[u8]) -> String {
    let hash: [u8; 32] = Sha256::digest(bytes).into();
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Signals that stale files need asynchronous re-generation (never a blocking synchronous re-parse).
///
/// In the current version, async re-generation is logged (structured log) but the
/// actual job-system enqueue is deferred: the worker has no `CodeReingest` handler yet
/// and `code update` is driven off-line by CLI/git hook. The `stale` flag in the response
/// is sufficient for the contract: the caller knows the entry is stale and MUST NOT treat
/// it as ground truth. The operator re-runs `gradatum-admin code update`.
/// Documented as tracked debt.
fn enqueue_regen(vault: &str, paths: &HashSet<String>) {
    tracing::info!(
        vault = %vault,
        stale_count = paths.len(),
        stale_paths = ?paths,
        "code_scope: fichiers stale détectés — régen requise (relancer `code update`)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_id_validation_rejects_main_and_traversal() {
        // Invariant sécu N°1 : tout ce qui n'est pas un vault code- sûr est refusé.
        assert!(!validate_code_vault_id("main"), "main refusé");
        assert!(!validate_code_vault_id(""), "vide refusé");
        assert!(!validate_code_vault_id("code-"), "préfixe seul refusé");
        assert!(!validate_code_vault_id("notcode-x"), "mauvais préfixe");
        assert!(!validate_code_vault_id("code-../main"), "traversal refusé");
        assert!(!validate_code_vault_id("code-/etc/passwd"), "slash refusé");
        assert!(!validate_code_vault_id("code-a.b"), "point refusé");
        assert!(!validate_code_vault_id("code-MAIN"), "majuscule refusée");
        assert!(!validate_code_vault_id("code-a b"), "espace refusé");
        // Valides.
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
        // ~ (8+2+3+15+3+40)/4 ≈ 18 tokens.
        assert!(t < 50, "estimation raisonnable, got {t}");
    }

    #[test]
    fn rank_promotes_cohesion() {
        // 2 entrées, bm25 égal ; A dépend de B (présent) → A doit passer devant B.
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
}
