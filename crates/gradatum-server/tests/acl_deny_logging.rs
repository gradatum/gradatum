//! Le refus ACL ne doit plus être muet (B6′b, livrable 4).
//!
//! ## Le défaut mesuré
//!
//! Sur la route qui les enchaîne, `require_read_grant`, `require_write_grant`,
//! `require_active_target` et `write_scope_allowed` loggent tous leur refus. L'évaluation
//! ACL, elle, ne loggait rien : un `403` pouvait donc sortir sans **aucune** trace — ni
//! corps de réponse, ni ligne de journal. L'opérateur voyait un refus indistinguable d'une
//! panne, ce qui a coûté une journée d'instruction sur l'incident `engine` du 2026-07-27.
//! L'asymétrie était pure : même barrière, même statut, un seul barreau silencieux.
//!
//! ## Les trois sites
//!
//! | Site | Chemin | Atteint ici par |
//! |---|---|---|
//! | `effective_read_vault` | read cross-vault, `multi_tenant` ON | `vault_search_impl` |
//! | `lessons_recall_impl` | read du corpus `lessons-learned` partagé | direct |
//! | `vault_write_impl` | pendant write | direct |
//!
//! ## Ce que les tests exigent de la ligne
//!
//! Le `sub` **et** le locus évalué. Une ligne qui dirait seulement « refus ACL » ne
//! réduirait pas le temps de diagnostic : ce sont ces deux valeurs qu'il faut confronter
//! au preset pour conclure. Chaque test asserte donc les deux, pas la présence du log.

use std::sync::{Arc, Mutex};

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::error::GradatumError;
use gradatum_core::scope::{AgentId, TenantId};
use gradatum_core::trust::TrustContext;
use gradatum_server::api_v1::logic;
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::AppState;

/// Preset qui n'accorde rien à `engine` — l'identité est déclarée, elle n'a aucun droit.
///
/// Le cas exact de l'incident : la clé s'authentifie, le refus vient de l'ACL, et rien
/// dans le journal ne disait sur quel locus.
const PRESET: &str = r#"
[[consumer]]
identity = "other-agent"
read_patterns = ["main/**"]
write_patterns = ["main/**"]
"#;

// ── Capture des logs ─────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("tampon non empoisonné")).into_owned()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("tampon empoisonné"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Exécute `f` sous un subscriber local et rend les logs émis.
async fn capture<F, Fut, T>(f: F) -> (T, String)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let out = f().await;
    (out, buffer.contents())
}

/// Bearer `engine` sur le tenant `main` — authentifié, sans droit ACL.
fn engine_trust() -> TrustContext {
    TrustContext::BearerToken {
        kid: "k1".to_owned(),
        aud: "gradatum".to_owned(),
        sub: AgentId::new("engine"),
        scopes: vec!["write".to_owned(), "vault_read".to_owned()],
        tenant_id: TenantId::new("main"),
        jti: None,
    }
}

fn state(multi_tenant: bool) -> AppState {
    let acl = AclEngine::from_preset_str(PRESET).expect("preset valide");
    AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl).with_server_config(ServerConfig {
        multi_tenant: MultiTenantConfig {
            enabled: multi_tenant,
        },
        ..ServerConfig::default()
    })
}

/// Le refus doit rester un 403 : ce lot ajoute une trace, il ne change aucun verdict.
fn assert_forbidden<T: std::fmt::Debug>(res: Result<T, GradatumError>) {
    match res {
        Err(GradatumError::Forbidden(_)) => {}
        other => panic!("403 attendu, obtenu : {other:?}"),
    }
}

// ── Site 1 : effective_read_vault (via vault_search_impl, multi_tenant ON) ────

