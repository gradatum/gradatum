use gradatum_acl_policy::{AclDecision, AclEngine, AclOp};
use gradatum_core::trust::{StudioScope, TrustContext};

fn human_admin() -> TrustContext {
    TrustContext::Studio {
        user: "maintainer".into(),
        scope: StudioScope::Admin,
        step_up_until: None,
    }
}

fn agent(sub: &str, scopes: &[&str]) -> TrustContext {
    TrustContext::BearerToken {
        kid: format!("kid-{sub}"),
        aud: "gradatum".into(),
        sub: sub.into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        tenant_id: "main".into(),
    }
}

fn preset() -> AclEngine {
    // Chargé depuis la fixture de test locale — copie de crates/gradatum-admin/presets/hierarchical.toml.
    AclEngine::from_preset_str(include_str!("fixtures/hierarchical.toml")).unwrap()
}

#[test]
fn t01_human_reads_main_main() {
    assert_allow(preset().evaluate(&human_admin(), AclOp::Read, "main/main"));
}

#[test]
fn t02_human_reads_personal_classified() {
    assert_allow(preset().evaluate(&human_admin(), AclOp::Read, "main/personal-classified"));
}

#[test]
fn t03_main_agent_reads_personal_classified_denied() {
    assert_deny(preset().evaluate(
        &agent("main-agent", &["read"]),
        AclOp::Read,
        "main/personal-classified",
    ));
}

#[test]
fn t04_sub_agent_writer_writes_own_project_ok() {
    assert_allow(preset().evaluate(
        &agent("sub-agent-a", &["write"]),
        AclOp::Write,
        "project-a/backend",
    ));
}

#[test]
fn t05_sub_agent_writer_writes_cross_project_denied() {
    assert_deny(preset().evaluate(
        &agent("sub-agent-a", &["write"]),
        AclOp::Write,
        "vox/backend",
    ));
}

#[test]
fn t06_validator_reads_cross_project_ok() {
    assert_allow(preset().evaluate(
        &agent("validator", &["read"]),
        AclOp::Read,
        "project-a/backend",
    ));
}

#[test]
fn t07_validator_writes_cross_project_denied() {
    assert_deny(preset().evaluate(
        &agent("validator", &["read"]),
        AclOp::Write,
        "project-a/backend",
    ));
}

#[test]
fn t08_expert_off_theme_denied() {
    assert_deny(preset().evaluate(&agent("expert-rust", &["read"]), AclOp::Read, "design/ui"));
}

#[test]
fn t09_unauthenticated_denied_everywhere() {
    assert_deny(preset().evaluate(&TrustContext::Unauthenticated, AclOp::Read, "main/main"));
}

#[test]
fn t10_unknown_locus_default_deny() {
    assert_deny(preset().evaluate(&agent("unknown", &["read"]), AclOp::Read, "main/main"));
}

#[test]
fn t11_negation_overrides_allow() {
    assert_deny(preset().evaluate(
        &agent("main-agent", &["read"]),
        AclOp::Read,
        "main/personal-classified",
    ));
}

#[test]
fn t12_studio_with_admin_scope_reads_admin_locus() {
    assert_allow(preset().evaluate(&human_admin(), AclOp::Write, "main/main"));
}

fn assert_allow(d: AclDecision) {
    assert!(matches!(d, AclDecision::Allow), "expected Allow, got {d:?}");
}

fn assert_deny(d: AclDecision) {
    assert!(
        matches!(d, AclDecision::DenyExplicit | AclDecision::DenyImplicit),
        "expected Deny, got {d:?}"
    );
}
