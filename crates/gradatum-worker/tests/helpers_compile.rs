//! Sanity check : helpers/mod.rs compile et expose les bonnes signatures.
//!
//! Pattern : `#[path = "helpers/mod.rs"] mod helpers;` → utilise `helpers::*`.

#[path = "helpers/mod.rs"]
mod helpers;

#[tokio::test]
async fn helpers_compile_curate_fixture_smoke() {
    let fixture = helpers::test_curate_fixture().await;
    // Sanity : process_curate sur un body sans wikilink → Ok(JobOutput).
    helpers::process_curate(
        &fixture,
        "[DECISIONS] Smoke helper",
        "# Smoke\n\nAucun lien.",
    )
    .await
    .expect("handle_curate smoke doit réussir");
    // Sanity : has_backlink_to sur un id inexistant retourne false sans erreur.
    let none = helpers::has_backlink_to(&fixture.index, "01ZZZZZZZZZZZZZZZZZZZZZZZZ").await;
    assert!(!none);
}
