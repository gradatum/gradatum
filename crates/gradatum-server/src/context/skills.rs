//! Skill injection for the LLM context assembly pipeline.
//!
//! Ranks notes from the `"skills"` section by cosine similarity with the `query_embedding`
//! reused from the retrieval step — **zero additional embed calls**.
//!
//! ## Flux
//!
//! ```text
//! assemble_assembled (inject_skills=true)
//!     │
//!     ├─ query_embedding = None (Noop / timeout / fallback)
//!     │       └─ skip silencieux (tracing::debug!)
//!     │
//!     └─ query_embedding = Some(qemb)
//!             │
//!         get_or_build_skill_index (lazy, dans mod.rs)
//!             │
//!         rank_skills(qemb, max=3)   ← cosine, zéro embed
//!             │
//!         inject_skills_header(sub_budget=budget/4 max 64)
//!             │
//!         prepend header à assembled_text
//! ```
//!
//! ## Note sur `skill_query`
//!
//! Le champ `VaultContextRequest.skill_query` est accepté (désérialisé) mais **ignoré
//! pour le ranking** : le ranking réutilise `query_embedding` (embedding de la requête
//! principale) afin de rester à zéro embed supplémentaire. Un filtre lexical léger sur
//! `skill_query` pourrait être ajouté dans une version ultérieure sans changer le contrat.

use gradatum_core::error::GradatumError;
use gradatum_core::index::Index;
use gradatum_embed::Embedder;

use crate::context::tokens::TokenEstimator;

// ── Types ─────────────────────────────────────────────────────────────────────

/// Entrée unitaire du [`SkillIndex`] — note de section `"skills"` avec son embedding.
pub struct SkillEntry {
    /// ULID de la note (format string).
    pub ulid: String,
    /// Titre Markdown H1 de la note (extrait par le curator).
    pub title: String,
    /// Corps complet Markdown (`body_text` SQLite, incluant le titre H1 si présent).
    pub body: String,
    /// Embedding du corps précalculé via `embed_batch` au moment du lazy build.
    pub embedding: Vec<f32>,
}

/// Index en mémoire des notes de section `"skills"`.
///
/// Construit paresseusement lors du premier appel `vault_context` avec `inject_skills=true`
/// (via `get_or_build_skill_index` dans `context/mod.rs`). Stocké dans `AppState.skills_index`
/// comme `Arc<SkillIndex>` pour extraction hors guard sans copie.
///
/// # Invalidation
///
/// Pas de hook `vault_write` section `"skills"` pour l'instant.
/// Le cache est rebuilt uniquement si `None` (démarrage ou si le précédent build a échoué).
///
/// # ECON: liste plate, scan max 200 notes
/// Acceptable pour un petit nombre de skills. Upgrade → invalidation incrémentale
/// ou pagination si le corpus skills dépasse ~200 notes.
pub struct SkillIndex {
    /// Skills ordonnés par ordre de scan (le ranking se fait à l'appel).
    pub entries: Vec<SkillEntry>,
}

// ── Cosine ────────────────────────────────────────────────────────────────────

/// Cosine similarity entre deux vecteurs `f32`.
///
/// Retourne `0.0` si vecteurs vides, dimensions incompatibles, ou norme nulle.
/// Garantit l'absence de panic quelle que soit l'entrée.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

// ── Ranking ───────────────────────────────────────────────────────────────────

/// Range les skills par cosine décroissant avec `query_emb`, tronqué à `max_skills`.
///
/// ## Zéro embed
///
/// Réutilise `query_emb` fourni (embedding de la requête principale calculé par
/// the retrieval) — no additional embedder calls.
///
/// ## Robustesse
///
/// Dimensions incompatibles entre `query_emb` et une entrée → score `0.0` pour cette
/// entrée (pas de panic, pas de skip — l'entrée reste dans le résultat avec score nul).
/// Vecteurs nuls → score `0.0`.
pub fn rank_skills<'a>(
    index: &'a SkillIndex,
    query_emb: &[f32],
    max_skills: usize,
) -> Vec<&'a SkillEntry> {
    if max_skills == 0 || query_emb.is_empty() {
        return vec![];
    }

    let mut scored: Vec<(f32, &SkillEntry)> = index
        .entries
        .iter()
        .map(|e| (cosine_similarity(query_emb, &e.embedding), e))
        .collect();

    // Tri décroissant par score cosine. `partial_cmp` gère NaN avec fallback Equal.
    scored.sort_unstable_by(|(a, _), (b, _)| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(max_skills);
    scored.into_iter().map(|(_, e)| e).collect()
}

// ── Génération header ─────────────────────────────────────────────────────────

