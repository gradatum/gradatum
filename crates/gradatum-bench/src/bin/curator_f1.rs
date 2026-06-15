//! Bench curator F1 — T11 P2.0b (post-bug-fix: routing regex + Qwen body preprocessing)
//! Extension A2 v0.4.3 : golden-set 41 cas + métriques hint_respect + council→reference.
//!
//! Mesure la qualité de classification de section sur deux datasets :
//!
//! ## Dataset 1 — gradatum-balanced-v1-with-body (147 notes, 10 sections)
//! - `heuristic` : `heuristic_route(title, body_preview)` offline, pas de réseau
//! - `llm` : HTTP POST `/v1/chat/completions` sur `LLM_ENDPOINT` (défaut: `http://localhost:8082`)
//!   - `LLM_ENDPOINT` : URL de base (ex: `http://127.0.0.1:19999`)
//!   - `LLM_MODEL` : nom du modèle dans le payload (ex: `Qwen3-4B-Instruct-2507`)
//!   - Rétrocompat : `QWEN_ENDPOINT` toujours accepté si `LLM_ENDPOINT` absent
//!
//! ## Dataset 2 — golden-set-curator (≥50 cas, 11 sections, A2 v0.4.3 / D2.4 v0.4.8)
//! Format étendu : colonnes `expected_section` + `section_hint` (optionnelle).
//! Dataset local-only, non distribué : contient des données du mainteneur,
//! jamais commité (couvert par .gitignore `crates/gradatum-bench/datasets/*.jsonl`).
//!
//! Path par défaut : `golden-set-curator-v1.jsonl` (41 cas perso). Override via
//! `CURATOR_GOLDEN_PATH` pour pointer vers un golden-set étendu ≥50 cas
//! 100% synthétique généré par `scripts/gen_golden_curator_synthetic.sh` (D2.4) —
//! committable conceptuellement mais reste gitignored par la règle datasets/*.jsonl ;
//! on commit donc le **générateur**, pas le `.jsonl`.
//! Métriques supplémentaires :
//! - `hint_respect_rate` : taux de respect du hint quand fourni (hint fort post-A1 → ~100%)
//! - `council_to_reference_rate` : taux de mauvais routage council→reference (B3 root-cause)
//!
//! ## Fix bug Z.2d
//! Le dataset `gradatum-balanced-v1-final.jsonl` ne contenait que titre/path/section
//! (body_preview perdu lors de l'étape de classification Z.2). Ce run utilise
//! `gradatum-balanced-v1-with-body.jsonl` (merger des deux sources par path).
//!
//! ## Fix Bug 1 — routing.rs regex `\b SECTION \b`
//! `\b` ne matche pas entre `[` et `D` (deux non-word chars).
//! Fix : pattern alterné `(?:\[DECISIONS?\]|\bdecisions?\b|\bkeyword\b)` pour chaque section.
//! Impacte les 10 sections canoniques, rétrocompatible avec les notes sans préfixe.
//!
//! ## Fix Bug 2 — Qwen body Markdown preprocessing
//! Le body Markdown brut polluait Qwen3-0.6B (-0.198 F1 vs titre-seul).
//! Fix : `clean_body_for_llm()` strip headings/wikilinks/code/frontmatter avant envoi LLM.
//! L'heuristique reçoit toujours le body brut (elle tolère le Markdown).
//!
//! ## Métriques produites
//! - accuracy, f1_micro, f1_weighted, f1_macro
//! - per_section : precision / recall / f1 / support
//! - matrice de confusion (paires true_section / predicted_section)
//! - (golden uniquement) hint_respect_rate, council_to_reference_rate
//!
//! ## Outputs
//! - `target/bench/curator-f1-gradatum-natif-fix-bugs-{date}.json`     — résultats JSON dataset 1
//! - `target/bench/gap-analysis-fix-bugs-{date}.jsonl`                 — tous les gaps
//! - `target/bench/gap-sample-review-{date}.md`                         — top 15 gaps lisibles
//! - `target/bench/curator-golden-baseline-{date}.json`                — baseline golden-set A2
//!
//! ## Exit P2.0b assertions
//! - heuristic F1 weighted ≥ 0.65
//! - LLM F1 weighted ≥ 0.75 (ou fallback heuristic si endpoint KO)
//!
//! ## CI assertion
//! `GRADATUM_CI=1` active le mode assertion strict :
//! - Si le dataset est absent (gitignored perso) → WARN + exit 0 (skip propre CI)
//! - heuristic F1 weighted ≥ 0.78
//! - LLM F1 weighted ≥ 0.78 si endpoint opérationnel

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use once_cell::sync::Lazy;
use regex::Regex;

use gradatum_curator::routing::heuristic_route;

// ─── Types dataset ───────────────────────────────────────────────────────────

/// Entrée du dataset gradatum-balanced-v1-with-body.jsonl (fix Z.2d).
///
/// Format legacy : 7 champs obligatoires, body_preview est la source de vérité.
#[derive(Deserialize, Debug, Clone)]
struct DatasetNote {
    path: String,
    title: String,
    /// Section legacy — dans les datasets produits par le classifieur Z.2, ce champ
    /// s'appelle `vault_mem_section`. L'alias `legacy_section_hint` couvre une variante
    /// ancienne. Non utilisé dans les métriques, conservé pour traçabilité.
    #[serde(alias = "legacy_section_hint", alias = "vault_mem_section")]
    legacy_section: String,
    /// Body (500 chars max) — présent depuis le fix Z.2d (merger avec source JSONL).
    body_preview: String,
    expected_section_gradatum: String,
    /// Niveau de confiance du classifieur Z.2 — conservé pour traçabilité dataset.
    #[allow(dead_code)]
    confidence: String,
    /// Raison de classification Z.2 — conservée pour traçabilité dataset.
    #[allow(dead_code)]
    classification_reason: String,
}

/// Entrée du golden-set A2 v0.4.3 — format étendu, rétrocompatible.
///
/// Colonnes obligatoires : `path`, `title`, `body_preview`, `expected_section`.
/// Colonnes optionnelles : `section_hint` (signal curator fort post-A1).
///
/// Permet de mesurer :
/// - concordance globale (expected vs assigné par l'heuristique)
/// - taux de respect du hint quand fourni (`section_hint` ≠ None)
/// - taux de mauvais routage `council` → `reference` (B3 root-cause)
#[derive(Deserialize, Debug, Clone)]
struct GoldenNote {
    /// Chemin relatif de la note dans le vault (pour traçabilité, pas d'IP ni nom).
    path: String,
    /// Titre de la note (signal principal pour l'heuristique préfixe).
    title: String,
    /// Corps tronqué à ~500 chars (signal sémantique).
    body_preview: String,
    /// Section attendue parmi les 11 sections canoniques.
    expected_section: String,
    /// Hint fourni par le créateur (optionnel — None si absent dans le JSONL).
    ///
    /// Post-A1 : si valide parmi les 11 sections canoniques, le curator admet
    /// directement sans heuristique ni LLM. On s'attend donc à hint_respect ≈ 1.0
    /// sur les cas avec hint valide.
    #[serde(default)]
    section_hint: Option<String>,
}

