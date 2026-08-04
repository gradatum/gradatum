//! Retrospective vault audit / deduplication (**detection only**).
//!
//! This module performs a **batch retrospective pass** over a set of notes already present
//! in the vault and produces a **report** of waste / duplicate candidates. It reuses the
//! detection primitives of [`crate::novelty`] and [`crate::dedup`], which are **provided but
//! not wired into [`crate::CuratorPipeline`]'s `process` in 1.0.0** (planned post-1.0);
//! `audit` drives them directly for this offline pass.
//!
//! ## Founding invariant (report-only)
//!
//! This module **never mutates** the vault. It detects and reports. The archival decision
//! (a delete is an archival, reversible from the archive store) remains exclusively the
//! operator's, via the `gradatum-admin` CLI, or the system GC — never an agent, never by
//! accident. The report proposes; the operator disposes.
//!
//! The 6 `PROTECTED_DELETE` sections are excluded **upstream** (at the scan query level,
//! server-side) — this module never sees any of their notes. Detection stays
//! **precision-first**: a false positive (a live note proposed for archival) costs more
//! than a false negative (a missed duplicate).
//!
//! ## Detection tiers
//!
//! - **T0** exact duplicate — SHA-256 of the normalised body ([`novelty::content_hash`]).
//! - **T1** structural waste — rule heuristics (empty body, short probe) — `debug` only.
//! - **T2** lexical near-dup — title collision + MinHash/Jaccard ([`novelty`]).
//! - **T3** semantic duplicate — body cosine ([`dedup::cosine`]) crossed with Jaccard.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{dedup, novelty};

/// Section that carries infrastructure waste (probes, smoke tests, stress writes).
///
/// The aggressive T1 structural heuristics only apply to this section: a short note
/// anywhere else (a reference, a piece of feedback) may well be legitimate.
// ECON: section unique en dur. Upgrade -> liste configurable si un 2e cas concret apparaît.
const JUNK_SECTION: &str = "debug";

/// Detection parameters (safety bounds plus tunable thresholds).
///
/// Injected from the server configuration (`AuditConfig`) so that no magic number is
/// hard-coded in the detection logic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AuditThresholds {
    /// Body-size ceiling (bytes, trimmed) for the "short note written by the test agent"
    /// T1 signal. The empty-body and probe-title signals ignore it.
    pub min_probe_bytes: usize,
    /// Shingle size (word k-gram) used by the T2 MinHash.
    pub shingle_k: usize,
    /// A Jaccard score at or above this threshold is a strong lexical near-dup (T2).
    pub jaccard_strong: f32,
    /// Cosine value above which a semantic match would count as strong.
    ///
    /// Carried for configuration completeness only: the detection pass classifies every
    /// semantic match at [`AuditTier::Review`], so it consults [`Self::cosine_review`]
    /// alone and never reads this field.
    pub cosine_strong: f32,
    /// Cosine floor for a semantic candidate (T3). A pair at or above this value, and at
    /// or above [`Self::semantic_jaccard_floor`] on Jaccard, is reported for review.
    pub cosine_review: f32,
    /// Jaccard floor required to confirm a T3 candidate: the lexical cross-check rules
    /// out the "same topic, different content" false positive.
    pub semantic_jaccard_floor: f32,
}

impl Default for AuditThresholds {
    fn default() -> Self {
        Self {
            min_probe_bytes: 64,
            shingle_k: 3,
            jaccard_strong: 0.92,
            cosine_strong: 0.92,
            cosine_review: 0.85,
            semantic_jaccard_floor: 0.30,
        }
    }
}

/// Input note for detection (projected view, storage-free).
#[derive(Debug, Clone)]
pub struct AuditRecord {
    /// ULID of the note.
    pub id: String,
    /// Canonical section — never a delete-protected section, those are filtered upstream.
    pub section: String,
    /// Display title (derived from the body by the caller).
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// Logical author (`author_id`), when known. T1 signal: `tester` inside `debug`
    /// marks a note produced by the test agent.
    pub author_id: Option<String>,
    /// Body embedding when available. `None` is tolerated (degraded ANN mode) and simply
    /// disables the T3 comparison for this note.
    pub embedding: Option<Vec<f32>>,
    /// Embedding model identifier. T3 comparisons are restricted to pairs sharing it.
    pub embedder_id: Option<String>,
}

/// Detected duplicate / waste category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum DedupCategory {
    /// Body byte-identical (after normalisation) to another note's.
    ExactDuplicate,
    /// Probe / smoke test / empty note, with no memory value.
    StructuralJunk,
    /// Body near-identical (MinHash/Jaccard) to another note's.
    NearDuplicate,
    /// Identical title shared by several notes (misfile or migration debris).
    TitleCollision,
    /// Content semantically very close (cosine) to another note's.
    SemanticDuplicate,
}

impl DedupCategory {
    /// Severity used to arbitrate between several verdicts on one note (higher wins).
    fn severity(self) -> u8 {
        match self {
            DedupCategory::ExactDuplicate => 5,
            DedupCategory::StructuralJunk => 4,
            DedupCategory::NearDuplicate => 3,
            DedupCategory::TitleCollision => 2,
            DedupCategory::SemanticDuplicate => 1,
        }
    }

    /// Action tier recommended for this category.
    fn default_tier(self) -> AuditTier {
        match self {
            // Déchet infra pur ou doublon exact/quasi → archivable (via opérateur).
            DedupCategory::ExactDuplicate
            | DedupCategory::StructuralJunk
            | DedupCategory::NearDuplicate => AuditTier::Delete,
            // Contenu potentiellement réel → revue humaine obligatoire.
            DedupCategory::TitleCollision | DedupCategory::SemanticDuplicate => AuditTier::Review,
        }
    }

    /// Stable label for the report and for aggregate keys.
    fn as_str(self) -> &'static str {
        match self {
            DedupCategory::ExactDuplicate => "exact-duplicate",
            DedupCategory::StructuralJunk => "structural-junk",
            DedupCategory::NearDuplicate => "near-duplicate",
            DedupCategory::TitleCollision => "title-collision",
            DedupCategory::SemanticDuplicate => "semantic-duplicate",
        }
    }
}

/// Proposed action tier — **never carried out by the job itself**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AuditTier {
    /// Archival candidate: a `gradatum-admin` command is emitted for the operator to run.
    Delete,
    /// Needs a human check — never archived automatically.
    Review,
}

