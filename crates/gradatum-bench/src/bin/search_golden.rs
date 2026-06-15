//! Golden-set SEARCH — gate d'activation F-17 (trust decay) v0.4.4.
//!
//! ## Rôle
//!
//! F-17 introduit un multiplicateur `trust_decay` dans le `composite_score` de la
//! recherche (couche RRF). Ce changement touche **tous** les scores de recherche
//! (risque R1 du plan). Ce bin établit une **baseline** mesurable AVANT activation,
//! puis permet de re-mesurer APRÈS pour vérifier la non-régression :
//!
//! - `recall@5` ≥ baseline − 0.05
//! - stabilité top-1 ≥ 80 %
//!
//! ## Méthode d'exécution : API LIVE (reproductible)
//!
//! Le bin interroge le serveur gradatum LIVE (`POST /api/v1/vault_search`) plutôt que
//! la lib search in-process. Justification :
//! - L'API LIVE est exactement la surface affectée par F-17 (composite_score réel servi).
//! - Une exécution in-process exigerait une copie de l'index SQLite (vault personnel)
//!   — non reproductible en CI et fuite de données (C2). L'API LIVE lit l'index déployé
//!   sans le matérialiser dans le repo.
//! - La recherche est déterministe à index figé (BM25 + RRF), donc reproductible.
//!
//! ## Confidentialité (C2 ABSOLUE)
//!
//! Le dataset de requêtes ET le rapport baseline contiennent des données du vault
//! personnel (ULIDs, titres). Ils vivent dans `crates/gradatum-bench/datasets/*.json`
//! (gitignore `*.json` + `*.jsonl`) et ne sont JAMAIS commités. Seul ce CODE l'est.
//!
//! ## Format dataset (`SEARCH_GOLDEN_DATASET`, défaut datasets/search-golden-queries-v0.4.4.json)
//!
//! ```json
//! {
//!   "version": "v0.4.4-pre-F17",
//!   "queries": [
//!     { "id": "q01", "query": "council Art.19 gouvernance", "section": null,
//!       "expected_top5": ["council/01ABC...", "decisions/01DEF..."] }
//!   ]
//! }
//! ```
//!
//! `expected_top5` = ordre top-5 observé à l'état v0.4.4 pré-F-17 (capturé via ce même
//! endpoint). Le `path` (`section/ULID`) est l'identifiant stable comparé.
//!
//! ## Usage
//!
//! ```bash
//! # Mesure baseline (écrit le rapport JSON local) :
//! GRADATUM_API_KEY=$(sudo cat /etc/gradatum/claude-code.api-key) \
//!   cargo run -p gradatum-bench --bin search_golden
//!
//! # Variables :
//! #   SEARCH_GOLDEN_URL       (défaut http://localhost:19090)
//! #   SEARCH_GOLDEN_DATASET   (défaut crates/gradatum-bench/datasets/search-golden-queries-v0.4.4.json)
//! #   SEARCH_GOLDEN_REPORT    (défaut crates/gradatum-bench/datasets/search-golden-baseline-v0.4.4-pre-F17.json)
//! #   SEARCH_GOLDEN_BASELINE  (optionnel : chemin d'un rapport antérieur → compare top-1 stabilité)
//! #   GRADATUM_API_KEY        (api-key brute) OU GRADATUM_JWT (JWT déjà échangé)
//! ```
//!
//! ## Mode CI
//!
//! `GRADATUM_CI=1` : si le dataset est absent (gitignored, perso) → WARN + exit 0
//! (skip propre, le code compile et est vérifié sans exiger le vault personnel).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ─── Types dataset ───────────────────────────────────────────────────────────

/// Dataset de requêtes golden-set (fichier JSON local, jamais commité).
#[derive(Debug, Deserialize)]
struct GoldenDataset {
    /// Version du dataset (ex. `"v0.4.4-pre-F17"`).
    #[serde(default)]
    version: String,
    /// Requêtes à évaluer.
    queries: Vec<GoldenQuery>,
}