/// Métriques supplémentaires spécifiques au golden-set.
#[derive(Serialize, Debug, Clone)]
struct GoldenMetrics {
    /// Nombre de notes avec section_hint fourni.
    n_with_hint: usize,
    /// Nombre de fois où la prédiction correspond au hint (parmi les notes avec hint).
    hint_matched: usize,
    /// Taux de respect du hint : hint_matched / n_with_hint (NaN→0.0 si n_with_hint=0).
    hint_respect_rate: f32,
    /// Nombre de notes attendues en `council` dans le dataset.
    n_council: usize,
    /// Nombre de notes `council` incorrectement routées vers `reference`.
    council_to_reference: usize,
    /// Taux council→reference : council_to_reference / n_council (NaN→0.0 si n_council=0).
    council_to_reference_rate: f32,
}

/// ─── Types résultats ─────────────────────────────────────────────────────────

#[derive(Serialize, Debug, Clone, Default)]
struct SectionMetrics {
    precision: f32,
    recall: f32,
    f1: f32,
    support: usize,
}

#[derive(Serialize, Debug, Clone)]
struct BenchResult {
    backend: String,
    n: usize,
    accuracy: f32,
    f1_micro: f32,
    f1_weighted: f32,
    f1_macro: f32,
    per_section: HashMap<String, SectionMetrics>,
    /// (true_section, predicted_section) → count
    confusion: HashMap<String, usize>,
    /// Durée totale de classification en secondes
    elapsed_sec: f64,
}

/// Gap : note où la prédiction diffère du label attendu
#[derive(Serialize, Debug, Clone)]
struct Gap {
    path: String,
    title: String,
    legacy_section: String,
    expected_section_gradatum: String,
    predicted_section: String,
    body_preview: String,
}

/// ─── Utilitaires ─────────────────────────────────────────────────────────────
/// Sections canoniques dans l'ordre stable du routing.rs
const SECTIONS: &[&str] = &[
    "decisions",
    "architecture",
    "debug",
    "reasoning",
    "feedback",
    "lessons-learned",
    "retrospectives",
    "experiments",
    "agent-issues",
    "reference",
];

/// Calcule precision, recall, F1 et les agrégats micro/weighted/macro.
///
/// `predictions[i]` = section prédite, `labels[i]` = section vraie.
/// `sections` : liste des sections canoniques à suivre (10 pour le dataset legacy,
/// 11 pour le golden-set incluant `council`).
fn compute_metrics(
    labels: &[String],
    predictions: &[String],
    sections: &[&str],
) -> (f32, f32, f32, f32, HashMap<String, SectionMetrics>) {
    let n = labels.len();
    if n == 0 {
        return (0.0, 0.0, 0.0, 0.0, HashMap::new());
    }

    // Compteurs par section : TP, FP, FN
    let mut tp: HashMap<String, usize> = HashMap::new();
    let mut fp: HashMap<String, usize> = HashMap::new();
    let mut fn_: HashMap<String, usize> = HashMap::new();
    let mut support: HashMap<String, usize> = HashMap::new();

    for section in sections {
        tp.insert(section.to_string(), 0);
        fp.insert(section.to_string(), 0);
        fn_.insert(section.to_string(), 0);
        support.insert(section.to_string(), 0);
    }

    let mut correct = 0usize;
    for (label, pred) in labels.iter().zip(predictions.iter()) {
        *support.entry(label.clone()).or_insert(0) += 1;
        if pred == label {
            *tp.entry(label.clone()).or_insert(0) += 1;
            correct += 1;
        } else {
            *fn_.entry(label.clone()).or_insert(0) += 1;
            *fp.entry(pred.clone()).or_insert(0) += 1;
        }
    }

    let accuracy = correct as f32 / n as f32;

    // Métriques par section
    let mut per_section: HashMap<String, SectionMetrics> = HashMap::new();
    for section in sections {
        let sec = section.to_string();
        let tp_s = *tp.get(&sec).unwrap_or(&0) as f32;
        let fp_s = *fp.get(&sec).unwrap_or(&0) as f32;
        let fn_s = *fn_.get(&sec).unwrap_or(&0) as f32;
        let sup_s = *support.get(&sec).unwrap_or(&0);

        let prec = if tp_s + fp_s > 0.0 {
            tp_s / (tp_s + fp_s)
        } else {
            0.0
        };
        let rec = if tp_s + fn_s > 0.0 {
            tp_s / (tp_s + fn_s)
        } else {
            0.0
        };
        let f1 = if prec + rec > 0.0 {
            2.0 * prec * rec / (prec + rec)
        } else {
            0.0
        };

        per_section.insert(
            sec,
            SectionMetrics {
                precision: prec,
                recall: rec,
                f1,
                support: sup_s,
            },
        );
    }

    // F1 micro : TP total / (TP total + 0.5 * (FP total + FN total))
    let total_tp: f32 = tp.values().map(|v| *v as f32).sum();
    let total_fp: f32 = fp.values().map(|v| *v as f32).sum();
    let total_fn: f32 = fn_.values().map(|v| *v as f32).sum();
    let f1_micro = if total_tp + 0.5 * (total_fp + total_fn) > 0.0 {
        total_tp / (total_tp + 0.5 * (total_fp + total_fn))
    } else {
        0.0
    };

    // F1 macro : moyenne simple sur les sections avec support > 0
    let active_sections: Vec<_> = sections
        .iter()
        .filter(|s| *support.get(s.to_string().as_str()).unwrap_or(&0) > 0)
        .collect();
    let f1_macro = if active_sections.is_empty() {
        0.0
    } else {
        active_sections
            .iter()
            .map(|s| {
                per_section
                    .get(s.to_string().as_str())
                    .map_or(0.0, |m| m.f1)
            })
            .sum::<f32>()
            / active_sections.len() as f32
    };

    // F1 weighted : moyenne pondérée par support
    let f1_weighted = sections
        .iter()
        .map(|s| {
            let sup = *support.get(s.to_string().as_str()).unwrap_or(&0) as f32;
            let f1 = per_section
                .get(s.to_string().as_str())
                .map_or(0.0, |m| m.f1);
            sup * f1
        })
        .sum::<f32>()
        / n as f32;

    (accuracy, f1_micro, f1_weighted, f1_macro, per_section)
}

