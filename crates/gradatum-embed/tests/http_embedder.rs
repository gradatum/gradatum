//! Tests d'intégration pour `HttpEmbedder` avec mock serveur wiremock.

use gradatum_embed::{EmbedBackend, EmbedError, Embedder, HttpEmbedder};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Construit la réponse JSON OpenAI embeddings standard.
fn openai_response(embeddings: Vec<Vec<f64>>) -> serde_json::Value {
    let data: Vec<serde_json::Value> = embeddings
        .into_iter()
        .enumerate()
        .map(|(i, emb)| {
            serde_json::json!({
                "embedding": emb,
                "index": i
            })
        })
        .collect();
    serde_json::json!({
        "data": data,
        "model": "test-model",
        "usage": { "prompt_tokens": 5, "total_tokens": 5 }
    })
}

#[tokio::test]
async fn http_embed_parses_openai_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(openai_response(vec![vec![0.1, 0.2, 0.3]])),
        )
        .mount(&server)
        .await;

    let e = HttpEmbedder::new(format!("{}/v1/embeddings", server.uri()), "test-model", 3);
    let v = e.embed("hello").await.unwrap();
    assert_eq!(v.len(), 3);
    assert!((v[0] - 0.1_f32).abs() < 1e-5);
    assert!((v[1] - 0.2_f32).abs() < 1e-5);
    assert!((v[2] - 0.3_f32).abs() < 1e-5);
    assert_eq!(e.backend_kind(), EmbedBackend::Http);
    assert_eq!(e.dim(), 3);
}

#[tokio::test]
async fn http_embed_502_returns_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(502).set_body_string("Bad Gateway"))
        .mount(&server)
        .await;

    let e = HttpEmbedder::new(format!("{}/v1/embeddings", server.uri()), "test-model", 3);
    let result = e.embed("hello").await;
    assert!(result.is_err(), "HTTP 502 doit retourner une erreur");
    match result.unwrap_err() {
        EmbedError::InvalidResponse(msg) => {
            assert!(
                msg.contains("502"),
                "message doit mentionner le code 502, got: {msg}"
            );
        }
        other => panic!("attendu InvalidResponse, got: {other:?}"),
    }
}

#[tokio::test]
async fn http_embed_dim_mismatch_returns_error() {
    let server = MockServer::start().await;

    // Le serveur retourne 3 dimensions mais le client attend 1024.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(openai_response(vec![vec![0.1, 0.2, 0.3]])),
        )
        .mount(&server)
        .await;

    let e = HttpEmbedder::new(
        format!("{}/v1/embeddings", server.uri()),
        "test-model",
        1024, // dim attendu = 1024, reçu = 3
    );
    let result = e.embed("hello").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        EmbedError::DimMismatch { expected, got } => {
            assert_eq!(expected, 1024);
            assert_eq!(got, 3);
        }
        other => panic!("attendu DimMismatch, got: {other:?}"),
    }
}

#[tokio::test]
async fn http_embed_batch_returns_multiple_vectors() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(openai_response(vec![
                vec![1.0, 0.0],
                vec![0.0, 1.0],
                vec![0.5, 0.5],
            ])),
        )
        .mount(&server)
        .await;

    let e = HttpEmbedder::new(format!("{}/v1/embeddings", server.uri()), "test-model", 2);
    let texts = vec!["un", "deux", "trois"];
    let result = e.embed_batch(&texts).await.unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0], vec![1.0_f32, 0.0]);
    assert_eq!(result[1], vec![0.0_f32, 1.0]);
    assert_eq!(result[2], vec![0.5_f32, 0.5]);
}

#[tokio::test]
async fn http_embed_batch_count_mismatch_returns_error() {
    // Le serveur retourne 2 vecteurs pour 3 textes envoyés — protocol violation.
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            // Seulement 2 embeddings pour 3 textes (mismatch).
            ResponseTemplate::new(200)
                .set_body_json(openai_response(vec![vec![1.0, 0.0], vec![0.0, 1.0]])),
        )
        .mount(&server)
        .await;

    let e = HttpEmbedder::new(format!("{}/v1/embeddings", server.uri()), "test-model", 2);
    // On envoie 3 textes mais le serveur répond avec 2 vecteurs.
    let result = e.embed_batch(&["un", "deux", "trois"]).await;
    assert!(result.is_err(), "count mismatch doit retourner une erreur");
    match result.unwrap_err() {
        EmbedError::CountMismatch { sent, received } => {
            assert_eq!(sent, 3, "sent doit être 3");
            assert_eq!(received, 2, "received doit être 2");
        }
        other => panic!("attendu CountMismatch, got: {other:?}"),
    }
}

