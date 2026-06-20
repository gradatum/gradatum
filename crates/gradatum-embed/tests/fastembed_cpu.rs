//! Tests d'intégration pour `FastEmbedCpu`.
//!
//! **Requiert la feature `fastembed-cpu`** :
//! ```bash
//! cargo test -p gradatum-embed --features fastembed-cpu -- --ignored fastembed_cpu
//! ```
//!
//! Ces tests téléchargent ~150 MB de poids de modèle lors du premier run.
//! Marqués `#[ignore]` par défaut pour ne pas bloquer la CI sans réseau/ORT.

// Ce fichier de test ne compile que si la feature fastembed-cpu est active.
// Sans la feature, `FastEmbedCpu` n'est pas exporté et le module fastembed_cpu
// n'existe pas dans la crate.
#![cfg(feature = "fastembed-cpu")]

use gradatum_embed::{EmbedBackend, Embedder, FastEmbedCpu};

#[tokio::test]
#[ignore = "télécharge ~150 MB de poids bge-small-en-v1.5 — activer avec --ignored --features fastembed-cpu"]
async fn fastembed_cpu_returns_384d_vector() {
    let e = FastEmbedCpu::try_default().expect("init bge-small-en-v1.5");
    assert_eq!(e.dim(), 384);
    assert_eq!(e.embedder_id(), "bge-small-en-v1.5");
    assert_eq!(e.backend_kind(), EmbedBackend::FastembedCpu);

    let v = e.embed("hello world").await.expect("embed ok");
    assert_eq!(v.len(), 384);
    // Le vecteur doit avoir au moins quelques valeurs non-nulles.
    assert!(
        v.iter().any(|&x| x != 0.0),
        "le vecteur ne doit pas être entièrement nul"
    );
}

#[tokio::test]
#[ignore = "télécharge ~150 MB de poids bge-small-en-v1.5 — activer avec --ignored --features fastembed-cpu"]
async fn fastembed_cpu_batch_consistent() {
    let e = FastEmbedCpu::try_default().expect("init");
    let v = e.embed_batch(&["hello", "world", "foo bar"]).await.unwrap();
    assert_eq!(v.len(), 3);
    for embed in &v {
        assert_eq!(embed.len(), 384);
    }
}

#[tokio::test]
#[ignore = "télécharge ~150 MB de poids bge-small-en-v1.5 — activer avec --ignored --features fastembed-cpu"]
async fn fastembed_cpu_different_texts_produce_different_vectors() {
    let e = FastEmbedCpu::try_default().expect("init");
    let v1 = e.embed("bonjour le monde").await.unwrap();
    let v2 = e.embed("completely different text").await.unwrap();
    // Les vecteurs pour des textes différents doivent différer.
    let same = v1.iter().zip(v2.iter()).all(|(a, b)| (a - b).abs() < 1e-6);
    assert!(
        !same,
        "deux textes différents ne doivent pas produire le même vecteur"
    );
}