/// ─── Backend heuristic ───────────────────────────────────────────────────────
/// Classifie une note par heuristique regex sur titre + body_preview.
///
/// Fix Z.2d : le dataset contient désormais le body_preview (500 chars max).
/// L'heuristique `heuristic_route(title, body_preview)` est utilisée avec
/// le body réel pour mesurer les vraies performances de production.
fn classify_heuristic(title: &str, body_preview: &str) -> String {
    let (section, _confidence) = heuristic_route(title, body_preview);
    section.to_string()
}

/// ─── Preprocessing body pour LLM ─────────────────────────────────────────────
/// Nettoie le corps Markdown avant envoi au LLM (Qwen3-0.6B).
///
/// Opérations dans l'ordre :
/// 1. Strip frontmatter YAML (bloc `---\n...\n---\n` en tête)
/// 2. Strip blocs de code fencés (``` ``` ```)
/// 3. Remplace les inline code par leur contenu textuel
/// 4. Remplace les wikilinks `[[id]]` par `id`
/// 5. Strip les préfixes de headings Markdown (`## `, `# `, etc.)
/// 6. Collapse les espaces multiples en un seul
/// 7. Truncate à 500 chars (après nettoyage)
///
/// L'heuristique regex reçoit toujours le body brut — seul le LLM bénéficie
/// de ce preprocessing (l'heuristique TF-IDF tolère le Markdown).
fn clean_body_for_llm(body: &str) -> String {
    static MD_HEADING: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?m)^#{1,6}\s+").expect("pattern MD_HEADING valide"));
    static WIKILINK: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\[\[([^\]]+)\]\]").expect("pattern WIKILINK valide"));
    static CODE_INLINE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"`([^`]+)`").expect("pattern CODE_INLINE valide"));
    static CODE_BLOCK: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?s)```[a-z]*\n.*?```").expect("pattern CODE_BLOCK valide"));
    static WHITESPACE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\s+").expect("pattern WHITESPACE valide"));

    let mut s = body.to_string();

    // 1. Strip frontmatter YAML
    if s.starts_with("---\n") {
        if let Some(end) = s[4..].find("\n---\n") {
            s = s[4 + end + 5..].to_string();
        }
    }

    // 2. Strip blocs de code fencés
    s = CODE_BLOCK.replace_all(&s, " ").into_owned();

    // 3. Inline code → contenu textuel
    s = CODE_INLINE.replace_all(&s, "$1").into_owned();

    // 4. Wikilinks → id textuel
    s = WIKILINK.replace_all(&s, "$1").into_owned();

    // 5. Strip préfixes headings (par ligne)
    s = s
        .lines()
        .map(|l| MD_HEADING.replace(l, "").into_owned())
        .collect::<Vec<_>>()
        .join(" ");

    // 6. Collapse whitespace
    s = WHITESPACE.replace_all(s.trim(), " ").into_owned();

    // 7. Truncate à 500 chars
    if s.len() > 500 {
        // Truncate sur une frontière ASCII safe (évite de couper en milieu de multi-byte UTF-8)
        let truncate_at = s
            .char_indices()
            .take_while(|(i, _)| *i < 500)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(500);
        s.truncate(truncate_at);
    }

    s
}

/// ─── Backend LLM Qwen3 ───────────────────────────────────────────────────────
/// Prompt système pour la classification de section gradatum.
///
/// Utilise `CLASSIFIER_SYSTEM_PROMPT` depuis `gradatum_curator` (prompt v2,
/// 11 sections dont council + critères exclusion + hint injection).
/// Ce prompt est le même que celui utilisé en production par `CuratorPipeline`.
///
/// Note : Qwen3-4B ne nécessite pas le tag `/no_think` avec ce prompt JSON structuré —
/// la température 0.0 + format JSON explicit désactivent le mode thinking.
const SYSTEM_PROMPT: &str = gradatum_curator::CLASSIFIER_SYSTEM_PROMPT;

/// Payload envoyé à l'endpoint OpenAI-compatible.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Réponse partielle de l'endpoint (seuls les champs utiles).
#[derive(Deserialize, Debug)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize, Debug)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Deserialize, Debug)]
struct ChatMessageResponse {
    content: String,
}

/// Normalise une réponse LLM brute en section canonique.
///
/// Prend le premier token en minuscules, strip les backticks et espaces,
/// puis vérifie que c'est une section connue parmi les 11 sections canoniques.
/// Fallback : "reference".
///
/// Note : on normalise contre `SECTIONS_11` (11 sections) car le prompt v2
/// peut retourner "council". Le dataset 1 (10 sections) ne contient pas de notes
/// council — si le LLM retourne "council" sur une note du dataset 1, elle sera
/// comptabilisée comme prédiction incorrecte (cas marginal sans impact matériel).
///
/// Prompt v2 retourne un JSON structuré `{"section": "...", ...}`.
/// En cas de thinking inline `<think>...</think>`, le bloc est strippé avant parsing.
fn normalize_section(raw: &str) -> String {
    // Strip le bloc thinking Qwen3 si présent (ex: "<think>...\n</think>\n{...}")
    let stripped = if let Some(end) = raw.find("</think>") {
        raw[end + "</think>".len()..].trim()
    } else {
        raw.trim()
    };

    // Prompt v2 retourne un JSON — extraire le champ "section" si présent.
    // Fallback sur le texte brut si non-JSON (rétrocompat mode simple-token).
    let candidate = if let Ok(v) = serde_json::from_str::<serde_json::Value>(stripped) {
        v.get("section")
            .and_then(|s| s.as_str())
            .unwrap_or(stripped)
            .to_lowercase()
    } else {
        stripped
            .trim_matches('`')
            .to_lowercase()
            .split_whitespace()
            .next()
            .unwrap_or("reference")
            .to_string()
    };

    let clean = candidate.as_str();

    if SECTIONS_11.contains(&clean) {
        clean.to_string()
    } else {
        // Tente un match partiel (ex: "retrospective" → "retrospectives")
        SECTIONS_11
            .iter()
            .find(|s| s.starts_with(clean) || clean.starts_with(*s))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "reference".to_string())
    }
}