impl AuditTier {
    fn as_str(self) -> &'static str {
        match self {
            AuditTier::Delete => "delete",
            AuditTier::Review => "review",
        }
    }
}

/// One waste / duplicate candidate in the audit report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditCandidate {
    /// ULID of the candidate note.
    pub note_id: String,
    /// Section of the note.
    pub section: String,
    /// Title of the note.
    pub title: String,
    /// Detected category.
    pub category: DedupCategory,
    /// Proposed action tier.
    pub tier: AuditTier,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f32,
    /// ULID of the "survivor" note to keep (duplicate / near-dup cases), when applicable.
    pub survivor_id: Option<String>,
    /// Signals that triggered the verdict, for traceability.
    pub signals: Vec<String>,
}

/// An auto-downgrade action attempted by the irrelevance executor (traceability).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DowngradeAction {
    /// ULID of the note.
    pub note_id: String,
    /// Note title.
    pub title: String,
    /// Irrelevance reason carried into `status_reason`.
    pub reason: String,
    /// Result: `"downgraded"`, `"error: <msg>"`, or `"dry-run"`.
    pub outcome: String,
}

/// Complete audit report for one vault — a serialisable, report-only artefact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReport {
    /// Audited vault.
    pub vault_id: String,
    /// Generation timestamp (epoch ms).
    pub generated_at_ms: i64,
    /// Number of notes scanned (protected sections excluded).
    pub scanned: usize,
    /// Per-category counters (ordered, deterministic).
    pub counts_by_category: BTreeMap<String, usize>,
    /// Per-tier counters (ordered, deterministic).
    pub counts_by_tier: BTreeMap<String, usize>,
    /// Candidates, sorted by decreasing severity then increasing ULID.
    pub candidates: Vec<AuditCandidate>,
    /// Downgrade candidates on the relevance axis. `serde(default)` keeps older JSON
    /// reports deserialisable, with an empty vector.
    #[serde(default)]
    pub irrelevant: Vec<IrrelevantCandidate>,
    /// Downgrade actions actually carried out, pushed by the executor after
    /// [`build_report`] has returned.
    #[serde(default)]
    pub downgrade_actions: Vec<DowngradeAction>,
    /// Whether the `[downgrade] enabled` flag was set for this pass.
    ///
    /// The Markdown rendering derives its `DRY-RUN` label from this field, never from
    /// `downgrade_actions.is_empty()` — the latter would mislabel an enabled run that
    /// happened to find zero target.
    #[serde(default)]
    pub downgrade_enabled: bool,
    /// Whether the usage-collection window was covered — see [`window_covered`].
    #[serde(default)]
    pub downgrade_window_covered: bool,
}

/// Detects waste / duplicate candidates over a set of notes (one pass, one vault).
///
/// Tiers T0-T3 are run then merged: a note flagged by several tiers keeps the verdict of
/// **highest severity** (`DedupCategory::severity`). The output is **deterministic**,
/// sorted by decreasing severity then increasing ULID.
///
/// Precision-first: only T0 (exact), T1 (structural) and T2 (strong near-dup) emit
/// `Delete` candidates; T2 title collisions and T3 (semantic) stay at `Review`.
///
/// # Complexity
///
/// O(n²) over pairs (T2/T3), bounded by the caller through per-section windowing.
#[must_use]
pub fn detect(records: &[AuditRecord], th: &AuditThresholds) -> Vec<AuditCandidate> {
    // Verdict retenu par note (le plus sévère). Clé = note_id.
    let mut best: BTreeMap<String, AuditCandidate> = BTreeMap::new();

    let mut consider = |cand: AuditCandidate| {
        best.entry(cand.note_id.clone())
            .and_modify(|existing| {
                if cand.category.severity() > existing.category.severity() {
                    *existing = cand.clone();
                }
            })
            .or_insert(cand);
    };

    for c in detect_exact(records) {
        consider(c);
    }
    for c in detect_structural(records, th) {
        consider(c);
    }
    for c in detect_title_collisions(records) {
        consider(c);
    }
    for c in detect_near_and_semantic(records, th) {
        consider(c);
    }

    let mut out: Vec<AuditCandidate> = best.into_values().collect();
    out.sort_by(|a, b| {
        b.category
            .severity()
            .cmp(&a.category.severity())
            .then_with(|| a.note_id.cmp(&b.note_id))
    });
    out
}

/// T0 — exact duplicates by SHA-256 of the normalised body. The survivor is the smallest
/// (that is, oldest) ULID.
fn detect_exact(records: &[AuditRecord]) -> Vec<AuditCandidate> {
    let mut groups: BTreeMap<String, Vec<&AuditRecord>> = BTreeMap::new();
    for r in records {
        // Un corps vide n'est pas un « doublon exact » signifiant → laissé à T1.
        if r.body.trim().is_empty() {
            continue;
        }
        groups
            .entry(novelty::content_hash(&r.body))
            .or_default()
            .push(r);
    }
    let mut out = Vec::new();
    for (_hash, mut grp) in groups {
        if grp.len() < 2 {
            continue;
        }
        grp.sort_by(|a, b| a.id.cmp(&b.id));
        let survivor = grp[0].id.clone();
        for dup in grp.iter().skip(1) {
            out.push(AuditCandidate {
                note_id: dup.id.clone(),
                section: dup.section.clone(),
                title: dup.title.clone(),
                category: DedupCategory::ExactDuplicate,
                tier: DedupCategory::ExactDuplicate.default_tier(),
                confidence: 1.0,
                survivor_id: Some(survivor.clone()),
                signals: vec!["sha256-body-identical".to_string()],
            });
        }
    }
    out
}