/// Une requête golden avec son top-5 attendu (état pré-F-17).
#[derive(Debug, Deserialize)]
struct GoldenQuery {
    /// Identifiant court de la requête (ex. `"q01"`).
    id: String,
    /// Texte de la requête (FR).
    query: String,
    /// Section optionnelle (filtre — peut être `null`).
    #[serde(default)]
    section: Option<String>,
    /// Top-5 attendu : liste ordonnée de `path` (`section/ULID`).
    expected_top5: Vec<String>,
}

// ─── Types API ───────────────────────────────────────────────────────────────

/// Corps de la requête `POST /api/v1/vault_search`.
#[derive(Debug, Serialize)]
struct SearchRequest<'a> {
    query: &'a str,
    limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    section: Option<&'a str>,
}

/// Réponse `vault_search` (sous-ensemble des champs utilisés).
#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    items: Vec<SearchItem>,
}

/// Un hit de recherche.
#[derive(Debug, Deserialize)]
struct SearchItem {
    /// `section/ULID` — identifiant stable comparé aux expected.
    path: String,
}

/// Corps de la requête d'échange api-key → JWT.
#[derive(Debug, Serialize)]
struct ExchangeBody {}

/// Réponse de `POST /auth/exchange`.
#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    token: String,
}

// ─── Types rapport ───────────────────────────────────────────────────────────

/// Résultat par requête.
#[derive(Debug, Serialize)]
struct QueryResult {
    id: String,
    query: String,
    /// Top-5 effectivement retourné (paths).
    actual_top5: Vec<String>,
    /// recall@5 : |expected ∩ actual_top5| / |expected| (borné à 5).
    recall_at_5: f64,
    /// MRR : 1/rang du premier expected trouvé dans actual (0 si absent).
    reciprocal_rank: f64,
    /// top-1 identique entre expected\[0] et actual\[0].
    top1_stable: bool,
}

/// Rapport agrégé du golden-set.
#[derive(Debug, Serialize)]
struct GoldenReport {
    dataset_version: String,
    query_count: usize,
    /// recall@5 moyen sur toutes les requêtes.
    mean_recall_at_5: f64,
    /// MRR moyen.
    mean_reciprocal_rank: f64,
    /// Taux de stabilité top-1 (proportion de requêtes dont actual\[0] == expected\[0]).
    top1_stability_rate: f64,
    /// Détail par requête.
    per_query: Vec<QueryResult>,
}

// ─── Métriques ───────────────────────────────────────────────────────────────

/// Calcule recall@5, MRR et stabilité top-1 d'une requête.
///
/// - `expected` : top-5 de référence (ordre pré-F-17).
/// - `actual` : top-5 observé.
///
/// recall@5 = fraction des `expected` (max 5) présents dans `actual`.
/// MRR = 1/rang (1-indexé) du premier `expected` trouvé dans `actual`, 0 si aucun.
/// top1_stable = `expected\[0] == actual\[0]`.
fn compute_metrics(expected: &[String], actual: &[String]) -> (f64, f64, bool) {
    let expected_set: std::collections::BTreeSet<&String> = expected.iter().take(5).collect();
    let denom = expected_set.len().max(1) as f64;

    // recall@5 : combien d'expected sont dans actual (top-5).
    let hits = actual
        .iter()
        .take(5)
        .filter(|p| expected_set.contains(p))
        .count();
    let recall = hits as f64 / denom;

    // MRR : rang du premier expected dans actual.
    let mut rr = 0.0;
    for (rank, path) in actual.iter().enumerate() {
        if expected_set.contains(path) {
            rr = 1.0 / (rank as f64 + 1.0);
            break;
        }
    }

    let top1_stable = match (expected.first(), actual.first()) {
        (Some(e), Some(a)) => e == a,
        _ => false,
    };

    (recall, rr, top1_stable)
}

// ─── Client API ──────────────────────────────────────────────────────────────