/// Classifie un batch de paires `(title, body_preview)` via un LLM OpenAI-compatible.
///
/// Retourne `(predictions, throttle_count)`.
/// En cas d'erreur sur une note individuelle, fallback sur l'heuristique regex
/// et incrémente le compteur de throttles.
///
/// Ce noyau accepte `&[(title, body)]` pour être partagé par les deux datasets
/// (DatasetNote et GoldenNote) sans duplication — ADN 3 factorisé.
async fn classify_llm_batch(
    pairs: &[(&str, &str)],
    endpoint: &str,
    model: &str,
) -> Result<(Vec<String>, usize)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("build reqwest client")?;

    let url = format!("{endpoint}/v1/chat/completions");
    let mut predictions = Vec::with_capacity(pairs.len());
    let mut throttle_count = 0usize;

    for (title, body_preview) in pairs {
        // Bug 2 fix : le body Markdown brut polluait Qwen3-0.6B (-0.198 F1 vs titre-seul).
        // clean_body_for_llm() strip headings/wikilinks/code/frontmatter avant envoi.
        // L'heuristique reçoit toujours le body brut (elle tolère le Markdown).
        // Prompt v2 : JSON structuré attendu, max_tokens=64 (aligné avec [curator.classify]).
        // Format user_content identique à CuratorPipeline::process() (pas de hint_line ici —
        // le bench ne teste pas l'injection hint, uniquement la classification LLM).
        let cleaned_body = clean_body_for_llm(body_preview);
        let user_content = format!(
            "Classify this note.\nTitle: {title}\nBody (truncated to 500 chars): {cleaned_body}",
        );
        let req_body = ChatRequest {
            model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: SYSTEM_PROMPT,
                },
                ChatMessage {
                    role: "user",
                    content: &user_content,
                },
            ],
            temperature: 0.0,
            max_tokens: 64,
        };

        match client.post(&url).json(&req_body).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<ChatResponse>().await {
                    Ok(chat_resp) => {
                        let raw = chat_resp
                            .choices
                            .first()
                            .map(|c| c.message.content.as_str())
                            .unwrap_or("reference");
                        predictions.push(normalize_section(raw));
                    }
                    Err(_e) => {
                        // JSON malformé ou champ absent → fallback heuristique
                        throttle_count += 1;
                        predictions.push(classify_heuristic(title, body_preview));
                    }
                }
            }
            Ok(resp) => {
                // HTTP 429 ou 5xx → fallback heuristique
                let status = resp.status();
                if status.as_u16() == 429 {
                    throttle_count += 1;
                }
                predictions.push(classify_heuristic(title, body_preview));
            }
            Err(_e) => {
                // Réseau KO → fallback heuristique
                throttle_count += 1;
                predictions.push(classify_heuristic(title, body_preview));
            }
        }
    }

    Ok((predictions, throttle_count))
}

/// Classifie un batch de [`DatasetNote`] via LLM.
///
/// Délègue à [`classify_llm_batch`] après extraction des paires `(title, body_preview)`.
async fn classify_qwen_batch(
    notes: &[DatasetNote],
    endpoint: &str,
    model: &str,
) -> Result<(Vec<String>, usize)> {
    let pairs: Vec<(&str, &str)> = notes
        .iter()
        .map(|n| (n.title.as_str(), n.body_preview.as_str()))
        .collect();
    classify_llm_batch(&pairs, endpoint, model).await
}

/// Classifie un batch de [`GoldenNote`] via LLM.
///
/// Délègue à [`classify_llm_batch`] après extraction des paires `(title, body_preview)`.
async fn classify_qwen_batch_golden(
    notes: &[GoldenNote],
    endpoint: &str,
    model: &str,
) -> Result<(Vec<String>, usize)> {
    let pairs: Vec<(&str, &str)> = notes
        .iter()
        .map(|n| (n.title.as_str(), n.body_preview.as_str()))
        .collect();
    classify_llm_batch(&pairs, endpoint, model).await
}

/// ─── Matrice de confusion ────────────────────────────────────────────────────
fn build_confusion(labels: &[String], predictions: &[String]) -> HashMap<String, usize> {
    let mut confusion: HashMap<String, usize> = HashMap::new();
    for (label, pred) in labels.iter().zip(predictions.iter()) {
        // Clé : "true__predicted" (évite le tuple non-sérialisable)
        let key = format!("{label}__{pred}");
        *confusion.entry(key).or_insert(0) += 1;
    }
    confusion
}

/// ─── Gap analysis ────────────────────────────────────────────────────────────
/// Collecte les notes mal classifiées pour le gap analysis.
fn collect_gaps(notes: &[DatasetNote], predictions: &[String]) -> Vec<Gap> {
    notes
        .iter()
        .zip(predictions.iter())
        .filter(|(note, pred)| note.expected_section_gradatum != **pred)
        .map(|(note, pred)| Gap {
            path: note.path.clone(),
            title: note.title.clone(),
            legacy_section: note.legacy_section.clone(),
            expected_section_gradatum: note.expected_section_gradatum.clone(),
            predicted_section: pred.clone(),
            body_preview: note.body_preview.clone(),
        })
        .collect()
}

/// ─── Affichage ───────────────────────────────────────────────────────────────
fn print_result(result: &BenchResult) {
    println!(
        "\n--- Backend: {} ({} notes, {:.2}s) ---",
        result.backend, result.n, result.elapsed_sec
    );
    println!("  accuracy      : {:.4}", result.accuracy);
    println!("  F1 micro      : {:.4}", result.f1_micro);
    println!("  F1 weighted   : {:.4}", result.f1_weighted);
    println!("  F1 macro      : {:.4}", result.f1_macro);
    println!("  Per section   :");
    let mut sections: Vec<_> = result.per_section.iter().collect();
    sections.sort_by_key(|(s, _)| s.as_str());
    for (section, metrics) in &sections {
        println!(
            "    {:20} P={:.3} R={:.3} F1={:.3} (n={})",
            section, metrics.precision, metrics.recall, metrics.f1, metrics.support
        );
    }
}

