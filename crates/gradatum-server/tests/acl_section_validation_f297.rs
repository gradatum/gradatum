//! Une section inconnue doit produire une erreur de REQUÊTE, jamais un refus d'accès.
//!
//! ## Le défaut mesuré
//!
//! La section arrivant dans le corps d'une requête de lecture n'était validée nulle part :
//! elle servait telle quelle à construire le locus ACL (`{tenant}/{section}`), puis de
//! filtre. Une section qui n'existe pas — faute de frappe, nom inventé, ou nom d'une
//! famille que l'appelant croit exister — produisait donc un verdict ACL sur un chemin
//! sans existence. Deux issues, toutes deux trompeuses :
//!
//! - preset permissif : `200` avec zéro résultat — « il n'y a rien » là où la vérité est
//!   « ce nom ne désigne rien » ;
//! - preset restrictif : `403` — « vous n'avez pas le droit » là où la vérité est
//!   « votre requête est malformée ».
//!
//! Un `403` sur une section inexistante est la pire des deux : il envoie l'appelant
//! instruire ses droits, alors que le défaut est dans sa requête.
//!
//! ## Ce que les tests exigent
//!
//! Le **statut**, pas un log : `InvalidInput`, que la couche HTTP rend en `400`. Ces tests
//! appellent les handlers de logique directement et observent donc l'erreur TYPÉE, pas ce
//! qui part sur le fil : sur la surface HTTP le `400` sort nu (`err_to_status` ne rend
//! qu'un code), c'est la surface MCP qui relaie le message en `invalid_params`. L'assertion
//! sur le message porte donc sur l'erreur, et le libellé ne promet rien d'un corps HTTP.
//!
//! Le preset employé ci-dessous **refuse tout** à l'identité testée. C'est le discriminant :
//! sur le code d'avant, une section inconnue traversait la garde ACL et ressortait en
//! `Forbidden`. Obtenir `InvalidInput` prouve donc que la validation s'est produite **avant**
//! l'évaluation ACL — et non simplement qu'un message a changé. Symétriquement, les cas
//! nominaux (section canonique, section absente) doivent continuer à ressortir en
//! `Forbidden` : si la garde les convertissait en `400`, elle aurait débordé de son rôle.

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::error::GradatumError;
use gradatum_core::scope::{AgentId, TenantId};
use gradatum_core::section::Section;
use gradatum_core::trust::TrustContext;
use gradatum_server::api_v1::logic;
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::AppState;

/// Preset qui n'accorde rien à `engine` : tout accès autorisé ressort en `Forbidden`.
const PRESET: &str = r#"
[[consumer]]
identity = "other-agent"
read_patterns = ["main/**"]
write_patterns = ["main/**"]
"#;

/// La section inconnue employée par tous les cas rouges.
const UNKNOWN: &str = "personal-classified";

fn engine_trust() -> TrustContext {
    TrustContext::BearerToken {
        kid: "k1".to_owned(),
        aud: "gradatum".to_owned(),
        sub: AgentId::new("engine"),
        scopes: vec!["vault_read".to_owned()],
        tenant_id: TenantId::new("main"),
        jti: None,
    }
}

fn state(multi_tenant: bool) -> AppState {
    let acl = AclEngine::from_preset_str(PRESET).expect("preset de test valide");
    AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl).with_server_config(ServerConfig {
        multi_tenant: MultiTenantConfig {
            enabled: multi_tenant,
        },
        ..ServerConfig::default()
    })
}

// ── Assertions ───────────────────────────────────────────────────────────────

/// La requête est refusée pour ce qu'elle est, et le corps nomme la valeur fautive.
fn assert_unknown_section<T: std::fmt::Debug>(res: Result<T, GradatumError>, site: &str) {
    match res {
        Err(GradatumError::InvalidInput(msg)) => assert!(
            msg.contains(UNKNOWN),
            "[{site}] l'erreur doit nommer la section reçue — c'est ce que la surface MCP \
             relaie à l'appelant. Obtenu : {msg}"
        ),
        other => panic!(
            "[{site}] 400 attendu sur une section inconnue — un 403 renvoie l'appelant \
             instruire ses droits alors que le défaut est dans sa requête. Obtenu : {other:?}"
        ),
    }
}

/// Le cas nominal ressort inchangé : l'ACL a été atteinte, la garde n'a rien intercepté.
fn assert_reached_acl<T: std::fmt::Debug>(res: Result<T, GradatumError>, site: &str) {
    match res {
        Err(GradatumError::Forbidden(_)) => {}
        other => panic!(
            "[{site}] la garde doit laisser passer ce cas jusqu'à l'évaluation ACL \
             (403 sous ce preset). Obtenu : {other:?}"
        ),
    }
}

// ── Appels ───────────────────────────────────────────────────────────────────

async fn search(
    multi_tenant: bool,
    all_readable: bool,
    section: Option<&str>,
) -> Result<gradatum_server::api_v1::dto::VaultSearchResponse, GradatumError> {
    let st = state(multi_tenant);
    let mut req = gradatum_dto::VaultSearchRequest::new("peu importe".into());
    req.section = section.map(str::to_owned);
    req.all_readable = all_readable;
    logic::vault_search_impl(&st, &engine_trust(), req).await
}

async fn read(
    section: Option<&str>,
) -> Result<gradatum_server::api_v1::dto::VaultReadResponse, GradatumError> {
    let st = state(false);
    let mut req =
        gradatum_dto::VaultReadRequest::new("decisions/01ARZ3NDEKTSV4RRFFQ69G5FAV".into());
    req.section = section.map(str::to_owned);
    logic::vault_read_impl(&st, &engine_trust(), req).await
}

