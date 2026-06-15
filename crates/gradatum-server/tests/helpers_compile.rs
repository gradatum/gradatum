//! Sanity check : helpers/mod.rs serveur compile et expose les bonnes signatures.

#[path = "helpers/mod.rs"]
mod helpers;

#[tokio::test]
async fn helpers_compile_app_state_smoke() {
    let env = helpers::build_app().await;
    let _token = helpers::sign_token(&env.state);
    // Sanity : seed une note via vault et vérifie qu'elle est lisible via search.
    let nid = env
        .write_note_with_h1("Helper Smoke Title", "body smoke helper")
        .await;
    let rec = env
        .state
        .search
        .get_note("main", &nid.to_string())
        .await
        .expect("get_note");
    assert!(
        rec.is_some(),
        "note seedée doit être lisible via search.get_note"
    );
}