/// ─── Sélection du sample de 15 gaps représentatifs ─────────────────────────
/// Sélectionne les 15 gaps les plus représentatifs selon le template de la mission :
/// - 5 confusions decisions ↔ reasoning
/// - 3 confusions debug ↔ agent-issues
/// - 3 confusions retrospectives ↔ lessons-learned
/// - 4 autres par fréquence dans la matrice de confusion
fn select_gap_sample(gaps: &[Gap], confusion: &HashMap<String, usize>) -> Vec<Gap> {
    let mut sample: Vec<Gap> = Vec::new();
    let mut used_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Helper : trouver des gaps pour une paire de sections
    let find_pair =
        |a: &str, b: &str, limit: usize, used: &std::collections::HashSet<String>| -> Vec<Gap> {
            gaps.iter()
                .filter(|g| {
                    !used.contains(&g.path)
                        && ((g.expected_section_gradatum == a && g.predicted_section == b)
                            || (g.expected_section_gradatum == b && g.predicted_section == a))
                })
                .take(limit)
                .cloned()
                .collect()
        };

    // 5 confusions decisions ↔ reasoning
    let pair1 = find_pair("decisions", "reasoning", 5, &used_paths);
    for g in &pair1 {
        used_paths.insert(g.path.clone());
    }
    sample.extend(pair1);

    // 3 confusions debug ↔ agent-issues
    let pair2 = find_pair("debug", "agent-issues", 3, &used_paths);
    for g in &pair2 {
        used_paths.insert(g.path.clone());
    }
    sample.extend(pair2);

    // 3 confusions retrospectives ↔ lessons-learned
    let pair3 = find_pair("retrospectives", "lessons-learned", 3, &used_paths);
    for g in &pair3 {
        used_paths.insert(g.path.clone());
    }
    sample.extend(pair3);

    // 4 autres par fréquence confusion (top paires non encore représentées)
    let mut top_pairs: Vec<_> = confusion
        .iter()
        .filter(|(key, _)| {
            !key.contains("__") || {
                // Garder uniquement les paires où true != predicted (hors diagonale)
                let parts: Vec<_> = key.split("__").collect();
                parts.len() == 2 && parts[0] != parts[1]
            }
        })
        .collect();
    top_pairs.sort_by_key(|(_, count)| std::cmp::Reverse(**count));

    for (key, _count) in &top_pairs {
        if sample.len() >= 15 {
            break;
        }
        let parts: Vec<_> = key.split("__").collect();
        if parts.len() != 2 {
            continue;
        }
        let (true_sec, pred_sec) = (parts[0], parts[1]);
        if true_sec == pred_sec {
            continue;
        }
        let extras = gaps
            .iter()
            .filter(|g| {
                !used_paths.contains(&g.path)
                    && g.expected_section_gradatum == true_sec
                    && g.predicted_section == pred_sec
            })
            .take(1)
            .cloned()
            .collect::<Vec<_>>();
        for g in &extras {
            used_paths.insert(g.path.clone());
        }
        sample.extend(extras);
    }

    sample
}

/// Génère le rapport markdown lisible du gap sample.
fn format_gap_sample_md(backend: &str, sample: &[Gap], date: &str) -> String {
    let mut md = format!(
        "# Gap Sample — Backend `{backend}` — {date}\n\n\
         Sélection des gaps les plus représentatifs (≤15) pour revue the maintainer.\n\n"
    );

    for (i, gap) in sample.iter().enumerate() {
        md.push_str(&format!(
            "## Gap {}\n\
             - **Path** : `{}`\n\
             - **Titre** : {}\n\
             - **Section legacy vault** : `{}`\n\
             - **Section attendue (gradatum)** : `{}`\n\
             - **Section prédite** : `{}`\n\
             - **Preview** : {}\n\n",
            i + 1,
            gap.path,
            gap.title,
            gap.legacy_section,
            gap.expected_section_gradatum,
            gap.predicted_section,
            &gap.body_preview[..gap.body_preview.len().min(200)],
        ));
    }

    md
}

// ─── Golden-set A2 v0.4.3 ───────────────────────────────────────────────────

/// Sections canoniques 11 — inclut `council` (A1 post-fix).
///
/// Aligné avec `gradatum_curator::routing::SECTIONS` — importé directement
/// dans la logique golden pour rester en sync sans duplication.
const SECTIONS_11: &[&str] = &[
    "decisions",
    "council",
    "architecture",
    "debug",
    "reasoning",
    "feedback",
    "lessons-learned",
    "retrospectives",
    "experiments",
    "agent-issues",
    "reference",
];

/// Calcule les métriques hint + council→reference sur le golden-set.
///
/// `predictions[i]` = section prédite par l'heuristique pour la note golden `i`.
fn compute_golden_metrics(notes: &[GoldenNote], predictions: &[String]) -> GoldenMetrics {
    let mut n_with_hint = 0usize;
    let mut hint_matched = 0usize;
    let mut n_council = 0usize;
    let mut council_to_reference = 0usize;

    for (note, pred) in notes.iter().zip(predictions.iter()) {
        // Métriques hint
        if let Some(ref hint) = note.section_hint {
            // Seuls les hints valides sont comptabilisés (hint invalide = warn silencieux curator).
            if SECTIONS_11.contains(&hint.as_str()) {
                n_with_hint += 1;
                if pred == hint {
                    hint_matched += 1;
                }
            }
        }

        // Métriques council→reference
        if note.expected_section == "council" {
            n_council += 1;
            if pred == "reference" {
                council_to_reference += 1;
            }
        }
    }

    let hint_respect_rate = if n_with_hint > 0 {
        hint_matched as f32 / n_with_hint as f32
    } else {
        0.0
    };

    let council_to_reference_rate = if n_council > 0 {
        council_to_reference as f32 / n_council as f32
    } else {
        0.0
    };

    GoldenMetrics {
        n_with_hint,
        hint_matched,
        hint_respect_rate,
        n_council,
        council_to_reference,
        council_to_reference_rate,
    }
}

/// Gap doré — note golden-set mal classifiée.
#[derive(Serialize, Debug, Clone)]
struct GoldenGap {
    path: String,
    title: String,
    expected_section: String,
    predicted_section: String,
    section_hint: Option<String>,
    body_preview_short: String,
}

/// Collecte les gaps du golden-set (prédiction ≠ section attendue).
fn collect_golden_gaps(notes: &[GoldenNote], predictions: &[String]) -> Vec<GoldenGap> {
    notes
        .iter()
        .zip(predictions.iter())
        .filter(|(note, pred)| note.expected_section != **pred)
        .map(|(note, pred)| GoldenGap {
            path: note.path.clone(),
            title: note.title.clone(),
            expected_section: note.expected_section.clone(),
            predicted_section: pred.clone(),
            section_hint: note.section_hint.clone(),
            body_preview_short: note.body_preview.chars().take(150).collect(),
        })
        .collect()
}

/// Affiche le résumé du golden-set avec métriques spécifiques.
fn print_golden_result(result: &BenchResult, golden: &GoldenMetrics) {
    println!(
        "\n=== Golden-set A2 — Backend: {} ({} notes, {:.2}s) ===",
        result.backend, result.n, result.elapsed_sec
    );
    println!("  accuracy      : {:.4}", result.accuracy);
    println!("  F1 micro      : {:.4}", result.f1_micro);
    println!("  F1 weighted   : {:.4}", result.f1_weighted);
    println!("  F1 macro      : {:.4}", result.f1_macro);
    println!("\n  --- Métriques spécifiques golden-set ---");
    println!(
        "  hint_respect_rate       : {:.4}  ({}/{} hints respectés)",
        golden.hint_respect_rate, golden.hint_matched, golden.n_with_hint
    );
    println!(
        "  council→reference rate  : {:.4}  ({}/{} notes council mal routées)",
        golden.council_to_reference_rate, golden.council_to_reference, golden.n_council
    );
    println!("\n  Per section   :");
    let mut sections: Vec<_> = result.per_section.iter().collect();
    sections.sort_by_key(|(s, _)| s.as_str());
    for (section, metrics) in &sections {
        println!(
            "    {:20} P={:.3} R={:.3} F1={:.3} (n={})",
            section, metrics.precision, metrics.recall, metrics.f1, metrics.support
        );
    }
}

