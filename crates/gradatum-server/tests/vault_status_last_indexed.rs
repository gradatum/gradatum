//! Frontière serveur — F-169 : `vault_status_impl` renseigne `last_indexed_at`.
//!
//! La couche sqlite ([`last_indexed_at`]) ne renvoie que des millisecondes brutes ; le
//! rendu ms → ISO 8601 UTC et le passage `None` → `null` sont décidés UNIQUEMENT ici
//! (logic.rs). Ces deux tests traversent donc la frontière que la couche index ne peut
//! pas observer :
//! - corpus live peuplé → un horodatage ISO valide (fin du hardcode `None`) ;
//! - corpus live vide (sentinelle seule) → `None`.
//!
//! Ensemble ils prouvent que la valeur SUIT le corpus : un `Some(...)` gravé en dur
//! échouerait au 2ᵉ test, un `None` gravé en dur échouerait au 1ᵉʳ.

#![allow(dead_code)]

use std::sync::Arc;

use chrono::Utc;
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::trust::TrustContext;
use gradatum_server::config::ServerConfig;
use gradatum_server::state::AppState;
use gradatum_vault::Vault;
use tempfile::TempDir;

/// Preset ACL : `reader` en lecture sur `main/*` — Allow sur le locus mono-vault.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "reader"
read_patterns  = ["main/*"]
write_patterns = []
"#;

/// Frontmatter minimal `main`, section non protégée (pas de guard identité).
fn frontmatter_main() -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Feedback,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// `TrustContext` BearerToken pour `main`, identité `reader` (read scope).
fn bearer_main() -> TrustContext {
    TrustContext::BearerToken {
        kid: "k".into(),
        aud: "gradatum".into(),
        sub: "reader".into(),
        scopes: vec!["read".into()],
        tenant_id: "main".into(),
        jti: None,
    }
}

/// Monte un `AppState` mono-vault (`multi_tenant` OFF par défaut) adossé à un vault
/// physique `main`. Renvoie l'état et le vault (pour semer des notes).
async fn build_state() -> (AppState, Arc<Vault>, TempDir) {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("vault");
    let vault_main = Arc::new(
        Vault::create(&root, VaultId::new("main"))
            .await
            .expect("Vault::create main"),
    );
    let shared_index = Arc::clone(vault_main.index());

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");
    let mut state =
        AppState::with_jwt_and_acl(jwt, acl).with_server_config(ServerConfig::default());
    let search_index: Arc<dyn gradatum_core::index::Index> = shared_index.clone();
    state.search = search_index;

    (state, vault_main, tmp)
}

/// Corpus peuplé → `last_indexed_at` = horodatage ISO 8601 UTC valide (fin du hardcode).
#[tokio::test]
async fn vault_status_last_indexed_at_is_iso_when_notes_exist() {
    let (state, vault_main, _tmp) = build_state().await;
    vault_main
        .write_note_with_id(frontmatter_main(), "# Note\n\nCorps.".into(), NoteId::new())
        .await
        .expect("write_note_with_id");

    let resp = gradatum_server::api_v1::logic::vault_status_impl(&state, &bearer_main())
        .await
        .expect("vault_status_impl");

    let iso = resp
        .last_indexed_at
        .expect("last_indexed_at doit être renseigné quand une note live existe");
    // Doit être un ISO 8601 UTC re-parseable — pas une chaîne arbitraire.
    chrono::DateTime::parse_from_rfc3339(&iso)
        .unwrap_or_else(|e| panic!("last_indexed_at n'est pas un ISO 8601 valide ({iso}) : {e}"));
    assert!(
        iso.ends_with('Z'),
        "last_indexed_at doit être en UTC (suffixe Z) : {iso}"
    );
}

/// Corpus live vide (seule la sentinelle existe) → `None`, pas un faux horodatage.
#[tokio::test]
async fn vault_status_last_indexed_at_is_none_when_corpus_empty() {
    let (state, _vault_main, _tmp) = build_state().await;

    let resp = gradatum_server::api_v1::logic::vault_status_impl(&state, &bearer_main())
        .await
        .expect("vault_status_impl");

    assert_eq!(
        resp.last_indexed_at, None,
        "corpus live vide → last_indexed_at = None"
    );
}
