//! Test TDD P2-a — Timeout embed dans `build_skill_index`.
//!
//! Vérifie la dégradation gracieuse de `build_skill_index` lorsque l'embedder
//! dépasse le timeout configuré : l'index retourné est vide (sans panic, sans Err).
//!
//! # Preuve TDD
//!
//! - **AVANT le fix** : `build_skill_index` n'accepte pas de paramètre `embed_timeout_ms`
//!   → ce fichier ne compile pas (signature incorrecte) → preuve rouge.
//! - **APRÈS le fix** : compile + `entries.is_empty()` prouvé → vert.
//!
//! # Setup
//!
//! 1 note section `"skills"` seedée → `list_notes` retourne 1 note.
//! `SlowEmbedder.embed_batch` dort 500ms >> `embed_timeout_ms=50ms` → timeout déclenché.

#[path = "helpers/mod.rs"]
mod helpers;

use std::time::Duration;

use async_trait::async_trait;
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use helpers::build_app;
use ulid::Ulid;

/// Embedder simulant une latence de 500ms — dépasse largement tout timeout raisonnable.
///
/// Utilisé pour déclencher le timeout dans `build_skill_index` de manière déterministe.
struct SlowEmbedder;

#[async_trait]
impl Embedder for SlowEmbedder {
    fn embedder_id(&self) -> &str {
        "slow-embedder-test"
    }

    fn dim(&self) -> u16 {
        8
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(vec![0.0f32; 8])
    }

    /// Dort 500ms avant de retourner — déclenche le timeout de 50ms dans le test.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(texts.iter().map(|_| vec![0.0f32; 8]).collect())
    }

    fn backend_kind(&self) -> EmbedBackend {
        // Http = non-Noop pour simuler un embedder réel (même si lent).
        EmbedBackend::Http
    }
}

/// `build_skill_index` avec embedder lent → timeout → index vide, pas de panic.
///
/// # Preuve TDD (rouge → vert)
///
/// AVANT fix : paramètre `embed_timeout_ms` inexistant → compilation échoue.
/// APRÈS fix : `Ok(SkillIndex { entries: [] })` retourné sans panic ni propagation d'Err.
///
/// # Invariant dégradation gracieuse
///
/// L'embedder dort 500ms. Le timeout est 50ms. La fonction doit :
/// 1. Annuler l'embed (timeout déclenché).
/// 2. Logger un `warn` (non testé ici — comportement observable en log).
/// 3. Retourner `Ok` avec un index vide.
#[tokio::test]
async fn build_skill_index_embed_timeout_returns_empty_index() {
    let env = build_app().await;
    let idx = env._vault_typed.index();

    // Seeder 1 note section "skills" — list_notes trouvera cette note avant l'embed.
    let ulid = Ulid::new().to_string();
    idx.seed_note_with_fts(
        &ulid,
        "skills",
        "# Skill de test P2-a\nCorps du skill pour le test de timeout embed.",
    )
    .await
    .expect("seed skill note section=skills — invariant test P2-a");

    // embed_timeout_ms=50ms << SlowEmbedder sleep=500ms → timeout systématiquement déclenché.
    let result = gradatum_server::context::skills::build_skill_index(
        "main",
        env.state.search.as_ref(),
        &SlowEmbedder,
        50, // embed_timeout_ms
    )
    .await;

    // Doit retourner Ok (pas d'Err propagée) — dégradation gracieuse, pas de panic.
    let index = result.expect(
        "P2-a : build_skill_index avec timeout embed doit retourner Ok — pas de propagation d'Err",
    );
    assert!(
        index.entries.is_empty(),
        "P2-a : timeout embed → entries vides attendues (dégradation gracieuse), got {} entries",
        index.entries.len()
    );
}