/// ─── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let date = Utc::now().format("%Y-%m-%d").to_string();
    // Fix Z.2d : dataset avec body_preview mergé depuis la source JSONL
    let dataset_path = "crates/gradatum-bench/datasets/gradatum-balanced-v1-with-body.jsonl";
    // Rétrocompat : LLM_ENDPOINT prioritaire, QWEN_ENDPOINT accepté en fallback
    let qwen_endpoint = std::env::var("LLM_ENDPOINT")
        .or_else(|_| std::env::var("QWEN_ENDPOINT"))
        .unwrap_or_else(|_| "http://localhost:8082".into());
    let llm_model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "Qwen3-4B-Instruct-2507".into());

    // Mode CI : GRADATUM_CI=1 → assertions P2.0c (seuil 0.78) + skip propre si dataset absent.
    // Le dataset gradatum-balanced-v1-with-body.jsonl est gitignored (données perso).
    let ci_mode = std::env::var("GRADATUM_CI").is_ok();

    // ── Chargement dataset ────────────────────────────────────────────────────
    let content = match std::fs::read_to_string(dataset_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && ci_mode => {
            // Dataset personnel gitignored absent en CI → skip propre (exit 0).
            // Le bench déterministe CI utilise les wiremock tests (T9) pour validation runtime.
            eprintln!(
                "CI WARN: dataset absent ({dataset_path}) — bench curator_f1 skipped (gitignored perso). \
                 Derniers résultats connus : heuristic F1w 0.7871 / LLM F1w 0.7938 (alpha.3 T11 P2.0b)."
            );
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("Lecture dataset: {dataset_path}")),
    };
    let notes: Vec<DatasetNote> = content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).with_context(|| format!("Parse ligne: {l}")))
        .collect::<Result<Vec<_>>>()?;

    let n = notes.len();
    println!("Bench dataset : {n} notes / 10 sections gradatum");
    println!("LLM endpoint  : {qwen_endpoint}");
    println!("LLM model     : {llm_model}");
    println!("Heuristique   : heuristic_route(title, body_preview) — fix Z.2d avec body 500 chars");

    let labels: Vec<String> = notes
        .iter()
        .map(|n| n.expected_section_gradatum.clone())
        .collect();

    std::fs::create_dir_all("target/bench").context("Création target/bench/")?;

    // ── Backend heuristic ─────────────────────────────────────────────────────
    println!("\n[1/2] Backend heuristic...");
    let t_start = Instant::now();
    let heuristic_preds: Vec<String> = notes
        .iter()
        .map(|note| classify_heuristic(&note.title, &note.body_preview))
        .collect();
    let heuristic_elapsed = t_start.elapsed().as_secs_f64();

    let (h_acc, h_f1_micro, h_f1_weighted, h_f1_macro, h_per_section) =
        compute_metrics(&labels, &heuristic_preds, SECTIONS);
    let h_confusion = build_confusion(&labels, &heuristic_preds);

    let heuristic_result = BenchResult {
        backend: "heuristic".into(),
        n,
        accuracy: h_acc,
        f1_micro: h_f1_micro,
        f1_weighted: h_f1_weighted,
        f1_macro: h_f1_macro,
        per_section: h_per_section,
        confusion: h_confusion.clone(),
        elapsed_sec: heuristic_elapsed,
    };
    print_result(&heuristic_result);

    // Gap analysis heuristic
    let h_gaps = collect_gaps(&notes, &heuristic_preds);
    println!("  Gaps heuristic : {} / {n}", h_gaps.len());

    // ── Backend LLM ───────────────────────────────────────────────────────────
    println!("\n[2/2] Backend LLM {llm_model} ({qwen_endpoint})...");
    let t_start = Instant::now();
    let (qwen_preds, throttle_count) = classify_qwen_batch(&notes, &qwen_endpoint, &llm_model)
        .await
        .unwrap_or_else(|e| {
            eprintln!("WARN: Qwen endpoint KO ({e}) — fallback heuristic-only");
            let preds = notes
                .iter()
                .map(|n| classify_heuristic(&n.title, &n.body_preview))
                .collect();
            (preds, n)
        });
    let qwen_elapsed = t_start.elapsed().as_secs_f64();

    let qwen_backend_label = if throttle_count == n {
        format!("{llm_model} (fallback-heuristic-all)")
    } else if throttle_count > 0 {
        format!("{llm_model} ({throttle_count}/{n} fallback)")
    } else {
        llm_model.clone()
    };

    let (q_acc, q_f1_micro, q_f1_weighted, q_f1_macro, q_per_section) =
        compute_metrics(&labels, &qwen_preds, SECTIONS);
    let q_confusion = build_confusion(&labels, &qwen_preds);

    let qwen_result = BenchResult {
        backend: qwen_backend_label,
        n,
        accuracy: q_acc,
        f1_micro: q_f1_micro,
        f1_weighted: q_f1_weighted,
        f1_macro: q_f1_macro,
        per_section: q_per_section,
        confusion: q_confusion.clone(),
        elapsed_sec: qwen_elapsed,
    };
    print_result(&qwen_result);

    let q_gaps = collect_gaps(&notes, &qwen_preds);
    println!("  Gaps Qwen     : {} / {n}", q_gaps.len());
    if throttle_count > 0 {
        println!("  WARN: {throttle_count}/{n} appels LLM → fallback heuristique");
    }

    // ── Sauvegarde JSON résultats ─────────────────────────────────────────────
    let out_json_path = format!("target/bench/curator-f1-gradatum-natif-fix-bugs-{date}.json");
    let out_json = serde_json::json!({
        "dataset": "gradatum-balanced-v1-with-body",
        "n": n,
        "date": date,
        "fixes": [
            "Bug1: routing.rs regex \\b→[SECTION] alternance pour word-boundary [+D",
            "Bug2: clean_body_for_llm() strip Markdown avant envoi Qwen3-0.6B",
        ],
        "results": [&heuristic_result, &qwen_result],
    });
    std::fs::write(
        &out_json_path,
        serde_json::to_string_pretty(&out_json).context("Sérialisation JSON résultats")?,
    )
    .with_context(|| format!("Écriture {out_json_path}"))?;
    println!("\nRésultats JSON : {out_json_path}");

    // ── Gap analysis JSONL (backend heuristic — référence P2.0b) ─────────────
    let out_gaps_path = format!("target/bench/gap-analysis-fix-bugs-{date}.jsonl");
    let gaps_content = h_gaps
        .iter()
        .map(|g| serde_json::to_string(g).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&out_gaps_path, gaps_content + "\n")
        .with_context(|| format!("Écriture {out_gaps_path}"))?;
    println!("Gap analysis   : {out_gaps_path} ({} lignes)", h_gaps.len());

    // ── Gap sample markdown (15 gaps représentatifs) ──────────────────────────
    let out_sample_path = format!("target/bench/gap-sample-review-{date}.md");
    let sample = select_gap_sample(&h_gaps, &h_confusion);
    let sample_md = format_gap_sample_md("heuristic", &sample, &date);
    std::fs::write(&out_sample_path, &sample_md)
        .with_context(|| format!("Écriture {out_sample_path}"))?;
    println!("Gap sample     : {out_sample_path} ({} gaps)", sample.len());

    // ── Golden-set A2 v0.4.3 ─────────────────────────────────────────────────
    // Le golden-set est commité (non gitignored) → toujours exécuté, en CI compris.
    // Il mesure la baseline AVANT prompt v2 (A3) sur 41 cas incluant `council` (11e section).
    //
    // Le mode LLM golden n'est exécuté que si le LLM a répondu sur le dataset 1
    // (throttle_count < n), sinon on mesure l'heuristique seule.
    // Path du golden-set. Override possible via `CURATOR_GOLDEN_PATH` (D2.4) :
    // permet de pointer vers un golden-set étendu (≥50 cas, synthétique généré par
    // `scripts/gen_golden_curator_synthetic.sh`) sans écraser le `v1` local perso.
    // Défaut : `golden-set-curator-v1.jsonl` (gitignored, données perso locales).
    let golden_path = std::env::var("CURATOR_GOLDEN_PATH")
        .unwrap_or_else(|_| "crates/gradatum-bench/datasets/golden-set-curator-v1.jsonl".into());
    let golden_path = golden_path.as_str();
    println!("\n═══════════════════════════════════════════════════════════════");
    println!("[A2] Golden-set classification baseline — v0.4.3 curator B3 piste d");
    println!("     Dataset : {golden_path}");
    println!("═══════════════════════════════════════════════════════════════");

    match std::fs::read_to_string(golden_path) {
        Err(e) => {
            eprintln!("WARN: golden-set absent ({golden_path}) — {e}");
        }
        Ok(golden_content) => {
            let golden_notes: Result<Vec<GoldenNote>> = golden_content
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str(l).with_context(|| format!("Parse golden: {l}")))
                .collect();

            match golden_notes {
                Err(e) => {
                    eprintln!("WARN: erreur parse golden-set — {e}");
                }
                Ok(golden_notes) => {
                    let gn = golden_notes.len();
                    println!("\n  {gn} notes chargées (11 sections dont council)");

                    // Extracte les labels attendus
                    let golden_labels: Vec<String> = golden_notes
                        .iter()
                        .map(|n| n.expected_section.clone())
                        .collect();

                    // ── Backend heuristique sur golden-set ────────────────────
                    let t_golden = Instant::now();
                    let golden_heuristic_preds: Vec<String> = golden_notes
                        .iter()
                        .map(|note| classify_heuristic(&note.title, &note.body_preview))
                        .collect();
                    let golden_h_elapsed = t_golden.elapsed().as_secs_f64();

                    let (gh_acc, gh_f1_micro, gh_f1_weighted, gh_f1_macro, gh_per_section) =
                        compute_metrics(&golden_labels, &golden_heuristic_preds, SECTIONS_11);
                    let gh_confusion = build_confusion(&golden_labels, &golden_heuristic_preds);
                    let gh_golden_metrics =
                        compute_golden_metrics(&golden_notes, &golden_heuristic_preds);

                    let golden_h_result = BenchResult {
                        backend: "heuristic (golden)".into(),
                        n: gn,
                        accuracy: gh_acc,
                        f1_micro: gh_f1_micro,
                        f1_weighted: gh_f1_weighted,
                        f1_macro: gh_f1_macro,
                        per_section: gh_per_section,
                        confusion: gh_confusion,
                        elapsed_sec: golden_h_elapsed,
                    };
                    print_golden_result(&golden_h_result, &gh_golden_metrics);

                    let gh_gaps = collect_golden_gaps(&golden_notes, &golden_heuristic_preds);
                    println!("  Gaps golden heuristic : {} / {gn}", gh_gaps.len());

                    // ── Backend LLM sur golden-set (si LLM disponible) ────────
                    // On réutilise le même endpoint/modèle que le dataset 1.
                    // Si throttle_count == n (LLM KO sur dataset 1), on skip le LLM golden
                    // pour éviter des appels inutiles sur un endpoint down.
                    let (golden_llm_result, gq_golden_metrics) = if throttle_count < notes.len() {
                        let t_golden_llm = Instant::now();
                        let (golden_llm_preds, golden_throttle) =
                            classify_qwen_batch_golden(&golden_notes, &qwen_endpoint, &llm_model)
                                .await
                                .unwrap_or_else(|e| {
                                    eprintln!(
                                        "WARN: LLM golden endpoint KO ({e}) — fallback heuristic"
                                    );
                                    let preds = golden_notes
                                        .iter()
                                        .map(|n| classify_heuristic(&n.title, &n.body_preview))
                                        .collect();
                                    (preds, gn)
                                });
                        let golden_llm_elapsed = t_golden_llm.elapsed().as_secs_f64();

                        let gq_golden_metrics =
                            compute_golden_metrics(&golden_notes, &golden_llm_preds);
                        let (gq_acc, gq_f1_micro, gq_f1_weighted, gq_f1_macro, gq_per_section) =
                            compute_metrics(&golden_labels, &golden_llm_preds, SECTIONS_11);
                        let gq_confusion = build_confusion(&golden_labels, &golden_llm_preds);

                        let gq_backend_label = if golden_throttle == gn {
                            format!("{llm_model} (fallback-heuristic-all, golden)")
                        } else if golden_throttle > 0 {
                            format!("{llm_model} ({golden_throttle}/{gn} fallback, golden)")
                        } else {
                            format!("{llm_model} (golden)")
                        };

                        let golden_llm_result = BenchResult {
                            backend: gq_backend_label,
                            n: gn,
                            accuracy: gq_acc,
                            f1_micro: gq_f1_micro,
                            f1_weighted: gq_f1_weighted,
                            f1_macro: gq_f1_macro,
                            per_section: gq_per_section,
                            confusion: gq_confusion,
                            elapsed_sec: golden_llm_elapsed,
                        };
                        print_golden_result(&golden_llm_result, &gq_golden_metrics);

                        let gq_gaps = collect_golden_gaps(&golden_notes, &golden_llm_preds);
                        println!("  Gaps golden LLM : {} / {gn}", gq_gaps.len());
                        if golden_throttle > 0 {
                            println!(
                                "  WARN: {golden_throttle}/{gn} appels LLM golden → fallback heuristique"
                            );
                        }

                        (Some(golden_llm_result), Some(gq_golden_metrics))
                    } else {
                        println!(
                            "\n  [LLM golden skipped — endpoint KO sur dataset 1 (throttle={}/{})]",
                            throttle_count,
                            notes.len()
                        );
                        (None, None)
                    };

                    // ── Sauvegarde baseline JSON golden ───────────────────────
                    let out_golden_path =
                        format!("target/bench/curator-golden-baseline-{date}.json");
                    let out_golden = serde_json::json!({
                        "dataset": "golden-set-curator-v1",
                        "dataset_path": golden_path,
                        "n": gn,
                        "date": date,
                        "version": "v0.4.3-A2-baseline",
                        "note": "Baseline AVANT prompt v2 (A3). council = 11e section. hint fort post-A1.",
                        "sections_11": SECTIONS_11,
                        "results": {
                            "heuristic": {
                                "bench": &golden_h_result,
                                "golden_metrics": &gh_golden_metrics,
                                "gaps": &gh_gaps,
                            },
                            "llm": golden_llm_result.as_ref().map(|r| serde_json::json!({
                                "bench": r,
                                "golden_metrics": &gq_golden_metrics,
                            })),
                        }
                    });
                    match std::fs::write(
                        &out_golden_path,
                        serde_json::to_string_pretty(&out_golden)
                            .context("Sérialisation JSON golden")?,
                    ) {
                        Ok(()) => println!("\nGolden baseline JSON : {out_golden_path}"),
                        Err(e) => eprintln!("WARN: écriture golden baseline échouée — {e}"),
                    }

                    // ── Résumé baseline (chiffres à consigner) ────────────────
                    println!("\n╔═══════════════════════════════════════════════════════╗");
                    println!("║  BASELINE A2 — Chiffres à consigner (AVANT A3)       ║");
                    println!("╠═══════════════════════════════════════════════════════╣");
                    println!(
                        "║  Heuristic accuracy     : {:.4}                       ║",
                        gh_acc
                    );
                    println!(
                        "║  Heuristic F1 weighted  : {:.4}                       ║",
                        gh_f1_weighted
                    );
                    println!(
                        "║  hint_respect_rate      : {:.4}  ({}/{} hints)         ║",
                        gh_golden_metrics.hint_respect_rate,
                        gh_golden_metrics.hint_matched,
                        gh_golden_metrics.n_with_hint
                    );
                    println!(
                        "║  council→ref rate       : {:.4}  ({}/{} council)       ║",
                        gh_golden_metrics.council_to_reference_rate,
                        gh_golden_metrics.council_to_reference,
                        gh_golden_metrics.n_council
                    );
                    println!(
                        "║  Gaps heuristic         : {}/{}                        ║",
                        gh_gaps.len(),
                        gn
                    );
                    println!("╚═══════════════════════════════════════════════════════╝");
                }
            }
        }
    }

    // ── Exit P2.0b assertions ─────────────────────────────────────────────────
    println!("\n=== Exit P2.0b assertions ===");
    println!(
        "Heuristic F1 weighted : {:.4}  (seuil ≥ 0.65) → {}",
        h_f1_weighted,
        if h_f1_weighted >= 0.65 {
            "PASS"
        } else {
            "FAIL"
        }
    );
    println!(
        "LLM F1 weighted       : {:.4}  (seuil ≥ 0.75) → {}{}",
        q_f1_weighted,
        if q_f1_weighted >= 0.75 {
            "PASS"
        } else {
            "FAIL"
        },
        if throttle_count == n {
            " [WARN: fallback-only — seuil LLM non applicable]"
        } else {
            ""
        }
    );

    // En mode CI (GRADATUM_CI=1) les assertions P2.0c remplacent P2.0b → skip P2.0b.
    if !ci_mode {
        if h_f1_weighted < 0.65 {
            eprintln!(
                "FAIL: heuristic F1 weighted {:.4} < 0.65 — floor P2.0b breach",
                h_f1_weighted
            );
            std::process::exit(1);
        }

        // Le seuil LLM n'est obligatoire que si l'endpoint a répondu (throttle_count < n)
        if throttle_count < n && q_f1_weighted < 0.75 {
            eprintln!(
                "FAIL: LLM F1 weighted {:.4} < 0.75 — beta threshold P2.0b breach",
                q_f1_weighted
            );
            std::process::exit(1);
        }
    }

    // ── CI assertion P2.0c (GRADATUM_CI=1) ───────────────────────────────────
    // Seuil 0.78 = ±2pp depuis alpha.3 : heuristic 0.7871 / LLM 0.7938.
    // Mode CI → exit non-zero si F1w heuristic en dessous du seuil.
    // Le seuil LLM CI n'est appliqué que si l'endpoint a effectivement répondu.
    if ci_mode {
        let f1_threshold: f32 = 0.78;
        println!("\n=== Exit P2.0c CI assertions (GRADATUM_CI=1, seuil ≥ {f1_threshold:.2}) ===");
        println!(
            "Heuristic F1 weighted : {:.4}  (seuil ≥ {f1_threshold:.2}) → {}",
            h_f1_weighted,
            if h_f1_weighted >= f1_threshold {
                "PASS"
            } else {
                "FAIL"
            }
        );
        if h_f1_weighted < f1_threshold {
            eprintln!(
                "CI FAIL: heuristic F1w {:.4} < {f1_threshold:.2} (alpha.3 0.7871 ±2pp)",
                h_f1_weighted
            );
            std::process::exit(1);
        }
        // Seuil LLM CI uniquement si l'endpoint a répondu (hors fallback total)
        if throttle_count < n {
            println!(
                "LLM F1 weighted       : {:.4}  (seuil ≥ {f1_threshold:.2}) → {}",
                q_f1_weighted,
                if q_f1_weighted >= f1_threshold {
                    "PASS"
                } else {
                    "FAIL"
                }
            );
            if q_f1_weighted < f1_threshold {
                eprintln!(
                    "CI FAIL: LLM F1w {:.4} < {f1_threshold:.2} (alpha.3 0.7938 ±2pp)",
                    q_f1_weighted
                );
                std::process::exit(1);
            }
        }
        println!("CI PASS: toutes assertions P2.0c satisfaites.");
    }

    println!("\nBench T11 P2.0b DONE.");
    Ok(())
}