/// Résout le JWT : `GRADATUM_JWT` direct, sinon échange `GRADATUM_API_KEY`.
async fn resolve_jwt(client: &reqwest::Client, base_url: &str) -> Result<String> {
    if let Ok(jwt) = std::env::var("GRADATUM_JWT") {
        if !jwt.trim().is_empty() {
            return Ok(jwt);
        }
    }
    let api_key = std::env::var("GRADATUM_API_KEY")
        .context("ni GRADATUM_JWT ni GRADATUM_API_KEY fournis — auth impossible")?;

    let resp = client
        .post(format!("{base_url}/auth/exchange"))
        .bearer_auth(api_key)
        .json(&ExchangeBody {})
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("échange api-key → JWT échoué (serveur LIVE injoignable ?)")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("auth/exchange a retourné {status} : {body}");
    }
    let parsed: ExchangeResponse =
        serde_json::from_str(&body).context("réponse auth/exchange non parsable")?;
    Ok(parsed.token)
}

/// Exécute une requête `vault_search` et retourne les paths du top-N.
async fn run_search(
    client: &reqwest::Client,
    base_url: &str,
    jwt: &str,
    q: &GoldenQuery,
    limit: u32,
) -> Result<Vec<String>> {
    let req = SearchRequest {
        query: &q.query,
        limit,
        section: q.section.as_deref(),
    };
    let resp = client
        .post(format!("{base_url}/api/v1/vault_search"))
        .bearer_auth(jwt)
        .json(&req)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .with_context(|| format!("vault_search échoué pour {} ('{}')", q.id, q.query))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("vault_search {} a retourné {status} : {body}", q.id);
    }
    let parsed: SearchResponse =
        serde_json::from_str(&body).context("réponse vault_search non parsable")?;
    Ok(parsed.items.into_iter().map(|i| i.path).collect())
}

