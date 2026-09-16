//! F-160 — ce que la frontière de détection ne doit PAS ouvrir côté serveur.
//!
//! ## Le défaut mesuré
//!
//! La passe de détection est pilotée par un budget et une **surface**. La surface est
//! un axe de coût : la voie synchrone (`message_send`) paie la latence sur la requête,
//! la voie asynchrone l'absorbe dans la file. Deux façons pour un client d'aller
//! chercher cette différence — et aucune ne doit aboutir :
//!
//! 1. **Choisir la surface** en posant `section_hint = "messaging"` sur l'écriture
//!    générique. La section Messaging est un canal inter-agents dont le locus d'écriture
//!    est `messaging/<destinataire>` et dont la politique d'émission (C4) est évaluée sur
//!    CE locus ; `vault_write` n'évalue que `{tenant}/main`. Accepter le hint ouvrirait
//!    une écriture dans Messaging sans passer la politique d'émission — et ferait
//!    basculer la détection en synchrone sur un chemin non prévu pour.
//! 2. **Choisir le namespace** en posant `tenant_id = "code-…"` dans le corps, pour
//!    viser un vault que la garde en dur dispense de détection. Le tenant est dérivé du
//!    JWT, jamais du corps.
//!
//! Ces tests appellent les handlers de logique directement et observent donc l'erreur
//! TYPÉE (`InvalidInput`, `Forbidden`), pas ce qui part sur le fil.
//!
//! Le preset ci-dessous **autorise** `engine` en écriture sur `main/main` : la garde de
//! section est donc bien ce qui refuse le premier cas, et non l'ACL. Un preset refusant
//! tout rendrait le test aveugle — les deux refus seraient indistinguables.

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::error::GradatumError;
use gradatum_core::scope::{AgentId, TenantId};
use gradatum_core::trust::TrustContext;
use gradatum_dto::VaultWriteRequest;
use gradatum_server::api_v1::logic::{self, FeatureWriteAuthority};
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::AppState;

/// Preset qui autorise `engine` à écrire sur `main/main`.
const PRESET: &str = r#"
[[consumer]]
identity = "engine"
read_patterns = ["main/**"]
write_patterns = ["main/**"]
"#;

/// Le namespace `code-*` qu'un client ne doit pas pouvoir se donner.
const CODE_NAMESPACE: &str = "code-evil";

fn engine_trust() -> TrustContext {
    TrustContext::BearerToken {
        kid: "k1".to_owned(),
        aud: "gradatum".to_owned(),
        sub: AgentId::new("engine"),
        scopes: vec!["vault_read".to_owned(), "vault_write".to_owned()],
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

async fn write(
    multi_tenant: bool,
    section_hint: Option<&str>,
    tenant_id: Option<&str>,
) -> Result<(), GradatumError> {
    let st = state(multi_tenant);
    let mut req = VaultWriteRequest::new("titre".into(), "corps".into());
    req.section_hint = section_hint.map(str::to_owned);
    req.tenant_id = tenant_id.map(TenantId::new);
    logic::vault_write_impl(
        &st,
        &engine_trust(),
        req,
        "req-f160",
        FeatureWriteAuthority::External,
    )
    .await
    .map(|_| ())
}

// ── (a) la surface ne se choisit pas par le corps ────────────────────────────

#[tokio::test]
async fn a_client_section_hint_cannot_select_the_sync_surface() {
    let res = write(false, Some("messaging"), None).await;
    match res {
        Err(GradatumError::InvalidInput(msg)) => assert!(
            msg.contains("messaging"),
            "l'erreur doit nommer la section refusée — c'est ce que la surface MCP relaie. \
             Obtenu : {msg}"
        ),
        other => panic!(
            "un `section_hint` client ne doit pas ouvrir la surface synchrone : la section \
             Messaging s'écrit par `message_send`, où la politique d'émission C4 est évaluée \
             sur `messaging/<destinataire>`. Obtenu : {other:?}"
        ),
    }
}

/// Flag ON : le hint Messaging ne doit pas aboutir non plus — mais la **propriété
/// testée est le refus, pas son motif**. En multi-tenant la voie write exige un grant
/// enregistré (et un scope write) *avant* d'atteindre la garde de section : sous ce
/// preset sans grant, c'est un `Forbidden` qui sort, pas l'`InvalidInput` du cas
/// mono-tenant. Exiger ici le même code serait tester l'ordre des barreaux d'une autre
/// frontière que celle de cette tâche.
#[tokio::test]
async fn a_client_section_hint_cannot_select_the_sync_surface_with_multi_tenant_on() {
    let res = write(true, Some("messaging"), None).await;
    assert!(
        matches!(
            res,
            Err(GradatumError::Forbidden(_) | GradatumError::InvalidInput(_))
        ),
        "flag ON : le hint Messaging ne doit pas aboutir, obtenu {res:?}"
    );
}

// ── (b) le namespace d'écriture ne se choisit pas par le corps ───────────────

#[tokio::test]
async fn an_ordinary_consumer_cannot_obtain_a_code_prefixed_write_vault() {
    let res = write(false, None, Some(CODE_NAMESPACE)).await;
    match res {
        Err(GradatumError::Forbidden(_)) => {}
        other => panic!(
            "le tenant d'écriture est dérivé du JWT, jamais du corps : viser un vault \
             `code-*` — que la garde en dur dispense de détection — doit être refusé. \
             Obtenu : {other:?}"
        ),
    }
}

#[tokio::test]
async fn an_ordinary_consumer_cannot_obtain_a_code_prefixed_write_vault_with_multi_tenant_on() {
    let res = write(true, None, Some(CODE_NAMESPACE)).await;
    assert!(
        matches!(res, Err(GradatumError::Forbidden(_))),
        "flag ON : la divergence de tenant est refusée avant toute lecture de grant, \
         obtenu {res:?}"
    );
}

/// Le cas nominal n'est pas emporté par les gardes ci-dessus : un corps sans
/// `tenant_id` et sans `section_hint` traverse la résolution de tenant. Le refus
/// observable ici vient de la file absente (AppState nu), pas d'une garde de
/// détection — ce qui prouve que la requête a bien atteint l'enfilement.
#[tokio::test]
async fn an_ordinary_write_is_not_refused_by_the_detection_guards() {
    let res = write(false, None, None).await;
    assert!(
        !matches!(res, Err(GradatumError::Forbidden(_))),
        "une écriture ordinaire ne doit pas être refusée par la frontière de détection, \
         obtenu {res:?}"
    );
}
