//! B2b — HttpEmbedder roundtrip via wiremock (P0)
//!
//! Mesure la latence client HTTP embed (parsing JSON + reqwest + serde).
//! Le mock wiremock simule un serveur de latence ~0ms (LAN local) pour isoler
//! le coût client pur sans variance réseau.
//!
//! Cible : p50 < 15ms, p99 < 50ms.
//! En pratique sur localhost (loopback), on mesure sub-1ms.
//! Le résultat documenté dans BENCH.md représente le coût client pur — pas LAN réel.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gradatum_embed::{Embedder, HttpEmbedder};

/// Prépare un mock server wiremock qui retourne une réponse d'embedding valide.
///
/// La réponse simule un vecteur de 384 dimensions (bge-small-en-v1.5).
fn make_embed_response() -> serde_json::Value {
    let vector: Vec<f32> = (0..384).map(|i| (i as f32) / 384.0).collect();
    serde_json::json!({
        "object": "list",
        "data": [{
            "object": "embedding",
            "index": 0,
            "embedding": vector
        }],
        "model": "bge-small-en-v1.5",
        "usage": { "prompt_tokens": 10, "total_tokens": 10 }
    })
}

fn bench_http_embedder(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // Setup wiremock — en dehors de la boucle de mesure.
    let (server_url, _mock_server) = rt.block_on(async {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_embed_response()))
            .expect(1..)
            .mount(&server)
            .await;

        let url = format!("{}/v1/embeddings", server.uri());
        (url, server)
    });

    let embedder = HttpEmbedder::new(&server_url, "bge-small-en-v1.5", 384);
    let text = "benchmark test sentence";

    let mut group = c.benchmark_group("B2b-http-embedder");
    group.sample_size(30);

    group.bench_function("single-embed-wiremock", |b| {
        b.iter(|| {
            let vec = rt
                .block_on(async { embedder.embed(black_box(text)).await })
                .expect("embed HTTP failed");
            black_box(vec);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_http_embedder);
criterion_main!(benches);
