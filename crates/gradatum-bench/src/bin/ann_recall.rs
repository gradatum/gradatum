//! Bench recall ANN sqlite-vec (v0.5.3 ANN-1).
//!
//! ## Usage
//!
//! ```sh
//! cargo run -p gradatum-bench --bin ann_recall
//! ```
//!
//! ## Design
//!
//! Ce binary enregistre l'extension sqlite-vec avant l'ouverture de la DB.
//! L'enregistrement unsafe (`sqlite3_auto_extension`) ne peut pas être fait
//! depuis `gradatum-index` (qui a `#![forbid(unsafe_code)]`).
//!
//! Le seeding des notes + embeddings passe par les API publiques de `SqliteIndex`
//! (`seed_note` + `seed_note_embedding`) — sans accès direct au champ `conn`.
//!
//! Les requêtes ANN sont faites via `backfill_ann_index` (API publique) puis
//! en SQL direct sur `note_embeddings_ann` (vec0) via un appel `pragma` qui
//! retourne les résultats — ici on utilise `SqliteIndex::pragma` pour valider
//! la table ou un helper dédié si disponible.
//!
//! ## Scénario
//!
//! 1. Enregistre sqlite-vec via `sqlite3_auto_extension`.
//! 2. Ouvre une DB in-memory (migration 0020 s'applique via `SqliteIndex::open_in_memory`).
//! 3. Seed N notes avec `seed_note`, puis embeddings avec `seed_note_embedding`.
//! 4. Backfill ANN via `SqliteIndex::backfill_ann_index()`.
//! 5. Requête ANN top-K via SQL direct (vec0 MATCH) — utilise `SqliteIndex::search_ann_bench`.
//! 6. Brute-force top-K (ground truth).
//! 7. Calcule recall@K.
//!
//! ## Seuil recall
//!
//! recall@10 ≥ 0.90 pour N=500 vecteurs dim=1024 (HNSW sqlite-vec).
//!
//! ## Safety
//!
//! ```
//! // SAFETY: sqlite3_vec_init a la signature sqlite3_auto_extension standard :
//! // (sqlite3*, char**, sqlite3_api_routines*) -> int, identique à RawAutoExtension
//! // dans rusqlite. Le transmute est nécessaire car sqlite-vec expose la fn comme
//! // extern "C" fn() sans paramètres (déclaration conservative C) mais l'ABI est
//! // identique. Ref: sqlite-vec 0.1.9 src/lib.rs test + SQLite extension API.
//! ```

use rusqlite::ffi::sqlite3_auto_extension;
use sqlite_vec::sqlite3_vec_init;

use gradatum_index::SqliteIndex;

/// Nombre de notes seedées pour le bench recall.
const N_NOTES: usize = 500;

/// Top-K pour le bench recall.
const K: usize = 10;

/// Seuil minimal de recall@K attendu pour HNSW sqlite-vec.
const RECALL_THRESHOLD: f32 = 0.90;

/// Dimension des vecteurs (bge-m3).
const DIM: usize = 1024;

/// Génère un vecteur pseudo-aléatoire normalisé déterministe (dim=DIM).
///
/// Utilise un hash itératif sans dépendance externe. Qualité suffisante
/// pour un bench recall (pas pour prod).
fn pseudo_rand_unit_vec(seed: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(DIM);
    let mut state: u64 = seed as u64 ^ 0xdeadbeef_cafebabe;
    for _ in 0..DIM {
        // LCG xorshift simple.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let raw = (state as f32 / u64::MAX as f32) * 2.0 - 1.0;
        v.push(raw);
    }
    // Normalisation L2.
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

/// Cosine similarity entre deux vecteurs normalisés (= produit scalaire).
fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Étape 1 : enregistrer sqlite-vec ─────────────────────────────────────
    //
    // SAFETY: sqlite3_vec_init a la signature sqlite3_auto_extension standard :
    // (sqlite3*, char**, sqlite3_api_routines*) -> int, identique à RawAutoExtension.
    // Le transmute convertit `extern "C" fn()` (déclaration conservative) vers le
    // type de pointeur de fn requis par sqlite3_auto_extension. L'ABI C est identique.
    // Ref: sqlite-vec 0.1.9 src/lib.rs (test utilise le même transmute).
    #[expect(
        clippy::missing_transmute_annotations,
        reason = "type cible dépend de l'ABI rusqlite/libsqlite3-sys interne — \
                  annoter introduirait une dépendance fragile sur les types internes"
    )]
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(sqlite3_vec_init as *const ())));
    }
    println!("[bench-ann-recall] sqlite-vec enregistré.");

    // ── Étape 2 : ouvrir la DB (migration 0020 appliquée) ────────────────────
    let idx = SqliteIndex::open_in_memory().await?;
    println!("[bench-ann-recall] DB in-memory ouverte, migrations appliquées.");

    // ── Étape 3 : seed N notes avec embeddings dim=1024 ──────────────────────
    let vecs: Vec<Vec<f32>> = (0..N_NOTES).map(pseudo_rand_unit_vec).collect();
    for (i, vec) in vecs.iter().enumerate() {
        let note_id = format!("01BENCH{i:020}");
        idx.seed_note(&note_id, "decisions", "bench note").await?;
        idx.seed_note_embedding(&note_id, "bge-m3", DIM as u16, vec)
            .await?;
    }
    println!("[bench-ann-recall] Seeded {N_NOTES} notes avec embeddings dim=1024.");

    // ── Étape 4 : backfill ANN ───────────────────────────────────────────────
    let backfilled = idx.backfill_ann_index().await?;
    println!("[bench-ann-recall] Backfill ANN : {backfilled} notes insérées.");

    // ── Étape 5 : requête ANN top-K via SQL direct ───────────────────────────
    //
    // `search_ann_bench` expose le résultat ANN directement depuis SqliteIndex.
    let query = pseudo_rand_unit_vec(N_NOTES + 1); // Vecteur non seedé.
    let k_oversample = (K * 2).min(1024);

    let ann_ids = idx
        .search_ann_bench("main", "bge-m3", &query, k_oversample)
        .await?;
    let ann_set: std::collections::HashSet<String> = ann_ids.iter().take(K).cloned().collect();
    println!(
        "[bench-ann-recall] ANN top-{K} récupérés ({} candidats).",
        ann_ids.len()
    );

    // ── Étape 6 : brute-force ground truth ───────────────────────────────────
    let mut bf_scores: Vec<(String, f32)> = vecs
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let note_id = format!("01BENCH{i:020}");
            (note_id, cosine_sim(&query, v))
        })
        .collect();
    bf_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let bf_set: std::collections::HashSet<String> =
        bf_scores.iter().take(K).map(|(id, _)| id.clone()).collect();

    // ── Étape 7 : recall@K ───────────────────────────────────────────────────
    let intersection = ann_set.intersection(&bf_set).count();
    let recall = intersection as f32 / K as f32;

    println!("[bench-ann-recall] recall@{K} = {intersection}/{K} = {recall:.3}");
    if recall >= RECALL_THRESHOLD {
        println!("[bench-ann-recall] OK recall@{K} >= {RECALL_THRESHOLD:.2} (seuil atteint)");
    } else {
        eprintln!(
            "[bench-ann-recall] WARN recall@{K} = {recall:.3} < {RECALL_THRESHOLD:.2} \
             — ANN dégradé ou N trop faible"
        );
        std::process::exit(1);
    }

    Ok(())
}