/// T1 — structural waste (rule-based, `debug` section only).
///
/// Three signals, all confined to `debug`, the section that carries infrastructure waste:
/// 1. empty body;
/// 2. **unambiguous probe title** (`is_probe_title`), with NO size guard: such titles
///    (`smoke*`, `tagprobe*`, `… test <n>`, …) never appear on a real note, and deployment
///    smoke tests often carry a descriptive body larger than `min_probe_bytes` — a size
///    guard here was measured to miss most of them;
/// 3. author `tester` on a short body — notes generated by the test agent.
fn detect_structural(records: &[AuditRecord], th: &AuditThresholds) -> Vec<AuditCandidate> {
    let mut out = Vec::new();
    for r in records {
        if r.section != JUNK_SECTION {
            continue;
        }
        let body = r.body.trim();
        let author_tester = r.author_id.as_deref() == Some("tester");
        let (matched, conf, signal) = if body.is_empty() {
            (true, 0.99, "empty-body")
        } else if is_probe_title(&r.title) {
            (true, 0.95, "probe-title")
        } else if author_tester && body.len() <= th.min_probe_bytes {
            (true, 0.90, "tester-author+short-body")
        } else {
            (false, 0.0, "")
        };
        if matched {
            out.push(AuditCandidate {
                note_id: r.id.clone(),
                section: r.section.clone(),
                title: r.title.clone(),
                category: DedupCategory::StructuralJunk,
                tier: DedupCategory::StructuralJunk.default_tier(),
                confidence: conf,
                survivor_id: None,
                signals: vec![signal.to_string()],
            });
        }
    }
    out
}

/// Probe-title heuristic (markers of a validation or stress-test write).
///
/// **Only called for the `debug` section** — `detect_structural` filters upstream. That is
/// what makes the broad matches (`smoke` as a substring) safe: outside `debug`, legitimate
/// titles do contain `smoke` (for instance `smoke_all_10_methods_reachable` under
/// `architecture`) and this predicate never reaches them.
fn is_probe_title(title: &str) -> bool {
    let t = title.trim().to_lowercase();
    // Sondes exactes minimales.
    if t == "t" || t == "test" || t == "probe" {
        return true;
    }
    // « smoke » n'importe où (smoke-deploy-*, `[DEBUG][x] Smoke …`, `… migration smoke`).
    if t.contains("smoke") {
        return true;
    }
    // Préfixes de sonde d'indexation / canary.
    const PROBE_PREFIXES: &[&str] = &["tagprobe", "probe", "canary", "zz", "[test]"];
    if PROBE_PREFIXES.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    // Familles de stress-test : titre finissant par « … test <n> » (burst / stall / post-idle).
    is_stress_test_title(&t)
}

/// `true` when the title carries a stress-test family pattern: the word `test` immediately
/// followed by an iteration number (`burst test 8`, `post-idle test 1 — 2026-06-02`,
/// `multikind stall test 15`). The number may sit mid-title, since a date suffix is common.
fn is_stress_test_title(t: &str) -> bool {
    let tokens: Vec<&str> = t.split_whitespace().collect();
    tokens
        .windows(2)
        .any(|w| w[0] == "test" && !w[1].is_empty() && w[1].chars().all(|c| c.is_ascii_digit()))
}

/// T2a — exact title collisions (2 or more notes sharing one non-empty title). Reported at
/// tier `Review`, since real content may hide behind a duplicated title.
fn detect_title_collisions(records: &[AuditRecord]) -> Vec<AuditCandidate> {
    let mut groups: BTreeMap<String, Vec<&AuditRecord>> = BTreeMap::new();
    for r in records {
        let key = r.title.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        groups.entry(key).or_default().push(r);
    }
    let mut out = Vec::new();
    for (_title, mut grp) in groups {
        if grp.len() < 2 {
            continue;
        }
        grp.sort_by(|a, b| a.id.cmp(&b.id));
        let survivor = grp[0].id.clone();
        for other in grp.iter().skip(1) {
            out.push(AuditCandidate {
                note_id: other.id.clone(),
                section: other.section.clone(),
                title: other.title.clone(),
                category: DedupCategory::TitleCollision,
                tier: DedupCategory::TitleCollision.default_tier(),
                confidence: 0.70,
                survivor_id: Some(survivor.clone()),
                signals: vec!["exact-title-collision".to_string()],
            });
        }
    }
    out
}

/// T2b + T3 — lexical near-dup (MinHash) and semantic duplicate (cosine crossed with
/// Jaccard).
///
/// A single pair loop (O(n²), bounded per section): MinHash signatures are pre-computed
/// once per note.
fn detect_near_and_semantic(records: &[AuditRecord], th: &AuditThresholds) -> Vec<AuditCandidate> {
    // Pré-calcul des shingles (k-grammes) et signatures MinHash (128 perms, convention ingest).
    // `has_shingles[i]` = le corps a ≥ shingle_k mots. Un corps trop court produit des shingles
    // VIDES ; `minhash_signature([], 128)` renvoie alors une signature all-`u64::MAX` (longueur
    // 128, non vide) qui COLLISIONNE à jaccard=1.0 avec toute autre signature all-MAX — faux
    // positif near-dup entre deux notes distinctes très courtes. On exclut donc ces paires.
    let shingle_sets: Vec<Vec<u64>> = records
        .iter()
        .map(|r| novelty::shingles(&r.body, th.shingle_k))
        .collect();
    let has_shingles: Vec<bool> = shingle_sets.iter().map(|s| !s.is_empty()).collect();
    let sigs: Vec<Vec<u64>> = shingle_sets
        .iter()
        .map(|s| novelty::minhash_signature(s, 128))
        .collect();

    let mut out = Vec::new();
    for i in 0..records.len() {
        for j in (i + 1)..records.len() {
            // Garde P1-A : une signature dérivée de shingles vides est all-MAX et fausse le
            // Jaccard (=1.0) — on saute la paire AVANT tout calcul (couvre T2b ET T3).
            if !has_shingles[i] || !has_shingles[j] {
                continue;
            }

            // Réglage F-51 (cat-F → delete) : deux notes de MÊME titre forment une famille
            // (misfile / débris de migration au boilerplate quasi-identique). Elles sont déjà
            // signalées par `detect_title_collisions` en tier **Review** — les laisser passer
            // ici les promeut en NearDuplicate/Delete (sévérité supérieure) à tort. On saute
            // donc les paires de même titre : la collision de titre (Review) prime. Un doublon
            // exact byte-à-byte reste capté par T0 (ExactDuplicate), indépendant du titre.
            if records[i]
                .title
                .trim()
                .eq_ignore_ascii_case(records[j].title.trim())
            {
                continue;
            }

            let (a, b) = (&records[i], &records[j]);
            // Le survivant est l'ULID minimal ; le candidat est l'autre.
            let (survivor, cand) = if a.id <= b.id { (a, b) } else { (b, a) };

            let jaccard = novelty::jaccard_estimate(&sigs[i], &sigs[j]);

            // T2b : near-dup lexical fort (jamais sur corps vide → sig vide → jaccard 0).
            if jaccard >= th.jaccard_strong {
                out.push(AuditCandidate {
                    note_id: cand.id.clone(),
                    section: cand.section.clone(),
                    title: cand.title.clone(),
                    category: DedupCategory::NearDuplicate,
                    tier: DedupCategory::NearDuplicate.default_tier(),
                    confidence: jaccard.min(0.99),
                    survivor_id: Some(survivor.id.clone()),
                    signals: vec![format!("minhash-jaccard={jaccard:.3}")],
                });
                continue;
            }

            // T3 : sémantique — cosine du corps, croisé Jaccard (anti faux-positif).
            let cos = match (&a.embedding, &b.embedding) {
                // Comparaison seulement entre embeddings du même modèle.
                (Some(va), Some(vb)) if a.embedder_id == b.embedder_id => dedup::cosine(va, vb),
                _ => continue,
            };
            if cos >= th.cosine_review && jaccard >= th.semantic_jaccard_floor {
                out.push(AuditCandidate {
                    note_id: cand.id.clone(),
                    section: cand.section.clone(),
                    title: cand.title.clone(),
                    category: DedupCategory::SemanticDuplicate,
                    tier: DedupCategory::SemanticDuplicate.default_tier(),
                    confidence: cos.min(0.99),
                    survivor_id: Some(survivor.id.clone()),
                    signals: vec![format!("cosine={cos:.3}"), format!("jaccard={jaccard:.3}")],
                });
            }
            // cosine élevé + jaccard bas = même sujet, contenu différent → volontairement ignoré.
        }
    }
    out
}

