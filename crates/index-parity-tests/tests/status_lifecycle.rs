//! Parité backend : machine à états du statut + decay (exclusion downgraded).
//!
//! Invariants :
//! - `downgrade_note` fait passer une note `Live` → `downgraded`, lisible via
//!   `get_note_status`.
//! - Une note `downgraded` est exclue de `search_semantic` (decay : les notes
//!   rétrogradées ne polluent plus la recherche sémantique).

mod common;

use common::{make_index, make_note_with_id, minimal_frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::status::NoteStatus;

#[tokio::test]
async fn live_status_readable_before_downgrade() {
    let idx = make_index().await;
    let note = make_note_with_id(NoteId::new(), minimal_frontmatter("main"), "vivante");
    idx.write_note(&note).await.expect("write");

    let before = idx
        .get_note_status("main", &note.id.to_string())
        .await
        .expect("get_note_status");
    assert_eq!(
        before,
        Some(NoteStatus::Live),
        "statut initial Live ({})",
        common::backend_label()
    );
}

#[tokio::test]
async fn downgrade_removes_note_from_live_listing() {
    // Contrat observable backend-agnostique : après `downgrade_note`, la note ne
    // figure plus dans `list_by_status(Live)`. (Le statut interne stocké est la
    // chaîne `'downgraded'`, distincte du variant enum `Deprecated`/`"deprecated"` —
    // incohérence latente documentée. On teste donc
    // l'effet observable, pas la valeur d'enum non parseable par `get_note_status`.)
    let idx = make_index().await;
    let vault = VaultId::new("main");
    let note = make_note_with_id(NoteId::new(), minimal_frontmatter("main"), "à rétrograder");
    idx.write_note(&note).await.expect("write");

    assert!(
        idx.list_by_status(&vault, NoteStatus::Live)
            .await
            .expect("list live before")
            .contains(&note.id),
        "note Live listée avant downgrade"
    );

    idx.downgrade_note(
        &gradatum_core::scope::AclCheckedVaultId::for_system_task(
            gradatum_core::scope::VaultId::new("main"),
        ),
        &note.id,
        "remplacée",
        None,
    )
    .await
    .expect("downgrade_note");

    assert!(
        !idx.list_by_status(&vault, NoteStatus::Live)
            .await
            .expect("list live after")
            .contains(&note.id),
        "note retirée du listing Live après downgrade ({})",
        common::backend_label()
    );
}

#[tokio::test]
async fn downgraded_note_excluded_from_semantic() {
    let idx = make_index().await;
    let embedder_id = "emb";
    let dim: u16 = 4;

    let live = make_note_with_id(NoteId::new(), minimal_frontmatter("main"), "vivante");
    let doomed = make_note_with_id(NoteId::new(), minimal_frontmatter("main"), "condamnée");
    idx.write_note(&live).await.expect("write live");
    idx.write_note(&doomed).await.expect("write doomed");

    // Même vecteur → mêmes scores, seul le statut les distingue.
    idx.insert_note_embedding("main", &live.id, embedder_id, dim, &[1.0, 0.0, 0.0, 0.0])
        .await
        .expect("emb live");
    idx.insert_note_embedding("main", &doomed.id, embedder_id, dim, &[1.0, 0.0, 0.0, 0.0])
        .await
        .expect("emb doomed");

    // Avant downgrade : les deux remontent.
    let before = idx
        .search_semantic(
            &gradatum_core::scope::AclCheckedVaultId::for_system_task(
                gradatum_core::scope::VaultId::new("main"),
            ),
            embedder_id,
            &[1.0, 0.0, 0.0, 0.0],
            10,
            None,
        )
        .await
        .expect("search before");
    assert!(
        before.iter().any(|(id, _)| *id == doomed.id),
        "doomed présente avant"
    );

    idx.downgrade_note(
        &gradatum_core::scope::AclCheckedVaultId::for_system_task(
            gradatum_core::scope::VaultId::new("main"),
        ),
        &doomed.id,
        "decay test",
        None,
    )
    .await
    .expect("downgrade");

    let after = idx
        .search_semantic(
            &gradatum_core::scope::AclCheckedVaultId::for_system_task(
                gradatum_core::scope::VaultId::new("main"),
            ),
            embedder_id,
            &[1.0, 0.0, 0.0, 0.0],
            10,
            None,
        )
        .await
        .expect("search after");
    assert!(
        after.iter().any(|(id, _)| *id == live.id),
        "live toujours présente"
    );
    assert!(
        !after.iter().any(|(id, _)| *id == doomed.id),
        "downgraded exclue du sémantique ({})",
        common::backend_label()
    );
}

#[tokio::test]
async fn get_note_status_absent_returns_none() {
    let idx = make_index().await;
    let status = idx
        .get_note_status("main", &NoteId::new().to_string())
        .await
        .expect("get_note_status absent");
    assert!(
        status.is_none(),
        "note absente → None ({})",
        common::backend_label()
    );
}
