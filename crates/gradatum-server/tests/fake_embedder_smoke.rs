//! Test de fumée — `FakeEmbedder` déterministe + `build_app_with_embedder` (Task 3.5).
//!
//! Vérifie :
//! - `backend_kind() != Noop` → le chemin sémantique est activé.
//! - Déterminisme strict : même texte → même vecteur (2 appels successifs).
//! - Dimension conforme au `dim` paramétré.
//!
//! Ce fichier est le gate de non-régression P0-1 du plan v0.7.0 : si ces assertions
//! échouent, tous les tests sémantiques aval (Tasks 5/9/10) seraient non probants.

#[path = "helpers/mod.rs"]
mod helpers;

use gradatum_embed::EmbedBackend;
use helpers::{FakeEmbedder, build_app_with_embedder};
use std::sync::Arc;

/// Le `FakeEmbedder` active le chemin sémantique et est déterministe.
///
/// Deux propriétés vérifiées :
/// 1. `backend_kind() != Noop` → RRF/semantic activé dans vault_search/vault_context.
/// 2. Même texte → même vecteur (aucun tirage aléatoire).
/// 3. Dimension = dim déclaré.
#[tokio::test]
async fn fake_embedder_enables_semantic_path() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;

    assert_ne!(
        env.state.embedder.backend_kind(),
        EmbedBackend::Noop,
        "FakeEmbedder doit activer le chemin sémantique (non-Noop)"
    );

    let v = env
        .state
        .embedder
        .embed("alpha")
        .await
        .expect("embed — invariant FakeEmbedder");
    let v2 = env
        .state
        .embedder
        .embed("alpha")
        .await
        .expect("embed — invariant FakeEmbedder");

    assert_eq!(
        v, v2,
        "FakeEmbedder doit être déterministe : même texte → même vecteur"
    );
    assert_eq!(
        v.len(),
        1024,
        "dimension du vecteur doit correspondre au dim=1024 paramétré"
    );
}
