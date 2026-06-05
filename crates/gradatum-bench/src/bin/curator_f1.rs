//! Bench curator F1 — T11 P2.0b (post-bug-fix: routing regex + Qwen body preprocessing)
//!
//! Mesure la qualité de classification de section sur le dataset
//! gradatum-balanced-v1-with-body (147 notes, 10 sections) pour deux backends :
//! - `heuristic` : `heuristic_route(title, body_preview)` offline, pas de réseau
//! - `llm` : HTTP POST `/v1/chat/completions` sur `LLM_ENDPOINT` (défaut: `http://localhost:8082`)
//!   - `LLM_ENDPOINT` : URL de base (ex: `http://127.0.0.1:19999`)
//!   - `LLM_MODEL` : nom du modèle dans le payload (ex: `Qwen3-4B-Instruct-2507`)
//!   - Rétrocompat : `QWEN_ENDPOINT` toujours accepté si `LLM_ENDPOINT` absent
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
//!
//! ## Outputs
//! - `target/bench/curator-f1-gradatum-natif-fix-bugs-{date}.json`     — résultats JSON
//! - `target/bench/gap-analysis-fix-bugs-{date}.jsonl`                 — tous les gaps
//! - `target/bench/gap-sample-review-{date}.md`                         — top 15 gaps lisibles
//!
//! ## Exit P2.0b assertions
//! - heuristic F1 weighted ≥ 0.65
//! - LLM F1 weighted ≥ 0.75 (ou fallback heuristic si endpoint KO)
//!
//! ## CI assertion P2.0c (T10)
//! `GRADATUM_CI=1` active le mode assertion strict :
//! - Si le dataset est absent (gitignored perso) → WARN + exit 0 (skip propre CI)
//! - heuristic F1 weighted ≥ 0.78 (±2pp depuis alpha.3 0.7871)
//! - LLM F1 weighted ≥ 0.78 si endpoint opérationnel (±2pp depuis alpha.3 0.7938)

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use once_cell::sync::Lazy;
use regex::Regex;

use gradatum_curator::routing::heuristic_route;

/// ─── Types dataset ───────────────────────────────────────────────────────────
/// Entrée du dataset gradatum-balanced-v1-with-body.jsonl (fix Z.2d)
#[derive(Deserialize, Debug, Clone)]
struct DatasetNote {
    path: String,
    title: String,
    #[serde(alias = "legacy_section_hint")]
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
fn compute_metrics(
    labels: &[String],
    predictions: &[String],
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

    for section in SECTIONS {
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
    for section in SECTIONS {
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
    let active_sections: Vec<_> = SECTIONS
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
    let f1_weighted = SECTIONS
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

/// ─── Backend LLM Qwen3-0.6B ──────────────────────────────────────────────────
/// Prompt système pour la classification de section gradatum.
///
/// Le tag `/no_think` désactive le mode thinking de Qwen3-0.6B
/// (qui sinon remplit `reasoning_content` au lieu de `content`).
const SYSTEM_PROMPT: &str = "\
Tu es un classificateur de notes pour gradatum. \
Classifie la note dans l'une des 10 sections canoniques : \
decisions, architecture, debug, reasoning, feedback, lessons-learned, \
retrospectives, experiments, agent-issues, reference. \
Réponds UNIQUEMENT avec le nom de la section, sans aucun autre texte. \
/no_think";

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
/// puis vérifie que c'est une section connue. Fallback : "reference".
fn normalize_section(raw: &str) -> String {
    let clean = raw
        .trim()
        .trim_matches('`')
        .to_lowercase()
        .split_whitespace()
        .next()
        .unwrap_or("reference")
        .to_string();

    if SECTIONS.contains(&clean.as_str()) {
        clean
    } else {
        // Tente un match partiel (ex: "retrospective" → "retrospectives")
        SECTIONS
            .iter()
            .find(|s| s.starts_with(&clean) || clean.starts_with(*s))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "reference".to_string())
    }
}

/// Classifie un batch de notes via un LLM OpenAI-compatible.
///
/// Retourne `(predictions, throttle_count)`.
/// En cas d'erreur sur une note individuelle, fallback sur l'heuristique
/// et incrémente le compteur de throttles.
async fn classify_qwen_batch(
    notes: &[DatasetNote],
    endpoint: &str,
    model: &str,
) -> Result<(Vec<String>, usize)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("build reqwest client")?;

    let url = format!("{endpoint}/v1/chat/completions");
    let mut predictions = Vec::with_capacity(notes.len());
    let mut throttle_count = 0usize;

    for note in notes {
        // Bug 2 fix : le body Markdown brut polluait Qwen3-0.6B (-0.198 F1 vs titre-seul).
        // clean_body_for_llm() strip headings/wikilinks/code/frontmatter avant envoi.
        // L'heuristique reçoit toujours le body brut (elle tolère le Markdown).
        let cleaned_body = clean_body_for_llm(&note.body_preview);
        let user_content = format!(
            "Classify this note:\nTitle: {}\nBody (truncated to 500 chars): {}",
            note.title, cleaned_body
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
            max_tokens: 30,
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
                        predictions.push(classify_heuristic(&note.title, &note.body_preview));
                    }
                }
            }
            Ok(resp) => {
                // HTTP 429 ou 5xx → fallback heuristique
                let status = resp.status();
                if status.as_u16() == 429 {
                    throttle_count += 1;
                }
                predictions.push(classify_heuristic(&note.title, &note.body_preview));
            }
            Err(_e) => {
                // Réseau KO → fallback heuristique
                throttle_count += 1;
                predictions.push(classify_heuristic(&note.title, &note.body_preview));
            }
        }
    }

    Ok((predictions, throttle_count))
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
        compute_metrics(&labels, &heuristic_preds);
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
        compute_metrics(&labels, &qwen_preds);
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
