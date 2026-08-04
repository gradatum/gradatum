//! Auto-vérification du harnais 2-vaults « flag ON » (C4-1e).
//!
//! Prouve que la fixture réutilisable ([`common::two_vault_index`] +
//! [`common::seed_colliding_note`]) crée bien deux vaults distincts contenant des
//! notes de MÊME ULID mais au contenu propre, et que la lecture scopée
//! `(vault_id, id)` les isole l'un de l'autre.
//!
//! Ce test est le socle des jeux « flag ON » des slices C4-1e suivantes : s'il
//! échoue, aucune assertion d'isolation en aval n'est fiable.

mod common;
use common::{VAULT_B, VAULT_MAIN, h1_title, seed_colliding_note, two_vault_index};

/// Deux notes de même ULID semées dans `main` et `vault-b` restent lisibles
/// séparément, chacune avec son propre titre — la collision d'ULID ne fusionne pas
/// les deux vaults.
#[tokio::test]
async fn harness_isolates_two_vaults_same_id() {
    let idx = two_vault_index().await;

    // Même `id` logique → même ULID dérivé dans les deux vaults (collision voulue).
    seed_colliding_note(&idx, VAULT_MAIN, "01AAA", "titre-main").await;
    seed_colliding_note(&idx, VAULT_B, "01AAA", "titre-b").await;

    let n_main = idx
        .get_note(VAULT_MAIN, &common::colliding_note_id("01AAA").to_string())
        .await
        .expect("lecture note main")
        .expect("la note main doit exister");
    let n_b = idx
        .get_note(VAULT_B, &common::colliding_note_id("01AAA").to_string())
        .await
        .expect("lecture note vault-b")
        .expect("la note vault-b doit exister");

    // Isolation : chaque vault renvoie SON titre (porté par le H1 du corps), pas
    // celui de l'autre vault malgré l'ULID identique.
    assert_eq!(
        h1_title(&n_main.body_text),
        Some("titre-main"),
        "le vault main doit conserver son propre titre"
    );
    assert_eq!(
        h1_title(&n_b.body_text),
        Some("titre-b"),
        "le vault vault-b doit conserver son propre titre"
    );
}