// ─── Main ────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let base_url =
        std::env::var("SEARCH_GOLDEN_URL").unwrap_or_else(|_| "http://localhost:19090".to_string());
    let dataset_path: PathBuf = std::env::var("SEARCH_GOLDEN_DATASET")
        .unwrap_or_else(|_| {
            "crates/gradatum-bench/datasets/search-golden-queries-v0.4.4.json".to_string()
        })
        .into();
    let report_path: PathBuf = std::env::var("SEARCH_GOLDEN_REPORT")
        .unwrap_or_else(|_| {
            "crates/gradatum-bench/datasets/search-golden-baseline-v0.4.4-pre-F17.json".to_string()
        })
        .into();
    let ci_mode = std::env::var("GRADATUM_CI")
        .map(|v| v == "1")
        .unwrap_or(false);

    // Dataset absent : skip propre en CI (gitignored, perso), erreur sinon.
    if !dataset_path.exists() {
        if ci_mode {
            eprintln!(
                "WARN: dataset {} absent (gitignored perso) — skip propre CI (exit 0).",
                dataset_path.display()
            );
            return Ok(());
        }
        anyhow::bail!(
            "dataset {} introuvable. Générer le dataset queries d'abord \
             (voir l'en-tête du bin pour le format).",
            dataset_path.display()
        );
    }

    let raw = std::fs::read_to_string(&dataset_path)
        .with_context(|| format!("lecture dataset {}", dataset_path.display()))?;
    let dataset: GoldenDataset =
        serde_json::from_str(&raw).context("dataset golden-set non parsable")?;

    if dataset.queries.is_empty() {
        anyhow::bail!("dataset golden-set vide — aucune requête à évaluer.");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("construction client HTTP")?;

    let jwt = resolve_jwt(&client, &base_url).await?;

    let mut per_query = Vec::with_capacity(dataset.queries.len());
    let mut sum_recall = 0.0;
    let mut sum_rr = 0.0;
    let mut top1_stable_count = 0usize;

    for q in &dataset.queries {
        let actual = run_search(&client, &base_url, &jwt, q, 5).await?;
        let (recall, rr, top1_stable) = compute_metrics(&q.expected_top5, &actual);
        sum_recall += recall;
        sum_rr += rr;
        if top1_stable {
            top1_stable_count += 1;
        }
        per_query.push(QueryResult {
            id: q.id.clone(),
            query: q.query.clone(),
            actual_top5: actual,
            recall_at_5: recall,
            reciprocal_rank: rr,
            top1_stable,
        });
    }

    let n = dataset.queries.len() as f64;
    let report = GoldenReport {
        dataset_version: dataset.version.clone(),
        query_count: dataset.queries.len(),
        mean_recall_at_5: sum_recall / n,
        mean_reciprocal_rank: sum_rr / n,
        top1_stability_rate: top1_stable_count as f64 / n,
        per_query,
    };

    // Comparaison optionnelle à une baseline antérieure (post-F-17 vs pré-F-17).
    if let Ok(baseline_path) = std::env::var("SEARCH_GOLDEN_BASELINE") {
        if let Ok(prev_raw) = std::fs::read_to_string(&baseline_path) {
            if let Ok(prev) = serde_json::from_str::<serde_json::Value>(&prev_raw) {
                let prev_recall = prev
                    .get("mean_recall_at_5")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let delta = report.mean_recall_at_5 - prev_recall;
                println!(
                    "Comparaison baseline {} : recall@5 {prev_recall:.4} → {:.4} (Δ {delta:+.4})",
                    baseline_path, report.mean_recall_at_5
                );
                // Le gate (Δ ≥ -0.05 ET top1 ≥ 0.80) est appliqué par le Tester/CI — ici on informe.
            }
        }
    }

    let report_json =
        serde_json::to_string_pretty(&report).context("sérialisation rapport JSON")?;
    std::fs::write(&report_path, &report_json)
        .with_context(|| format!("écriture rapport {}", report_path.display()))?;

    // Résumé lisible sur stdout (ne contient pas de path par requête — métriques seules).
    let mut by_section: BTreeMap<&str, u32> = BTreeMap::new();
    for q in &dataset.queries {
        let sect = q.section.as_deref().unwrap_or("(aucune)");
        *by_section.entry(sect).or_insert(0) += 1;
    }
    println!("── Golden-set SEARCH ({}) ──", report.dataset_version);
    println!("  requêtes        : {}", report.query_count);
    println!("  recall@5 moyen  : {:.4}", report.mean_recall_at_5);
    println!("  MRR moyen       : {:.4}", report.mean_reciprocal_rank);
    println!(
        "  top-1 stabilité : {:.1}%",
        report.top1_stability_rate * 100.0
    );
    println!("  rapport écrit   : {}", report_path.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn recall_perfect_match() {
        let expected = s(&["a", "b", "c", "d", "e"]);
        let actual = s(&["a", "b", "c", "d", "e"]);
        let (recall, rr, top1) = compute_metrics(&expected, &actual);
        assert_eq!(recall, 1.0, "recall@5 parfait");
        assert_eq!(rr, 1.0, "MRR = 1 (premier expected en rang 1)");
        assert!(top1, "top-1 stable");
    }

    #[test]
    fn recall_partial_and_reordered() {
        let expected = s(&["a", "b", "c", "d", "e"]);
        // 3/5 présents, top-1 différent, premier expected ('a') en rang 2.
        let actual = s(&["x", "a", "b", "c", "z"]);
        let (recall, rr, top1) = compute_metrics(&expected, &actual);
        assert!((recall - 0.6).abs() < 1e-9, "recall@5 = 3/5 = 0.6");
        assert!((rr - 0.5).abs() < 1e-9, "MRR = 1/2 (a en rang 2)");
        assert!(!top1, "top-1 instable (x != a)");
    }

    #[test]
    fn recall_zero_when_disjoint() {
        let expected = s(&["a", "b"]);
        let actual = s(&["x", "y", "z"]);
        let (recall, rr, top1) = compute_metrics(&expected, &actual);
        assert_eq!(recall, 0.0);
        assert_eq!(rr, 0.0);
        assert!(!top1);
    }

    #[test]
    fn empty_actual_is_safe() {
        let expected = s(&["a"]);
        let actual: Vec<String> = vec![];
        let (recall, rr, top1) = compute_metrics(&expected, &actual);
        assert_eq!(recall, 0.0);
        assert_eq!(rr, 0.0);
        assert!(!top1, "actual vide → top-1 instable");
    }

    #[test]
    fn recall_caps_expected_at_five() {
        // expected de 7 → dénominateur borné à 5.
        let expected = s(&["a", "b", "c", "d", "e", "f", "g"]);
        let actual = s(&["a", "b", "c", "d", "e"]);
        let (recall, _, _) = compute_metrics(&expected, &actual);
        assert!((recall - 1.0).abs() < 1e-9, "5/5 expected (top-5) trouvés");
    }
}
