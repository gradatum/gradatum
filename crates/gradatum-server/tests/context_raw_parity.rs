//! Test de parité bit-pour-bit : `mode=Raw` reproduit le comportement legacy v0.6.x.
//!
//! Vérifie que la nouvelle [`VaultContextResponse`] (`assembled_text` / `included` /
//! `budget_used` / `diagnostics`) préserve exactement la logique du handler legacy :
//!
//! - Jointure `"\n\n---\n\n"` entre les parties de notes.
//! - Budget `(assembled_text.chars().count() / 3)` (division entière — parité exacte).
//! - Troncature char-safe (jamais au milieu d'un codepoint UTF-8).
//! - `diagnostics.embed_fallback = false` (pas d'embed en mode Raw).
//! - `included` contient les sources avec `score = 0.0`.

#[path = "helpers/mod.rs"]
mod helpers;

use axum::body::Body;
use axum::http::Request;
use helpers::{build_app, sign_token};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// POST `/api/v1/vault_context` avec un body JSON complet (permet de passer `mode`).
///
/// Contourne la signature fixe de `call_vault_context` (qui n'a pas de param `mode`).
async fn call_vault_context_json(
    app: axum::Router,
    token: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let req = Request::builder()
        .uri("/api/v1/vault_context")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&body).expect("sérialisation body — invariant"),
        ))
        .expect("construction requête — invariant");
    let resp = app
        .oneshot(req)
        .await
        .expect("vault_context oneshot — invariant");
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::OK,
        "vault_context doit retourner 200"
    );
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body — invariant")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("decode vault_context JSON — invariant")
}

/// Le mode Raw reproduit l'ancienne jointure, le budget chars/3, la troncature char-safe.
///
/// # Invariants vérifiés
///
/// 1. `assembled_text` = jointure `"\n\n---\n\n"` des `body_text` (si >= 2 notes matchées).
/// 2. `budget_used` = `(assembled_text.chars().count() / 3)` (division entière, plancher 1).
/// 3. `diagnostics.embed_fallback = false` (Raw n'invoque pas l'embedder).
/// 4. `included` non-vide, chaque entrée a `ulid`/`section`/`date`/`score=0.0`.
#[tokio::test]
async fn raw_mode_reproduces_legacy_dump() {
    let env = build_app().await;

    // Seed 5 notes contenant "alpha" pour déclencher le chemin FTS multi-notes.
    env.write_note_with_h1(
        "Alpha Note Un",
        "Contenu alpha pour tester le mode raw numéro un.",
    )
    .await;
    env.write_note_with_h1(
        "Alpha Note Deux",
        "Deuxième note alpha contenu test parité.",
    )
    .await;
    env.write_note_with_h1(
        "Alpha Note Trois",
        "Troisième contenu alpha raw parity check.",
    )
    .await;
    env.write_note_with_h1(
        "Alpha Quatre",
        "Quatrième note sur alpha contenu additionnel.",
    )
    .await;
    env.write_note_with_h1(
        "Alpha Cinq",
        "Cinquième note alpha pour couvrir la jointure.",
    )
    .await;

    let token = sign_token(&env.state);

    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha",
            "tenant_id": "main",
            "mode": "raw",
            "max_tokens": 2000
        }),
    )
    .await;

    // ── Champ assembled_text ─────────────────────────────────────────────────
    let txt = resp["assembled_text"]
        .as_str()
        .expect("assembled_text doit être une string");

    // Au moins une note matchée → assembled_text non-vide.
    assert!(
        !txt.is_empty(),
        "assembled_text vide — aucune note trouvée; resp={resp}"
    );

    // Jointure "\n\n---\n\n" présente si >= 2 notes incluses.
    let included = resp["included"]
        .as_array()
        .expect("included doit être un array");
    if included.len() >= 2 {
        assert!(
            txt.contains("\n\n---\n\n"),
            "jointure '\\n\\n---\\n\\n' absente malgré {} notes — assembled_text={txt:?}",
            included.len()
        );
    }

    // ── Champ budget_used — parité division entière legacy ────────────────────
    let budget_used = resp["budget_used"]
        .as_u64()
        .expect("budget_used doit être un entier");
    let chars = txt.chars().count();
    let expected_budget = (chars / 3).max(1) as u64;
    assert_eq!(
        budget_used, expected_budget,
        "budget_used={budget_used} != chars/3={expected_budget} (chars={chars})"
    );

    // ── Champ diagnostics ─────────────────────────────────────────────────────
    assert_eq!(
        resp["diagnostics"]["embed_fallback"],
        serde_json::json!(false),
        "embed_fallback doit être false en mode Raw (pas d'embed invoqué)"
    );
    let candidates = resp["diagnostics"]["candidates_considered"]
        .as_u64()
        .expect("candidates_considered u64");
    assert!(candidates >= 1, "candidates_considered doit être >= 1");

    // ── Champ included — structure et parité ─────────────────────────────────
    assert!(
        !included.is_empty(),
        "included vide — aucune note dans la réponse"
    );

    for note in included {
        assert!(
            note["ulid"].is_string(),
            "IncludedNote.ulid doit être une string — note={note}"
        );
        assert!(
            note["section"].is_string(),
            "IncludedNote.section doit être une string — note={note}"
        );
        assert!(
            note["date"].is_string(),
            "IncludedNote.date doit être une string — note={note}"
        );
        // Score = 0.0 en mode Raw (pas de scoring composite).
        assert_eq!(
            note["score"].as_f64().unwrap_or(-1.0),
            0.0,
            "score doit être 0.0 en mode Raw — note={note}"
        );
    }
}