/// Le refus ACL du chemin read cross-vault porte le `sub` et le locus.
///
/// Discriminant : avant B6′b ce chemin sortait un `403` **sans aucune ligne**, alors que
/// les trois barreaux suivants de la même fonction loggaient déjà les leurs. Le test
/// échoue sur `0e0f615a`.
#[tokio::test]
async fn effective_read_vault_logs_the_denied_identity_and_locus() {
    let st = state(true);
    let trust = engine_trust();
    let mut req = gradatum_dto::VaultSearchRequest::new("peu importe".into());
    req.section = Some("decisions".into());

    let (res, logs) = capture(|| logic::vault_search_impl(&st, &trust, req)).await;

    assert_forbidden(res);
    assert!(
        logs.contains("engine"),
        "la ligne doit nommer l'identité refusée. Obtenu : {logs}"
    );
    assert!(
        logs.contains("main/decisions"),
        "la ligne doit porter le LOCUS évalué — sans lui, elle dit qu'un refus a eu lieu \
         sans dire sur quoi. Obtenu : {logs}"
    );
}

// ── Site 2 : lessons_recall_impl ─────────────────────────────────────────────

/// Le refus ACL du corpus `lessons-learned` porte le `sub` et le locus.
#[tokio::test]
async fn lessons_recall_logs_the_denied_identity_and_locus() {
    let st = state(false);
    let trust = engine_trust();
    let mut params = gradatum_dto::LessonsRecallRequest::new("deploy".into());
    params.limit = Some(3);

    let (res, logs) = capture(|| logic::lessons_recall_impl(&st, &trust, params)).await;

    assert_forbidden(res);
    assert!(
        logs.contains("engine"),
        "la ligne doit nommer l'identité refusée. Obtenu : {logs}"
    );
    assert!(
        logs.contains("main/lessons-learned"),
        "la ligne doit porter le locus du corpus partagé. Obtenu : {logs}"
    );
}

// ── Site 3 : vault_write_impl (pendant write) ────────────────────────────────

/// Le refus ACL du chemin write porte le `sub` et le locus.
///
/// Discriminant : l'audit `auth_failure` émis au même endroit n'est PAS un substitut —
/// il part vers le sink JSONL, pas vers le journal du service, et ne porte pas le locus.
/// Le diagnostic d'exploitation se fait sur le journal.
#[tokio::test]
async fn vault_write_logs_the_denied_identity_and_locus() {
    let st = state(false);
    let trust = engine_trust();
    let mut req = gradatum_dto::VaultWriteRequest::new("Titre".into(), "Corps.".into());
    req.section_hint = Some("decisions".into());

    let (res, logs) = capture(|| {
        logic::vault_write_impl(
            &st,
            &trust,
            req,
            "req-1",
            logic::FeatureWriteAuthority::External,
        )
    })
    .await;

    assert_forbidden(res);
    assert!(
        logs.contains("engine"),
        "la ligne doit nommer l'identité refusée. Obtenu : {logs}"
    );
    assert!(
        logs.contains("main/main"),
        "la ligne doit porter le locus write évalué. Obtenu : {logs}"
    );
}

// ── Contre-épreuve : pas de bruit sur le cas autorisé ────────────────────────

/// Un accès AUTORISÉ n'émet aucune ligne de refus.
///
/// Discriminant : c'est ce qui sépare une trace utile d'un journal noyé. Un helper
/// appelé inconditionnellement (ou branché dans `AclEngine::evaluate`, qui est aussi
/// traversé par des chemins de filtrage) produirait ici une ligne — et rendrait le
/// signal illisible exactement au moment où on en a besoin.
#[tokio::test]
async fn an_allowed_request_emits_no_denial_line() {
    let st = state(false);
    let trust = TrustContext::BearerToken {
        kid: "k1".to_owned(),
        aud: "gradatum".to_owned(),
        sub: AgentId::new("other-agent"),
        scopes: vec!["vault_read".to_owned()],
        tenant_id: TenantId::new("main"),
        jti: None,
    };
    let mut params = gradatum_dto::LessonsRecallRequest::new("deploy".into());
    params.limit = Some(3);

    let (_res, logs) = capture(|| logic::lessons_recall_impl(&st, &trust, params)).await;

    assert!(
        !logs.contains("acl deny"),
        "aucune ligne de refus ne doit être émise sur un accès autorisé. Obtenu : {logs}"
    );
}