/// Assembles an [`AuditReport`] from the detected candidates (deterministic aggregates).
#[must_use]
pub fn build_report(
    vault_id: &str,
    generated_at_ms: i64,
    scanned: usize,
    candidates: Vec<AuditCandidate>,
    irrelevant: Vec<IrrelevantCandidate>,
) -> AuditReport {
    let mut counts_by_category: BTreeMap<String, usize> = BTreeMap::new();
    let mut counts_by_tier: BTreeMap<String, usize> = BTreeMap::new();
    for c in &candidates {
        *counts_by_category
            .entry(c.category.as_str().to_string())
            .or_insert(0) += 1;
        *counts_by_tier
            .entry(c.tier.as_str().to_string())
            .or_insert(0) += 1;
    }
    // Défaut cohérent : la fenêtre est couverte ssi au moins un candidat est actionnable
    // (le flag actionable est uniforme, issu de `window_covered`). L'exécuteur `audit_once`
    // écrase `downgrade_enabled`/`downgrade_window_covered` avec les valeurs autoritatives.
    let downgrade_window_covered = irrelevant.iter().any(|c| c.actionable);
    AuditReport {
        vault_id: vault_id.to_string(),
        generated_at_ms,
        scanned,
        counts_by_category,
        counts_by_tier,
        candidates,
        irrelevant,
        downgrade_actions: Vec::new(),
        downgrade_enabled: false,
        downgrade_window_covered,
    }
}

/// Renders the report as operator-readable Markdown.
#[must_use]
pub fn render_markdown(report: &AuditReport) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "# Vault audit report — F-51 (Option A, dry-run)\n\n\
         - vault: `{}`\n- generated: {} (epoch ms)\n- notes scanned: {}\n\n",
        report.vault_id, report.generated_at_ms, report.scanned
    ));
    s.push_str("## Counters by category\n\n");
    for (k, v) in &report.counts_by_category {
        s.push_str(&format!("- {k} : {v}\n"));
    }
    s.push_str("\n## Counters by tier\n\n");
    for (k, v) in &report.counts_by_tier {
        s.push_str(&format!("- {k} : {v}\n"));
    }
    s.push_str("\n## Candidates\n\n| note_id | section | category | tier | conf | survivor |\n");
    s.push_str("|---|---|---|---|---|---|\n");
    for c in &report.candidates {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {:.2} | {} |\n",
            c.note_id,
            c.section,
            c.category.as_str(),
            c.tier.as_str(),
            c.confidence,
            c.survivor_id.as_deref().unwrap_or("—"),
        ));
    }
    render_downgrade_section(&mut s, report);
    s
}

/// Renders the downgrade section: candidates, window guard and executed actions.
///
/// Labels are derived from explicit report fields (`downgrade_enabled`,
/// `downgrade_window_covered`, `candidate.actionable`), never inferred from
/// `downgrade_actions.is_empty()`.
fn render_downgrade_section(s: &mut String, report: &AuditReport) {
    s.push_str("\n## Downgrade candidates (F-111 graded forgetting)\n\n");
    let window = if report.downgrade_window_covered {
        "YES"
    } else {
        "NO"
    };
    s.push_str(&format!("- collection window covered: {window}\n\n"));
    s.push_str("| note_id | section | reason | actionable |\n");
    s.push_str("|---|---|---|---|\n");
    for c in &report.irrelevant {
        let flag = if c.actionable {
            "ACTIONABLE"
        } else {
            "NON-ACTIONABLE"
        };
        s.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            c.note_id, c.section, c.reason, flag
        ));
    }
    s.push_str("\n### Executed actions\n\n");
    if !report.downgrade_enabled {
        s.push_str("DRY-RUN — no action (flag `[downgrade] enabled=false`).\n");
    } else if report.downgrade_actions.is_empty() {
        s.push_str("No action (flag active but 0 actionable candidate).\n");
    } else {
        s.push_str("| note_id | outcome | reason |\n|---|---|---|\n");
        for a in &report.downgrade_actions {
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                a.note_id, a.outcome, a.reason
            ));
        }
    }
}