/// Génère un en-tête Markdown de skills borné par `sub_budget` tokens.
///
/// Inclut les skills séquentiellement tant que le budget tient. S'arrête dès que
/// l'ajout d'un skill ferait dépasser `sub_budget` (pas de troncature partielle du skill).
///
/// # Retour
///
/// `(header_markdown, tokens_consommés)` — retourne `(String::new(), 0)` si :
/// - `skills` est vide,
/// - `sub_budget == 0`,
/// - ou l'en-tête intro seule dépasse le budget,
/// - ou aucun skill ne tient dans le budget résiduel.
///
/// # Format
///
/// ```text
/// ## Skills disponibles
///
/// ### <titre>
/// <body>
///
/// ```
pub fn inject_skills_header(
    skills: &[&SkillEntry],
    sub_budget: u32,
    est: &dyn TokenEstimator,
) -> (String, u32) {
    if skills.is_empty() || sub_budget == 0 {
        return (String::new(), 0);
    }

    let intro = "## Skills disponibles\n\n";
    let intro_tokens = est.estimate(intro);
    if intro_tokens > sub_budget {
        return (String::new(), 0);
    }

    let mut out = String::from(intro);
    let mut used = intro_tokens;
    let mut any_added = false;

    for skill in skills {
        // Format : titre H3 + corps + référence source wikilink (traçabilité LLM).
        let block = format!(
            "### {}\n{}\n\n— source: [[{}]]\n\n",
            skill.title, skill.body, skill.ulid
        );
        let block_tokens = est.estimate(&block);
        if used.saturating_add(block_tokens) > sub_budget {
            break;
        }
        out.push_str(&block);
        used = used.saturating_add(block_tokens);
        any_added = true;
    }

    if !any_added {
        return (String::new(), 0);
    }

    (out, used)
}

// ── Build index ───────────────────────────────────────────────────────────────

/// Construit l'index skills en scannant la section `"skills"` et en embeddant les bodies.
///
/// Appelé par `get_or_build_skill_index` (dans `context/mod.rs`) lors d'un cache miss.
///
/// ## Algorithme
///
/// 1. `search.list_notes(tenant, Some("skills"), 200, None)` — scan max 200 notes.
///    ECON: pas de pagination, acceptable pour un corpus de skills typique (< 200).
/// 2. Si aucune note, retourner `SkillIndex::empty()` sans appel embed.
/// 3. `embedder.embed_batch(bodies)` borné par `embed_timeout_ms` — un seul appel.
/// 4. Zip (NoteRecord, embedding) → `Vec<SkillEntry>`.
///
/// ## Dégradation gracieuse (P2-a)
///
/// Sur **timeout** (`embed_timeout_ms` dépassé) OU **erreur** `embed_batch` :
/// retourne `Ok(SkillIndex { entries: vec![] })` + `tracing::warn!`.
/// L'erreur embed n'est **pas** propagée — seule une erreur SQL (`list_notes`)
/// remonte à l'appelant (infrastructure indisponible).
///
/// Cette dégradation aligne `build_skill_index` sur le retrieval (`retrieve_candidates`)
/// qui borne également son embed et retourne BM25-only sur timeout (P2-3).
///
/// # Errors
///
/// - [`GradatumError::Storage`] : échec `list_notes` SQL uniquement.
pub async fn build_skill_index(
    tenant: &str,
    search: &dyn Index,
    embedder: &dyn Embedder,
    embed_timeout_ms: u64,
) -> Result<SkillIndex, GradatumError> {
    // ECON: scan complet max 200, pas de pagination.
    // Upgrade → pagination + invalidation incrémentale si corpus > 200 skills.
    let (records, _total) = search.list_notes(tenant, Some("skills"), 200, None).await?;

    if records.is_empty() {
        return Ok(SkillIndex { entries: vec![] });
    }

    // Embed batch borné par embed_timeout_ms — dégradation gracieuse sur timeout ou erreur.
    // Aligne sur le pattern du retrieval (P2-3) : pas de propagation d'erreur embed.
    let texts: Vec<&str> = records.iter().map(|r| r.body_text.as_str()).collect();
    let embed_dur = std::time::Duration::from_millis(embed_timeout_ms);
    let embeddings = match tokio::time::timeout(embed_dur, embedder.embed_batch(&texts)).await {
        Ok(Ok(embs)) => embs,
        Ok(Err(e)) => {
            tracing::warn!(
                err = %e,
                tenant,
                skills_count = texts.len(),
                "skills: embed_batch error — dégradation gracieuse, index vide"
            );
            return Ok(SkillIndex { entries: vec![] });
        }
        Err(_timeout) => {
            tracing::warn!(
                embed_timeout_ms,
                tenant,
                skills_count = texts.len(),
                "skills: embed_batch timeout — dégradation gracieuse, index vide"
            );
            return Ok(SkillIndex { entries: vec![] });
        }
    };

    let entries = records
        .into_iter()
        .zip(embeddings)
        .map(|(r, emb)| SkillEntry {
            ulid: r.id,
            title: r.title.unwrap_or_else(|| "(sans titre)".to_string()),
            body: r.body_text,
            embedding: emb,
        })
        .collect();

    Ok(SkillIndex { entries })
}

