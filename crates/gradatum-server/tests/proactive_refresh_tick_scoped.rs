//! Verrou du flag-gate + du scoping per-vault du tick proactive-refresh (L5, caveat pré-flip).
//!
//! [`proactive_refresh_tick`] (`proactive_recall/refresh.rs`) est le miroir de
//! `review_promote::promote_tick` : le gate porte sur le FLAG `multi_tenant`, pas sur le
//! contenu de la liste de vaults. Ce fichier verrouille par test les deux propriétés
//! critiques du fix :
//!
//! 1. **OFF byte-identical** — à flag OFF, le tick délègue à `proactive_refresh_once`
//!    (mono-`"main"`) et n'itère JAMAIS `list_active_vaults()` : un second vault actif
//!    (`vault-b`) reste sans surface (`None`). Si le dispatcher itérait à OFF, `vault-b`
//!    aurait une surface.
//! 2. **ON scopé, sans clobber** — à flag ON, la boucle itère les vaults actifs et écrit
//!    la surface de CHAQUE vault dans SON propre tenant (lecture ET écriture scopées).
//!    Le second hardcode `upsert_surface("main", …)` — s'il n'était pas paramétré —
//!    ferait écraser la surface de `main` par celle de `vault-b` (clobber). Le test vérifie
//!    que la surface de `main` ne contient QUE ses propres candidats, et réciproquement.
//!
//! Régime multi-vault LOCAL au test : les notes de `main` et `vault-b` coexistent dans le
//! même index SQLite (partition par colonne `vault_id`). En prod le flag reste OFF (chemin
//! `proactive_refresh_once`, byte-identical).

use std::sync::Arc;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;
use gradatum_server::proactive_recall::ProactiveRecallConfig;
use gradatum_server::proactive_recall::refresh::proactive_refresh_tick;
use gradatum_server::proactive_surface_store::ProactiveSurfaceStore;
use gradatum_server::state::AppState;
use tempfile::TempDir;

/// ACL minimal — les jobs système (`for_system_task`) ne consultent pas l'ACL consumer ;
/// les deux vaults sont néanmoins déclarés en lecture pour rester explicite.
const MINIMAL_ACL: &str = r#"
[[consumer]]
identity = "test"
read_patterns  = ["main/*", "vault-b/*"]
write_patterns = []
"#;

// Corpus textuel commun aux deux vaults (scopé par vault_id — pas de fuite cross-vault).
// Les sources portent la salience (titre + tags) ; les candidats portent le body matchable BM25.
const SRC_TITLE: &str = "rust async recall";
const SRC_TAGS: &str = "recall async";
const SRC_BODY: &str = "rust async recall source note body";
const CND_TITLE: &str = "candidate recall note";
const CND_TAGS: &str = "candidate recall";
const CND_BODY: &str = "rust async recall candidate note body";

const RECENT_MS: i64 = 3_000_000_000_000; // sources : récentes
const OLD_MS: i64 = 1_000_000_000_000; // candidats : plus anciens

/// Construit un `AppState` minimal (index réel + store surface) et l'`Arc<SqliteIndex>`
/// concret (pour `seed_*` / `provision_vault`).
async fn build_state() -> (AppState, Arc<SqliteIndex>, TempDir) {
    let tmp = TempDir::new().expect("TempDir — invariant test fixture");
    let index_path = tmp.path().join("index.db");

    let idx = Arc::new(
        SqliteIndex::open(&index_path)
            .await
            .expect("SqliteIndex::open — invariant test fixture"),
    );
    let surface_store = ProactiveSurfaceStore::open(&index_path)
        .await
        .expect("ProactiveSurfaceStore::open — migration 0022 doit exister");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(MINIMAL_ACL).expect("AclEngine — invariant test fixture");

    let mut state = AppState::with_jwt_and_acl(jwt, acl);
    state.search = Arc::clone(&idx) as Arc<dyn Index>;
    state.proactive_surface = Some(surface_store);

    (state, idx, tmp)
}

/// Seed le corpus (3 sources récentes + 3 candidats anciens) d'un vault donné.
/// Retourne les ULIDs candidats (attendus dans la surface — les sources en sont exclues).
async fn seed_vault_corpus(idx: &SqliteIndex, vault_id: &str, id_infix: &str) -> Vec<String> {
    // ULIDs valides : exactement 26 caractères Crockford base32 (l'étage retrieval parse
    // chaque note_id comme ULID). `01K` + infix(1) + `SRC`/`CND`(3) + 17 zéros + nn(2) = 26.
    let mut candidate_ids = Vec::with_capacity(3);
    for n in 1..=3 {
        let src_id = format!("01K{id_infix}SRC00000000000000000{n:02}");
        idx.seed_lesson_vault(&src_id, vault_id, SRC_TITLE, SRC_TAGS, SRC_BODY, RECENT_MS)
            .await
            .expect("seed source lesson");

        let cnd_id = format!("01K{id_infix}CND00000000000000000{n:02}");
        idx.seed_lesson_vault(&cnd_id, vault_id, CND_TITLE, CND_TAGS, CND_BODY, OLD_MS)
            .await
            .expect("seed candidate lesson");
        candidate_ids.push(cnd_id);
    }
    candidate_ids
}