/// Emits ready-to-run `gradatum-admin` commands for the `Delete` candidates.
///
/// This function itself mutates nothing: it **prepares** the commands the operator will
/// run (a delete is an archival, admin CLI only). `Review` candidates are listed as
/// comments, with no command attached — nothing is ever archived automatically.
///
/// **This is not a whole-job invariant.** The audit job in
/// `gradatum-server::audit_job` does mutate when `[audit.downgrade] enabled = true`: it
/// calls `Downgrader::downgrade` on actionable candidates (capped by `max_per_run`).
/// Dry-run, the default, performs zero mutation. Only the *archival* path is
/// operator-driven; downgrades are not.
#[must_use]
pub fn render_admin_commands(report: &AuditReport) -> String {
    let mut s = String::new();
    s.push_str("# F-51 — prepared commands (to be run by the operator, never by an agent)\n");
    s.push_str(
        "# delete = archiving (F-100); reversible via `gradatum-admin archives restore`.\n\n",
    );
    for c in &report.candidates {
        match c.tier {
            AuditTier::Delete => s.push_str(&format!(
                "gradatum-admin delete {}  # {} conf={:.2}\n",
                c.note_id,
                c.category.as_str(),
                c.confidence
            )),
            AuditTier::Review => s.push_str(&format!(
                "# REVIEW (manual, no auto command): {} [{}]\n",
                c.note_id,
                c.category.as_str()
            )),
        }
    }
    s
}

// ── F-111 : axe pertinence (oubli gradué C5) — détection pure ──

/// Thresholds for the irrelevance rule (graduated forgetting).
#[derive(Debug, Clone)]
pub struct IrrelevanceThresholds {
    /// Minimum note age (days) before a note can be a candidate. Default 90.
    pub age_min_days: u32,
    /// Trust strictly below this value qualifies. Default 0.6.
    pub trust_max: f64,
    /// Usage observation window (days). Default 30.
    pub usage_window_days: u32,
}

impl Default for IrrelevanceThresholds {
    fn default() -> Self {
        Self {
            age_min_days: 90,
            trust_max: 0.6,
            usage_window_days: 30,
        }
    }
}

/// Per-note input for irrelevance detection (projected view, storage-free).
#[derive(Debug, Clone)]
pub struct IrrelevanceInput {
    /// ULID of the note.
    pub note_id: String,
    /// Kebab-case section.
    pub section: String,
    /// Note title (carried into the report / reason).
    pub title: String,
    /// Lifecycle status. Only `"live"` notes are eligible — the upstream SQL scan only
    /// excludes `('downgraded','garbage')`, so `staging` / `pending-review` still reach
    /// here and must be filtered out by the rule itself.
    pub status: String,
    /// Note creation timestamp (epoch ms).
    pub created_ms: i64,
    /// Trust value if known (absent → 0.5 neutral, which QUALIFIES under the 0.6 default).
    pub trust: Option<f64>,
    /// Most recent usage event (epoch ms), from `note_usage.last_used_ms` MAX across kinds.
    /// `None` = never used since collection started.
    pub last_used_ms: Option<i64>,
}

/// A downgrade candidate produced by the irrelevance rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrrelevantCandidate {
    /// ULID of the candidate note.
    pub note_id: String,
    /// Kebab-case section.
    pub section: String,
    /// Note title.
    pub title: String,
    /// Human-readable reason, e.g. `"0 usage/30d, trust 0.5, age 210d"`.
    pub reason: String,
    /// `true` only when the usage-collection window is covered — see [`window_covered`].
    pub actionable: bool,
}

/// Collection-window guard: `true` iff usage history covers `window_days`.
///
/// `t0_ms` is `MIN(note_usage.last_used_ms)` (proxy for the start of usage
/// collection). When the table is empty (`None`) or the elapsed time since `t0`
/// is shorter than the window, the "0 usage" signal is untrustworthy and every
/// candidate is marked non-actionable.
#[must_use]
pub fn window_covered(t0_ms: Option<i64>, now_ms: i64, window_days: u32) -> bool {
    match t0_ms {
        Some(t0) => now_ms - t0 >= i64::from(window_days) * 86_400_000,
        None => false,
    }
}