// ── Tests unitaires ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Estimateur de tokens fixe pour les tests (chaque appel retourne la même valeur).
    struct FixedEstimator(u32);
    impl TokenEstimator for FixedEstimator {
        fn estimate(&self, _: &str) -> u32 {
            self.0
        }
    }

    fn make_entry(ulid: &str, body_emb: Vec<f32>) -> SkillEntry {
        SkillEntry {
            ulid: ulid.to_string(),
            title: format!("Titre {ulid}"),
            body: format!("Corps {ulid}"),
            embedding: body_emb,
        }
    }

    // ── rank_skills ──────────────────────────────────────────────────────────

    #[test]
    fn rank_skills_sorts_by_cosine_descending() {
        let idx = SkillIndex {
            entries: vec![
                make_entry("a", vec![0.0, 1.0]),
                make_entry("b", vec![1.0, 0.0]),
            ],
        };
        // query proche de "b" [1.0, 0.0] → b classé devant a
        let ranked = rank_skills(&idx, &[1.0, 0.0], 2);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].ulid, "b");
        assert_eq!(ranked[1].ulid, "a");
    }

    #[test]
    fn rank_skills_truncates_to_max() {
        let idx = SkillIndex {
            entries: vec![
                make_entry("a", vec![1.0, 0.0]),
                make_entry("b", vec![1.0, 0.0]),
                make_entry("c", vec![1.0, 0.0]),
            ],
        };
        let ranked = rank_skills(&idx, &[1.0, 0.0], 2);
        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn rank_skills_dim_mismatch_gives_zero_score_no_panic() {
        let idx = SkillIndex {
            entries: vec![make_entry("x", vec![1.0, 0.0, 0.0])],
        };
        // query_emb dim=2 vs entry dim=3 → score 0.0, pas de panic
        let ranked = rank_skills(&idx, &[1.0, 0.0], 1);
        assert_eq!(ranked.len(), 1); // retourné avec score 0.0
    }

    #[test]
    fn rank_skills_empty_query_emb_returns_empty() {
        let idx = SkillIndex {
            entries: vec![make_entry("a", vec![1.0])],
        };
        let ranked = rank_skills(&idx, &[], 3);
        assert!(ranked.is_empty());
    }

    #[test]
    fn rank_skills_max_zero_returns_empty() {
        let idx = SkillIndex {
            entries: vec![make_entry("a", vec![1.0])],
        };
        let ranked = rank_skills(&idx, &[1.0], 0);
        assert!(ranked.is_empty());
    }

    // ── inject_skills_header ─────────────────────────────────────────────────

    #[test]
    fn inject_skills_header_empty_skills_returns_empty() {
        let est = FixedEstimator(5);
        let (h, t) = inject_skills_header(&[], 100, &est);
        assert!(h.is_empty());
        assert_eq!(t, 0);
    }

    #[test]
    fn inject_skills_header_budget_zero_returns_empty() {
        let est = FixedEstimator(5);
        let e = make_entry("u", vec![]);
        let (h, t) = inject_skills_header(&[&e], 0, &est);
        assert!(h.is_empty());
        assert_eq!(t, 0);
    }

    #[test]
    fn inject_skills_header_budget_fits_one_skill() {
        // intro → 5 tokens, block → 5 tokens, total → 10, budget=20 → les deux tiennent
        let est = FixedEstimator(5);
        let e = make_entry("u", vec![]);
        let (h, t) = inject_skills_header(&[&e], 20, &est);
        assert!(!h.is_empty(), "le header ne doit pas être vide");
        assert!(h.contains("## Skills disponibles"));
        assert!(h.contains("Titre u"));
        assert!(h.contains("Corps u"));
        assert!(
            h.contains("[[u]]"),
            "le header doit contenir la référence source [[ulid]]"
        );
        assert_eq!(t, 10, "intro(5) + block(5) = 10 tokens");
    }

    #[test]
    fn inject_skills_header_budget_too_tight_for_intro_returns_empty() {
        // intro = 5 tokens, budget = 3 → ne tient pas
        let est = FixedEstimator(5);
        let e = make_entry("u", vec![]);
        let (h, t) = inject_skills_header(&[&e], 3, &est);
        assert!(
            h.is_empty(),
            "le header doit être vide si intro dépasse le budget"
        );
        assert_eq!(t, 0);
    }

    #[test]
    fn inject_skills_header_stops_when_skill_overflows_budget() {
        // intro = 2 tokens, block = 10 tokens, budget = 5 → intro tient, block non → vide
        struct VarEstimator;
        impl TokenEstimator for VarEstimator {
            fn estimate(&self, text: &str) -> u32 {
                if text.contains("## Skills") { 2 } else { 10 }
            }
        }
        let e = make_entry("u", vec![]);
        let (h, t) = inject_skills_header(&[&e], 5, &VarEstimator);
        assert!(h.is_empty(), "aucun skill ne tient → header vide");
        assert_eq!(t, 0);
    }
}
