//! Tests `vault_trace` — parallélisation seeds via `tokio::task::JoinSet`.
//!
//! ## Cas couverts
//!
//! 1. `vault_trace_parallel_seeds_returns_same_as_serial`
//!    — N=3 seeds cycle A→B→C→A, les résultats parallèles == résultats attendus.
//! 2. `vault_trace_ulid_fast_path_unaffected`
//!    — Non-régression : ULID direct produit status 200 + entrées correctes
//!    (le fast-path résout 1 seed, l'implémentation JoinSet ne le brise pas).

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{build_app, call_vault_trace, sign_token};

// ── Helper local : construit N notes en cycle (A→B→C→...→A) et retourne leurs ids ──

/// Crée `n` notes reliées en cycle (0→1, 1→2, ..., n-1→0) via `upsert_link`.
///
/// Retourne `(app, token, ids)` où `ids[i]` est l'identifiant ULID String de la note i.
/// Chaque note a un body unique contenant `"vault_trace_parallel_seed_{i}"` pour
/// permettre un match FTS textuel.
async fn build_trace_test_state_with_links(n: usize) -> (helpers::TestEnv, String, Vec<String>) {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Créer n notes avec contenu FTS unique.
    let mut ids: Vec<String> = Vec::with_capacity(n);
    for i in 0..n {
        let title = format!("TraceParallelNote {i}");
        let body = format!("vault_trace_parallel_seed_{i} test content");
        let nid = env.write_note_with_h1(&title, &body).await;
        ids.push(nid.to_string());
    }

    // Créer cycle : ids[i] → ids[(i+1) % n]
    for i in 0..n {
        let src = &ids[i];
        let dst = &ids[(i + 1) % n];
        env.state
            .search
            .upsert_link("main", src, dst)
            .await
            .expect("upsert_link cycle");
    }

    (env, token, ids)
}

// ── Test 1 : parallélisation retourne les mêmes résultats qu'en mode série ──────

/// C2 — vault_trace avec N seeds FTS retourne les mêmes résultats qu'en mode série.
///
/// Vérifie que la parallélisation (JoinSet) ne change pas les résultats (non-régression).
/// Setup : 3 notes avec wikilinks cycle A→B→C→A.
/// Query textuelle "vault_trace_parallel_seed" → seeds FTS → trace_lineage parallèle.
///
/// Assertions :
/// - ids[1] présent dans les entries (enfant direct de ids[0])
/// - ids[2] présent dans les entries (parent de ids[1], ou enfant de ids[1])
/// - 3 entries total (cycle complet sans dédup inter-seeds)
#[tokio::test]
async fn vault_trace_parallel_seeds_returns_same_as_serial() {
    let (env, token, ids) = build_trace_test_state_with_links(3).await;

    // Query textuelle commune à toutes les notes du cycle → FTS top-3 seeds.
    // fts_limit = min(limit=10, 5) = 5, donc les 3 seeds sont incluses.
    let resp = call_vault_trace(
        env.app.clone(),
        &token,
        "vault_trace_parallel_seed",
        "main",
        20,
    )
    .await
    .expect("vault_trace parallèle 3 seeds doit retourner 200");

    let entries = resp["entries"]
        .as_array()
        .expect("entries doit être un tableau JSON");

    let returned_ids: std::collections::HashSet<&str> =
        entries.iter().filter_map(|e| e["path"].as_str()).collect();

    assert!(
        returned_ids.contains(ids[1].as_str()),
        "parallel vault_trace doit retourner ids[1] dans le lineage de ids[0] — returned={returned_ids:?}, ids={ids:?}"
    );
    assert!(
        returned_ids.contains(ids[2].as_str()),
        "parallel vault_trace doit retourner ids[2] dans le lineage de ids[1] — returned={returned_ids:?}, ids={ids:?}"
    );
    // Le cycle A→B→C→A via 3 seeds doit traverser exactement les 3 notes.
    assert_eq!(
        entries.len(),
        3,
        "3 seeds traversés en parallèle doivent retourner exactement 3 entries (cycle) — entries={entries:?}"
    );
}

// ── Test 2 : non-régression fast-path ULID ───────────────────────────────────

/// C2 — vault_trace avec 1 seed ULID direct ne régresse pas après refactor JoinSet.
///
/// Non-régression comportementale : le fast-path ULID (resolved_seeds = [ulid_str])
/// produit status 200 et retourne les entries attendues (enfant direct).
///
/// Note sur le mock counter :
/// `get_parallel_invocation_count` nécessiterait un AtomicUsize dans AppState ou un
/// wrapper de test autour du handler. Le test vérifie le comportement observable uniquement.
#[tokio::test]
async fn vault_trace_ulid_fast_path_unaffected() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Crée 1 note source + 1 note enfant reliées par un lien.
    let id_src = env
        .write_note_with_h1("TraceFastPathSource", "contenu source ulid fast path")
        .await;
    let id_child = env
        .write_note_with_h1("TraceFastPathChild", "contenu enfant ulid fast path")
        .await;

    env.state
        .search
        .upsert_link("main", &id_src.to_string(), &id_child.to_string())
        .await
        .expect("upsert_link src → child");

    // Query = ULID string → fast-path : resolved_seeds = [id_src] (1 seed).
    let resp = call_vault_trace(env.app.clone(), &token, &id_src.to_string(), "main", 10)
        .await
        .expect("vault_trace ULID fast-path doit retourner 200");

    let entries = resp["entries"]
        .as_array()
        .expect("entries doit être un tableau JSON");

    // L'enfant doit figurer dans le lineage.
    let paths: Vec<&str> = entries.iter().filter_map(|e| e["path"].as_str()).collect();
    assert!(
        paths.iter().any(|p| p.contains(&id_child.to_string())),
        "ULID fast-path : id_child manquant dans vault_trace — paths={paths:?}"
    );

    // Après refactor JoinSet, le fast-path (1 seed) doit toujours retourner status 200.
    // Le check comportemental ci-dessus est suffisant pour garantir la non-régression.
    // Le mock counter n'est pas asserté ici (voir commentaire ci-dessus).
}