async fn list(
    section: Option<&str>,
) -> Result<gradatum_server::api_v1::dto::VaultListResponse, GradatumError> {
    let st = state(false);
    let mut req = gradatum_dto::VaultListRequest::default();
    req.section = section.map(str::to_owned);
    logic::vault_list_impl(&st, &engine_trust(), req).await
}

async fn context(
    section: Option<&str>,
) -> Result<gradatum_server::api_v1::dto::VaultContextResponse, GradatumError> {
    let st = state(false);
    let mut req = gradatum_dto::VaultContextRequest::new("peu importe".into());
    req.section = section.map(str::to_owned);
    logic::vault_context_impl(&st, &engine_trust(), req).await
}

// ── (a) section inconnue → 400, sur les quatre handlers ──────────────────────

#[tokio::test]
async fn search_rejects_an_unknown_section_as_a_bad_request() {
    assert_unknown_section(search(false, false, Some(UNKNOWN)).await, "vault_search");
}

/// Chemin mono `multi_tenant` ON : la garde doit précéder `effective_read_vault`, qui
/// construit le locus lui-même — une garde posée trop bas laisserait ce chemin découvert.
#[tokio::test]
async fn search_rejects_an_unknown_section_before_the_cross_vault_resolver() {
    assert_unknown_section(
        search(true, false, Some(UNKNOWN)).await,
        "vault_search multi_tenant ON",
    );
}

/// Chemin multivault : troisième construction de locus du même handler, même exigence.
#[tokio::test]
async fn search_rejects_an_unknown_section_before_the_multivault_resolver() {
    assert_unknown_section(
        search(true, true, Some(UNKNOWN)).await,
        "vault_search all_readable",
    );
}

#[tokio::test]
async fn read_rejects_an_unknown_section_as_a_bad_request() {
    assert_unknown_section(read(Some(UNKNOWN)).await, "vault_read");
}

#[tokio::test]
async fn list_rejects_an_unknown_section_as_a_bad_request() {
    assert_unknown_section(list(Some(UNKNOWN)).await, "vault_list");
}

#[tokio::test]
async fn context_rejects_an_unknown_section_as_a_bad_request() {
    assert_unknown_section(context(Some(UNKNOWN)).await, "vault_context");
}

// ── (b) section canonique → statut nominal inchangé ──────────────────────────

#[tokio::test]
async fn search_lets_a_canonical_section_reach_the_acl() {
    assert_reached_acl(
        search(false, false, Some("decisions")).await,
        "vault_search",
    );
}

#[tokio::test]
async fn read_lets_a_canonical_section_reach_the_acl() {
    assert_reached_acl(read(Some("decisions")).await, "vault_read");
}

#[tokio::test]
async fn list_lets_a_canonical_section_reach_the_acl() {
    assert_reached_acl(list(Some("decisions")).await, "vault_list");
}

#[tokio::test]
async fn context_lets_a_canonical_section_reach_the_acl() {
    assert_reached_acl(context(Some("decisions")).await, "vault_context");
}

// ── (c) section absente → statut nominal inchangé (locus `{tenant}/main`) ─────

#[tokio::test]
async fn search_lets_an_absent_section_reach_the_acl() {
    assert_reached_acl(search(false, false, None).await, "vault_search");
}

#[tokio::test]
async fn read_lets_an_absent_section_reach_the_acl() {
    assert_reached_acl(read(None).await, "vault_read");
}

#[tokio::test]
async fn list_lets_an_absent_section_reach_the_acl() {
    assert_reached_acl(list(None).await, "vault_list");
}

#[tokio::test]
async fn context_lets_an_absent_section_reach_the_acl() {
    assert_reached_acl(context(None).await, "vault_context");
}

// ── La garde est adossée au SSOT, pas à une liste recopiée ───────────────────

/// Aucune des sections canoniques n'est rejetée.
///
/// Discriminant : une garde écrite sur une liste recopiée à la main dériverait dès qu'une
/// variante est ajoutée au SSOT, et refuserait une section parfaitement valide. Ce test
/// balaie `Section::ALL` — il rougit le jour où les deux divergent, sans avoir à être
/// tenu à jour.
#[tokio::test]
async fn no_canonical_section_is_rejected() {
    for section in Section::ALL {
        assert_reached_acl(
            search(false, false, Some(section.as_str())).await,
            section.as_str(),
        );
    }
}

/// La valeur rejetée est renvoyée bornée, jamais telle quelle.
///
/// Discriminant : le message traverse les `tracing::error!(err = %e)` des handlers de
/// lecture et ressort côté MCP. Sans borne, la seule limite est la taille du corps de
/// requête — une section de plusieurs centaines de kilo-octets se recopierait dans le
/// journal. Le test envoie une section bien plus longue que la borne et vérifie que le
/// message reste court, sans dépendre de sa valeur exacte.
#[tokio::test]
async fn a_rejected_section_is_echoed_back_bounded() {
    let long = "z".repeat(10_000);
    match search(false, false, Some(&long)).await {
        Err(GradatumError::InvalidInput(msg)) => assert!(
            msg.len() < 500,
            "le message doit rester borné quelle que soit la taille de la section reçue. \
             Longueur obtenue : {}",
            msg.len()
        ),
        other => panic!("400 attendu sur une section inconnue. Obtenu : {other:?}"),
    }
}
