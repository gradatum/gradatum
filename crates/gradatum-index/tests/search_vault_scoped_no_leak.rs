//! `search_fts_for_forget` : scoping vault + typage `&VaultId`.
//!
//! ## Ce que ce test verrouille
//!
//! 1. **Scoping / no-leak (sécurité, PROUVÉ)** — la résolution de scope FTS du
//!    forget sémantique ne franchit jamais la frontière de vault : une recherche
//!    menée sur `vault-b` ne retourne AUCUNE note de `main`, et réciproquement.
//!    La requête inhérente (`sqlite.rs`) filtre `WHERE n.vault_id = ?1`. Ce test
//!    en apporte la preuve comportementale du **no-leak (correctness)**.
//!
//!    ⚠️ **PAS sargable — caveat perf-sargable OUVERT pour ce path.** Cette
//!    requête est **FTS-MATCH-driven** (`notes_fts MATCH ?2 JOIN notes`) : `notes_fts`
//!    est la table pilote, `vault_id` est un **filtre RÉSIDUEL post-match** (la PK
//!    composite `(vault_id, id)` n'est PAS utilisée par ce plan). En multi-vault,
//!    la requête FTS-matche à travers TOUS les vaults puis jette les lignes
//!    hors-vault → le no-leak tient (correctness), mais le caveat pré-flip
//!    **perf sargable reste OUVERT** pour le path FTS-forget (fermeture = `vault_id`
//!    dans la table FTS ou FTS partitionné par vault — W2/pré-flip). Ne PAS le
//!    considérer clos sur la base de ce test.
//!
//! 2. **Typage (`&str → &VaultId`)** — le test invoque la méthode via le trait
//!    `IndexStore` en lui passant un `&VaultId`. Tant que la signature du trait
//!    reste `vault_id: &str`, ce fichier NE COMPILE PAS (RED). L'appel est
//!    volontairement qualifié `IndexStore::search_fts_for_forget(&idx, …)` pour
//!    contourner la méthode inhérente homonyme `&str` de `SqliteIndex`, qui
//!    masquerait sinon la méthode du trait par résolution de méthode Rust.

mod common;

use common::{VAULT_B, VAULT_MAIN, seed_colliding_note, two_vault_index};
use gradatum_core::IndexStore;
use gradatum_core::scope::VaultId;

/// La résolution de scope FTS du forget est strictement cloisonnée par vault :
/// aucun terme propre à un vault n'est visible depuis un autre, et le terme propre
/// d'un vault reste trouvable dans ce vault (scoping positif).
#[tokio::test]
async fn search_fts_for_forget_scoped_no_cross_vault_leak() {
    let idx = two_vault_index().await;

    // Deux notes à ULID distincts, chacune porteuse d'un token FTS unique.
    // `main` → « zephyrtoken », `vault-b` → « quokkatoken ».
    seed_colliding_note(&idx, VAULT_MAIN, "note-main", "zephyrtoken").await;
    seed_colliding_note(&idx, VAULT_B, "note-b", "quokkatoken").await;

    // (1) Le token de `main` NE FUIT PAS dans `vault-b`.
    let leak_into_b =
        IndexStore::search_fts_for_forget(&idx, &VaultId::new(VAULT_B), "zephyrtoken", 50)
            .await
            .expect("fts forget scopée vault-b (zephyrtoken)");
    assert!(
        leak_into_b.is_empty(),
        "la recherche forget sur vault-b ne doit retourner aucune note de main (zephyrtoken), obtenu {leak_into_b:?}"
    );

    // (2) Scoping positif — le token propre de `vault-b` reste trouvable dans `vault-b`.
    let hit_in_b =
        IndexStore::search_fts_for_forget(&idx, &VaultId::new(VAULT_B), "quokkatoken", 50)
            .await
            .expect("fts forget scopée vault-b (quokkatoken)");
    assert_eq!(
        hit_in_b.len(),
        1,
        "la recherche forget sur vault-b doit trouver la note propre de vault-b (quokkatoken), obtenu {hit_in_b:?}"
    );

    // (3) Réciproque — le token de `vault-b` NE FUIT PAS dans `main`.
    let leak_into_main =
        IndexStore::search_fts_for_forget(&idx, &VaultId::new(VAULT_MAIN), "quokkatoken", 50)
            .await
            .expect("fts forget scopée main (quokkatoken)");
    assert!(
        leak_into_main.is_empty(),
        "la recherche forget sur main ne doit retourner aucune note de vault-b (quokkatoken), obtenu {leak_into_main:?}"
    );
}