/// F2 — index dupliqué dans la réponse → `IndexMismatch` (désalignement détecté).
///
/// Un serveur buggé peut retourner `[{index:0, ...}, {index:0, ...}]` pour 2 textes.
/// Après le tri `sort_by_key(index)`, les deux items ont `index=0`. La validation
/// de permutation contiguë détecte que `items[1].index (0) != position (1)` → erreur.
/// Sans cette vérification, le code retournait 2 vecteurs silencieusement alignés de façon
/// incorrecte (les deux mapperaient au même texte d'origine).
#[tokio::test]
async fn f2_duplicate_index_returns_index_mismatch_error() {
    let server = MockServer::start().await;

    // Serveur buggé : retourne deux items avec le même index 0.
    let body = serde_json::json!({
        "data": [
            { "embedding": [1.0, 0.0], "index": 0 },
            { "embedding": [0.0, 1.0], "index": 0 }
        ],
        "model": "test-model"
    });

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let e = HttpEmbedder::new(format!("{}/v1/embeddings", server.uri()), "test-model", 2);
    let result = e.embed_batch(&["premier", "deuxieme"]).await;
    assert!(
        result.is_err(),
        "index dupliqué doit retourner une erreur, pas un succès silencieux"
    );
    match result.unwrap_err() {
        EmbedError::IndexMismatch {
            position,
            expected,
            got,
        } => {
            // Après tri par index (0, 0), la position 1 a got=0 au lieu de expected=1.
            assert_eq!(
                position, 1,
                "la position incohérente doit être 1 (index 0 dupliqué)"
            );
            assert_eq!(expected, 1, "expected doit être 1 (= position)");
            assert_eq!(got, 0, "got doit être 0 (l'index dupliqué du serveur)");
        }
        other => panic!("attendu IndexMismatch, got: {other:?}"),
    }
}

/// F2 — index hors-borne (n au lieu de 0..n-1) → `IndexMismatch`.
///
/// Un index hors-borne (ex. `index=1,5` pour 2 items) est détecté car
/// après tri [1, 5], position 0 a `got=1` au lieu de `expected=0`.
#[tokio::test]
async fn f2_out_of_bounds_index_returns_index_mismatch_error() {
    let server = MockServer::start().await;

    // Serveur buggé : index 1 et 5 pour 2 textes (5 est hors-borne).
    let body = serde_json::json!({
        "data": [
            { "embedding": [0.0, 1.0], "index": 5 },
            { "embedding": [1.0, 0.0], "index": 1 }
        ],
        "model": "test-model"
    });

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let e = HttpEmbedder::new(format!("{}/v1/embeddings", server.uri()), "test-model", 2);
    let result = e.embed_batch(&["premier", "deuxieme"]).await;
    assert!(
        result.is_err(),
        "index hors-borne doit retourner une erreur"
    );
    // Après sort_by_key: [index:1, index:5] → position 0 a got=1 (attendu 0)
    match result.unwrap_err() {
        EmbedError::IndexMismatch {
            position,
            expected,
            got,
        } => {
            assert_eq!(position, 0);
            assert_eq!(expected, 0);
            assert_eq!(got, 1);
        }
        other => panic!("attendu IndexMismatch, got: {other:?}"),
    }
}

#[tokio::test]
async fn http_embed_reorders_data_by_index() {
    // Certains serveurs retournent data[] dans un ordre différent de l'input.
    let server = MockServer::start().await;

    // Retourne les items avec index inversé.
    let body = serde_json::json!({
        "data": [
            { "embedding": [0.0, 1.0], "index": 1 },
            { "embedding": [1.0, 0.0], "index": 0 }
        ],
        "model": "test-model"
    });

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let e = HttpEmbedder::new(format!("{}/v1/embeddings", server.uri()), "test-model", 2);
    let result = e.embed_batch(&["premier", "deuxieme"]).await.unwrap();
    // Après tri par index, premier = [1.0, 0.0] (index 0), deuxième = [0.0, 1.0] (index 1).
    assert_eq!(result[0], vec![1.0_f32, 0.0]);
    assert_eq!(result[1], vec![0.0_f32, 1.0]);
}