fn cfg() -> ProactiveRecallConfig {
    ProactiveRecallConfig {
        recent_k: 3, // prend les 3 sources récentes
        // `surface_size` = top_n du retrieval AVANT exclusion des sources. Le corpus par vault
        // = 3 sources + 3 candidats ; un top_n large (20) garantit que les 3 candidats
        // survivent à l'exclusion des 3 sources (sinon un candidat tomberait hors du top_n).
        surface_size: 20,
        ..Default::default()
    }
}

async fn surface_ulids(state: &AppState, tenant: &str) -> Option<Vec<String>> {
    state
        .proactive_surface
        .as_ref()
        .expect("proactive_surface présent — invariant test")
        .get_surface(tenant)
        .await
        .expect("get_surface")
        .map(|hits| hits.into_iter().map(|h| h.ulid).collect())
}

// ---------------------------------------------------------------------------
// Test 1 — OFF : mono-`"main"`, list_active_vaults JAMAIS consulté
// ---------------------------------------------------------------------------

/// À flag OFF, `proactive_refresh_tick` délègue à `proactive_refresh_once` (tenant `"main"`)
/// et n'itère pas les vaults actifs : `vault-b`, pourtant actif et doté d'un corpus, ne
/// reçoit AUCUNE surface (`None`). Preuve comportementale que la liste des vaults n'est pas
/// parcourue à OFF (byte-identical au chemin mono-tenant historique).
#[tokio::test]
async fn tick_off_keeps_mono_main_and_ignores_other_active_vaults() {
    let (state, idx, _tmp) = build_state().await;

    let main_candidates = seed_vault_corpus(&idx, "main", "M").await;

    // vault-b : tenant ACTIF + corpus — ne DOIT PAS être rafraîchi à OFF.
    idx.provision_vault("vault-b")
        .await
        .expect("provision_vault vault-b");
    let _ = seed_vault_corpus(&idx, "vault-b", "V").await;

    let result = proactive_refresh_tick(&state, &cfg(), false).await;
    assert!(result.is_ok(), "OFF ne doit pas échouer : {result:?}");

    let main_surface = surface_ulids(&state, "main")
        .await
        .expect("OFF : la surface main est écrite");
    assert!(
        !main_surface.is_empty(),
        "OFF : la surface main doit contenir les candidats main"
    );
    for cnd in &main_candidates {
        assert!(
            main_surface.contains(cnd),
            "OFF : le candidat main {cnd} doit être surfacé"
        );
    }

    assert!(
        surface_ulids(&state, "vault-b").await.is_none(),
        "OFF : vault-b ne doit recevoir AUCUNE surface (list_active_vaults non consulté)"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — ON : itération per-vault, écriture scopée, aucun clobber de `"main"`
// ---------------------------------------------------------------------------

/// À flag ON, la boucle itère les vaults actifs et écrit CHAQUE surface dans SON tenant.
/// La surface de `main` ne contient QUE ses candidats (jamais ceux de `vault-b`) et
/// réciproquement — verrou anti-clobber du second hardcode `upsert_surface(vault_id, …)`.
#[tokio::test]
async fn tick_on_iterates_active_vaults_writes_each_surface_scoped() {
    let (state, idx, _tmp) = build_state().await;

    let main_candidates = seed_vault_corpus(&idx, "main", "M").await;

    idx.provision_vault("vault-b")
        .await
        .expect("provision_vault vault-b");
    let vaultb_candidates = seed_vault_corpus(&idx, "vault-b", "V").await;

    let result = proactive_refresh_tick(&state, &cfg(), true).await;
    assert!(result.is_ok(), "ON ne doit pas échouer : {result:?}");
    assert!(
        result.unwrap() > 0,
        "ON : au moins une surface non vide sur l'ensemble des vaults"
    );

    // main : surface non vide, contenant UNIQUEMENT ses propres candidats.
    let main_surface = surface_ulids(&state, "main")
        .await
        .expect("ON : la surface main est écrite");
    assert!(
        !main_surface.is_empty(),
        "ON : la surface main doit être non vide (non clobbered par vault-b)"
    );
    for cnd in &main_candidates {
        assert!(
            main_surface.contains(cnd),
            "ON : le candidat main {cnd} doit être surfacé dans main"
        );
    }
    for cnd in &vaultb_candidates {
        assert!(
            !main_surface.contains(cnd),
            "ON : aucun candidat de vault-b ({cnd}) ne doit fuiter dans la surface main (clobber)"
        );
    }

    // vault-b : surface non vide écrite dans SON propre tenant, contenant UNIQUEMENT ses
    // candidats. Si le write n'était pas scopé, cette surface aurait écrasé le tenant `main`.
    let vaultb_surface = surface_ulids(&state, "vault-b")
        .await
        .expect("ON : la surface vault-b est écrite dans son propre tenant");
    assert!(
        !vaultb_surface.is_empty(),
        "ON : la surface vault-b doit être non vide"
    );
    for cnd in &vaultb_candidates {
        assert!(
            vaultb_surface.contains(cnd),
            "ON : le candidat vault-b {cnd} doit être surfacé dans vault-b"
        );
    }
    for cnd in &main_candidates {
        assert!(
            !vaultb_surface.contains(cnd),
            "ON : aucun candidat de main ({cnd}) ne doit fuiter dans la surface vault-b"
        );
    }
}