/// Conjunctive irrelevance rule — pure and deterministic (oldest first, then ULID).
///
/// A note is a candidate iff ALL hold: `status == "live"`, section ∉ `protected`,
/// age > `age_min_days`, usage within the window is zero (`last_used_ms` absent or
/// older than the window), and `trust` (defaulting to 0.5) < `trust_max`.
/// `actionable` mirrors [`window_covered`] — candidates are still listed when the
/// window is uncovered, but the executor must treat them as inert.
///
/// The output is sorted oldest-first (ascending `created_ms`), then by ULID, using a
/// pair-sort so that the comparator never re-scans `inputs`.
#[must_use]
pub fn detect_irrelevant(
    inputs: &[IrrelevanceInput],
    th: &IrrelevanceThresholds,
    protected: &[&str],
    t0_ms: Option<i64>,
    now_ms: i64,
) -> Vec<IrrelevantCandidate> {
    let actionable = window_covered(t0_ms, now_ms, th.usage_window_days);
    let age_floor_ms = i64::from(th.age_min_days) * 86_400_000;
    let usage_floor_ms = now_ms - i64::from(th.usage_window_days) * 86_400_000;

    let mut pairs: Vec<(i64, IrrelevantCandidate)> = inputs
        .iter()
        .filter(|n| n.status == "live")
        .filter(|n| !protected.contains(&n.section.as_str()))
        .filter(|n| now_ms - n.created_ms > age_floor_ms)
        .filter(|n| n.last_used_ms.is_none_or(|ms| ms < usage_floor_ms))
        .filter(|n| n.trust.unwrap_or(0.5) < th.trust_max)
        .map(|n| {
            let age_days = (now_ms - n.created_ms) / 86_400_000;
            let trust_str = n.trust.map_or("n/a".to_string(), |t| format!("{t:.1}"));
            (
                n.created_ms,
                IrrelevantCandidate {
                    note_id: n.note_id.clone(),
                    section: n.section.clone(),
                    title: n.title.clone(),
                    reason: format!(
                        "0 usage/{}d, trust {}, age {}d",
                        th.usage_window_days, trust_str, age_days
                    ),
                    actionable,
                },
            )
        })
        .collect();
    // Pair-sort (P2-4) : plus vieux d'abord, puis ULID — pas de find linéaire.
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.note_id.cmp(&b.1.note_id)));
    pairs.into_iter().map(|(_, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, section: &str, title: &str, body: &str) -> AuditRecord {
        AuditRecord {
            id: id.to_string(),
            section: section.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            author_id: None,
            embedding: None,
            embedder_id: None,
        }
    }

    fn rec_auth(id: &str, section: &str, title: &str, body: &str, author: &str) -> AuditRecord {
        AuditRecord {
            author_id: Some(author.to_string()),
            ..rec(id, section, title, body)
        }
    }

    fn rec_emb(id: &str, title: &str, body: &str, emb: Vec<f32>) -> AuditRecord {
        AuditRecord {
            id: id.to_string(),
            section: "debug".to_string(),
            title: title.to_string(),
            body: body.to_string(),
            author_id: None,
            embedding: Some(emb),
            embedder_id: Some("bge-small".to_string()),
        }
    }

    // ── F-111 : détection irrelevance (règle conjonctive pure) ──
    const NOW: i64 = 1_784_200_000_000;
    const T0_OK: Option<i64> = Some(NOW - 45 * 86_400_000); // fenêtre 30j couverte

    fn irr_input(
        id: &str,
        section: &str,
        age_days: i64,
        trust: Option<f64>,
        last_used_age_days: Option<i64>,
    ) -> IrrelevanceInput {
        IrrelevanceInput {
            note_id: id.to_string(),
            section: section.to_string(),
            title: format!("titre-{id}"),
            status: "live".to_string(),
            created_ms: NOW - age_days * 86_400_000,
            trust,
            last_used_ms: last_used_age_days.map(|d| NOW - d * 86_400_000),
        }
    }

    // P1-2 : status non-live exclut (staging/pending-review passent le SQL amont)
    #[test]
    fn irrelevant_non_live_status_excludes() {
        let mut input = irr_input("01S", "debug", 210, Some(0.5), None);
        input.status = "staging".to_string();
        let out = detect_irrelevant(
            &[input],
            &IrrelevanceThresholds::default(),
            &["council"],
            T0_OK,
            NOW,
        );
        assert!(out.is_empty());
    }

    // Candidat nominal : vieux, jamais utilisé, trust bas
    #[test]
    fn irrelevant_detects_old_unused_low_trust() {
        let inputs = vec![irr_input("01A", "debug", 210, Some(0.5), None)];
        let out = detect_irrelevant(
            &inputs,
            &IrrelevanceThresholds::default(),
            &["council"],
            T0_OK,
            NOW,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].actionable);
        assert_eq!(out[0].reason, "0 usage/30d, trust 0.5, age 210d");
    }

    // Chaque condition exclut isolément
    #[test]
    fn irrelevant_each_condition_excludes() {
        let th = IrrelevanceThresholds::default();
        let prot = ["council"];
        // trop jeune (89j)
        assert!(
            detect_irrelevant(
                &[irr_input("01B", "debug", 89, Some(0.5), None)],
                &th,
                &prot,
                T0_OK,
                NOW
            )
            .is_empty()
        );
        // usage récent (5j < fenêtre 30j)
        assert!(
            detect_irrelevant(
                &[irr_input("01C", "debug", 210, Some(0.5), Some(5))],
                &th,
                &prot,
                T0_OK,
                NOW
            )
            .is_empty()
        );
        // trust haut (0.9 ≥ 0.6)
        assert!(
            detect_irrelevant(
                &[irr_input("01D", "debug", 210, Some(0.9), None)],
                &th,
                &prot,
                T0_OK,
                NOW
            )
            .is_empty()
        );
        // section protégée
        assert!(
            detect_irrelevant(
                &[irr_input("01E", "council", 210, Some(0.5), None)],
                &th,
                &prot,
                T0_OK,
                NOW
            )
            .is_empty()
        );
    }

    // Usage ancien (last_used avant la fenêtre) = candidat quand même
    #[test]
    fn irrelevant_stale_usage_counts_as_zero() {
        let out = detect_irrelevant(
            &[irr_input("01F", "debug", 210, Some(0.5), Some(40))], // utilisé il y a 40j > fenêtre 30j
            &IrrelevanceThresholds::default(),
            &["council"],
            T0_OK,
            NOW,
        );
        assert_eq!(out.len(), 1);
    }

    // trust absent = 0.5 (< 0.6 défaut → candidat)
    #[test]
    fn irrelevant_missing_trust_defaults_low() {
        let out = detect_irrelevant(
            &[irr_input("01G", "debug", 210, None, None)],
            &IrrelevanceThresholds::default(),
            &["council"],
            T0_OK,
            NOW,
        );
        assert_eq!(out.len(), 1);
    }

    // Garde fenêtre : T0 absent OU trop récent ⇒ candidats non-actionnables
    #[test]
    fn irrelevant_window_guard_marks_non_actionable() {
        let inputs = vec![irr_input("01H", "debug", 210, Some(0.5), None)];
        let th = IrrelevanceThresholds::default();
        let out = detect_irrelevant(&inputs, &th, &["council"], None, NOW); // table vide
        assert_eq!(out.len(), 1);
        assert!(!out[0].actionable);
        let t0_recent = Some(NOW - 10 * 86_400_000); // 10j < 30j
        let out = detect_irrelevant(&inputs, &th, &["council"], t0_recent, NOW);
        assert!(!out[0].actionable);
        assert!(window_covered(T0_OK, NOW, 30));
        assert!(!window_covered(None, NOW, 30));
    }

    // Tri déterministe : plus vieux d'abord
    #[test]
    fn irrelevant_sorted_oldest_first() {
        let inputs = vec![
            irr_input("01Z", "debug", 100, Some(0.5), None),
            irr_input("01Y", "debug", 300, Some(0.5), None),
        ];
        let out = detect_irrelevant(
            &inputs,
            &IrrelevanceThresholds::default(),
            &["council"],
            T0_OK,
            NOW,
        );
        assert_eq!(out[0].note_id, "01Y");
    }

    // F-111 : le rapport transporte les candidats irrelevance + serde backward-compat
    #[test]
    fn report_carries_irrelevant_and_renders_markdown() {
        let cand = IrrelevantCandidate {
            note_id: "01A".into(),
            section: "debug".into(),
            title: "vieille note".into(),
            reason: "0 usage/30d, trust 0.5, age 210d".into(),
            actionable: false,
        };
        let report = build_report("main", 1_784_200_000_000, 10, vec![], vec![cand]);
        assert_eq!(report.irrelevant.len(), 1);
        assert!(report.downgrade_actions.is_empty());
        let md = render_markdown(&report);
        assert!(md.contains("Downgrade candidates"));
        assert!(md.contains("0 usage/30d"));
        assert!(md.contains("NON-ACTIONABLE") || md.contains("actionable: no"));
        assert!(md.contains("DRY-RUN"));
    }

    // Ancien JSON (sans les nouveaux champs) se désérialise encore
    #[test]
    fn report_json_backward_compatible() {
        let report = build_report("main", 1, 0, vec![], vec![]);
        let mut v: serde_json::Value = serde_json::to_value(&report).unwrap();
        v.as_object_mut().unwrap().remove("irrelevant");
        v.as_object_mut().unwrap().remove("downgrade_actions");
        let back: AuditReport = serde_json::from_value(v).expect("backward compat");
        assert!(back.irrelevant.is_empty());
    }

    // T0 — Catégorie C du corpus (trio smoke-alpha15 corps identique).
    #[test]
    fn exact_duplicates_flag_all_but_oldest_survivor() {
        let recs = vec![
            rec(
                "01A",
                "debug",
                "smoke-alpha15",
                "# smoke test alpha.15\n\nDeploy.",
            ),
            rec(
                "01B",
                "debug",
                "smoke-alpha15",
                "# smoke test alpha.15\n\nDeploy.",
            ),
            rec(
                "01C",
                "debug",
                "smoke-alpha15",
                "# smoke test alpha.15\n\nDeploy.",
            ),
        ];
        let cands = detect(&recs, &AuditThresholds::default());
        let exact: Vec<_> = cands
            .iter()
            .filter(|c| c.category == DedupCategory::ExactDuplicate)
            .collect();
        assert_eq!(exact.len(), 2, "2 doublons, 1 survivant");
        assert!(
            exact
                .iter()
                .all(|c| c.survivor_id.as_deref() == Some("01A"))
        );
        assert!(exact.iter().all(|c| c.tier == AuditTier::Delete));
    }

    // T1 — Catégories A/E (sonde canary + note vide) en section debug.
    #[test]
    fn structural_junk_flags_empty_and_probe_in_debug() {
        let recs = vec![
            rec("01A", "debug", "smoke occurred_at zzocctok", ""),
            rec("01B", "debug", "tagprobe status:open", "tag probe"),
        ];
        let cands = detect(&recs, &AuditThresholds::default());
        assert_eq!(cands.len(), 2);
        assert!(
            cands
                .iter()
                .all(|c| c.category == DedupCategory::StructuralJunk)
        );
        assert!(cands.iter().all(|c| c.tier == AuditTier::Delete));
    }

    // Garde-fou : la même sonde courte HORS debug n'est PAS flaggée par T1.
    #[test]
    fn structural_junk_does_not_fire_outside_debug() {
        let recs = vec![rec("01A", "reference", "probe", "probe")];
        let cands = detect(&recs, &AuditThresholds::default());
        assert!(
            cands.is_empty(),
            "T1 ne s'applique qu'à la section debug (précision-first)"
        );
    }

    // T2a — Catégorie F (débris migration TODO, titre-collision) → tier Review.
    #[test]
    fn title_collision_is_review_tier_not_delete() {
        let recs = vec![
            rec(
                "01A",
                "debug",
                "[DEBUG] Description — 2026-05-29",
                "external-agent ci fix bla bla longtext",
            ),
            rec(
                "01B",
                "debug",
                "[DEBUG] Description — 2026-05-29",
                "llm-commons content block autre longtext",
            ),
        ];
        let cands = detect(&recs, &AuditThresholds::default());
        let tc: Vec<_> = cands
            .iter()
            .filter(|c| c.category == DedupCategory::TitleCollision)
            .collect();
        assert_eq!(tc.len(), 1, "1 candidat (l'autre = survivant)");
        assert_eq!(tc[0].tier, AuditTier::Review);
        assert_eq!(tc[0].survivor_id.as_deref(), Some("01A"));
    }

    // T2b — near-dup lexical : corps long quasi-identique (un mot ajouté), titre non-sonde
    // pour isoler T2b de T1, corps > seuil probe pour ne pas déclencher le déchet structurel.
    #[test]
    fn near_duplicate_lexical_flags_quasi_identical_bodies() {
        let base: String = (1..=40).map(|n| format!("mot{n} ")).collect();
        let variant = format!("{base}extra");
        let recs = vec![
            rec("01A", "architecture", "Note archi longue A", &base),
            rec("01B", "architecture", "Note archi longue B", &variant),
        ];
        let cands = detect(&recs, &AuditThresholds::default());
        let nd: Vec<_> = cands
            .iter()
            .filter(|c| c.category == DedupCategory::NearDuplicate)
            .collect();
        assert_eq!(nd.len(), 1, "1 near-dup (l'autre = survivant)");
        assert_eq!(nd[0].tier, AuditTier::Delete);
        assert_eq!(nd[0].survivor_id.as_deref(), Some("01A"));
    }

    // T3 — sémantique : cosine haut + jaccard haut → Review ; cosine haut + jaccard bas → ignoré.
    #[test]
    fn semantic_requires_lexical_cross_check() {
        // Deux corps lexicalement proches ET vecteurs identiques → SemanticDuplicate (ou NearDuplicate).
        let close = vec![
            rec_emb(
                "01A",
                "a",
                "the quick brown fox jumps over the lazy dog again",
                vec![1.0, 0.0, 0.0],
            ),
            rec_emb(
                "01B",
                "b",
                "the quick brown fox jumps over the lazy dog again now",
                vec![1.0, 0.0, 0.0],
            ),
        ];
        let c1 = detect(&close, &AuditThresholds::default());
        assert!(
            !c1.is_empty(),
            "corps proches + vecteurs identiques → candidat"
        );

        // Vecteurs identiques MAIS corps lexicalement disjoints → PAS de candidat sémantique.
        let disjoint = vec![
            rec_emb(
                "01A",
                "a",
                "alpha beta gamma delta epsilon zeta eta theta",
                vec![1.0, 0.0, 0.0],
            ),
            rec_emb(
                "01B",
                "b",
                "kappa lambda mu nu xi omicron pi rho sigma",
                vec![1.0, 0.0, 0.0],
            ),
        ];
        let c2 = detect(&disjoint, &AuditThresholds::default());
        assert!(
            c2.iter()
                .all(|c| c.category != DedupCategory::SemanticDuplicate),
            "cosine seul ne suffit pas : jaccard bas = même sujet ≠ doublon (faux-positif évité)"
        );
    }

    // Régression P1-A : deux corps DISTINCTS de < shingle_k mots (shingles vides → signatures
    // all-MAX collisionnant à jaccard=1.0) ne doivent JAMAIS être flaggés near-dup. Hors debug
    // pour isoler du garde structurel T1. Preuve revue de code : jaccard("trust dynamique","voir adr")==1.0.
    #[test]
    fn short_distinct_bodies_are_not_near_duplicates() {
        let recs = vec![
            rec("01A", "reference", "Trust dynamique", "trust dynamique"),
            rec("01B", "architecture", "Voir ADR", "voir adr"),
        ];
        let cands = detect(&recs, &AuditThresholds::default());
        assert!(
            cands.is_empty(),
            "corps courts distincts (<3 mots) → shingles vides → aucune near-dup (P1-A)"
        );
    }

    // Réglage F-51 (cat B) : un smoke-deploy au corps descriptif > min_probe_bytes doit être
    // flaggé StructuralJunk malgré sa taille (le garde de taille faisait manquer 12/22 notes B).
    #[test]
    fn smoke_deploy_with_long_body_is_structural_junk() {
        let body = "Smoke test post-deploy 0.6.8 LIVE (server+worker sur l'hôte interne). \
                    Vérifie vault_write→read→search round-trip sur la nouvelle version. Note éphémère.";
        assert!(body.len() > AuditThresholds::default().min_probe_bytes);
        let recs = vec![rec(
            "01A",
            "debug",
            "[DEBUG][gradatum] smoke-deploy-v0.6.8 round-trip",
            body,
        )];
        let cands = detect(&recs, &AuditThresholds::default());
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].category, DedupCategory::StructuralJunk);
        assert_eq!(cands[0].tier, AuditTier::Delete);
    }

    // Réglage F-51 (cat D) : la famille post-idle (titre « … test <n> ») doit être captée.
    #[test]
    fn stress_test_family_is_detected_by_title_pattern() {
        let recs = vec![
            rec(
                "01A",
                "debug",
                "[DEBUG] post-idle test 1 — 2026-06-02",
                "post-idle test 1",
            ),
            rec(
                "01B",
                "debug",
                "[DEBUG] post-idle test 2 — 2026-06-02",
                "post-idle test 2",
            ),
        ];
        let cands = detect(&recs, &AuditThresholds::default());
        assert_eq!(cands.len(), 2);
        assert!(
            cands
                .iter()
                .all(|c| c.category == DedupCategory::StructuralJunk)
        );
    }

    // Réglage F-51 (cat A) : note générée par l'agent Tester en debug → StructuralJunk.
    #[test]
    fn tester_authored_short_debug_note_is_junk() {
        let recs = vec![rec_auth(
            "01A",
            "debug",
            "[DEBUG] Deuxième note test pour vérifier le dedup.",
            "Deuxième note test pour vérifier le dedup.",
            "tester",
        )];
        let cands = detect(&recs, &AuditThresholds::default());
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].category, DedupCategory::StructuralJunk);
        // Garde-fou : auteur tester HORS debug → jamais flaggé.
        let outside = vec![rec_auth(
            "01B",
            "experiments",
            "run éval",
            "run éval tester",
            "tester",
        )];
        assert!(detect(&outside, &AuditThresholds::default()).is_empty());
    }

    // Réglage F-51 (cat F → delete) : deux notes de MÊME titre au boilerplate quasi-identique
    // (jaccard ≥ seuil) restent en tier Review (collision de titre), jamais promues NearDup/Delete.
    #[test]
    fn same_title_family_stays_review_not_delete() {
        // Corps ~93 % identiques (boilerplate migration), titre identique.
        let boiler = "## Description : item. ## Contexte : migration 2026-05-29 owner main-agent. \
                      ## État : OPEN P2. ## Historique : CREATED OPEN par migration.";
        let recs = vec![
            rec(
                "01A",
                "debug",
                "[DEBUG] Description — 2026-05-29",
                &format!("{boiler} external-agent ci"),
            ),
            rec(
                "01B",
                "debug",
                "[DEBUG] Description — 2026-05-29",
                &format!("{boiler} llm commons"),
            ),
        ];
        let cands = detect(&recs, &AuditThresholds::default());
        assert!(
            cands.iter().all(|c| c.tier == AuditTier::Review),
            "famille de même titre → Review, jamais Delete (cat-F fix)"
        );
        assert!(
            cands
                .iter()
                .all(|c| c.category == DedupCategory::TitleCollision)
        );
    }

    // Contrôle négatif : notes distinctes et légitimes → zéro candidat.
    #[test]
    fn distinct_notes_yield_no_candidates() {
        let recs = vec![
            rec(
                "01A",
                "debug",
                "Fix Forgejo Actions",
                "Fix Forgejo Actions bug gradatum-www gitea namespace commit acc2860 réparé",
            ),
            rec(
                "01B",
                "reference",
                "REALFEEDBACK",
                "REX public llama.cpp gateway sur AMD field notes tableau modèles",
            ),
        ];
        let cands = detect(&recs, &AuditThresholds::default());
        assert!(
            cands.is_empty(),
            "aucun faux positif sur des notes distinctes légitimes"
        );
    }

    #[test]
    fn report_aggregates_and_sorts_by_severity() {
        let recs = vec![
            rec("01A", "debug", "smoke-alpha15", "identical body here now"),
            rec("01B", "debug", "smoke-alpha15", "identical body here now"),
            rec("01C", "debug", "probe", ""),
        ];
        let cands = detect(&recs, &AuditThresholds::default());
        let report = build_report("main", 1234, recs.len(), cands, vec![]);
        assert_eq!(report.scanned, 3);
        // Le plus sévère (ExactDuplicate) vient en tête.
        assert_eq!(report.candidates[0].category, DedupCategory::ExactDuplicate);
        assert!(report.counts_by_tier.get("delete").copied().unwrap_or(0) >= 1);
        let md = render_markdown(&report);
        assert!(md.contains("Vault audit report"));
        let cmds = render_admin_commands(&report);
        assert!(cmds.contains("gradatum-admin delete"));
    }
}
