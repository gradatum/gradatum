//! Sanity check : helpers/mod.rs compile et expose les bonnes signatures.
//!
//! Pattern A-rev2-3 : `#[path = "helpers/mod.rs"] mod helpers;` → utilise `helpers::*`.

#[path = "helpers/mod.rs"]
mod helpers;

#[tokio::test]
async fn helpers_compile_dispatcher_fixture_smoke() {
    let fixture = helpers::test_dispatcher_with_index().await;
    // Sanity : la queue est vide → run_once retourne Ok(false).
    let processed = fixture.dispatcher.run_once().await.unwrap();
    assert!(!processed, "queue vide → run_once = false");
    // Sanity : has_backlink_to sur un id inexistant retourne false sans erreur.
    let none = helpers::has_backlink_to(&fixture.index, "01ZZZZZZZZZZZZZZZZZZZZZZZZ").await;
    assert!(!none);
}
