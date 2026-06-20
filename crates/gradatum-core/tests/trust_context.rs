use gradatum_core::trust::{StudioScope, TrustContext};

#[test]
fn unauthenticated_variant() {
    let ctx = TrustContext::Unauthenticated;
    assert!(matches!(ctx, TrustContext::Unauthenticated));
}

#[test]
fn bearer_token_variant_construction() {
    let ctx = TrustContext::BearerToken {
        kid: "agent-backend-2026-05".into(),
        aud: "gradatum".into(),
        sub: "service-backend".into(),
        scopes: vec!["read".into(), "write".into()],
        tenant_id: "main".into(),
    };
    let TrustContext::BearerToken {
        kid,
        aud,
        sub,
        scopes,
        tenant_id,
    } = ctx
    else {
        panic!("expected BearerToken variant");
    };
    assert_eq!(kid, "agent-backend-2026-05");
    assert_eq!(aud, "gradatum");
    assert_eq!(sub, "service-backend");
    assert_eq!(scopes, vec!["read".to_string(), "write".to_string()]);
    assert_eq!(tenant_id, "main");
}

#[test]
fn tenant_id_helper() {
    // D3-complet (AUTH-T7) : tenant_id() retourne Some pour BearerToken.
    let ctx = TrustContext::BearerToken {
        kid: "k".into(),
        aud: "gradatum".into(),
        sub: "svc".into(),
        scopes: vec!["admin".into()],
        tenant_id: "staging".into(),
    };
    assert_eq!(ctx.tenant_id(), Some("staging"));
    assert_eq!(TrustContext::Unauthenticated.tenant_id(), None);
}

#[test]
fn mtls_variant_with_fingerprint() {
    let ctx = TrustContext::Mtls {
        cn: "client.example.org".into(),
        fingerprint_sha256: [0xab; 32],
    };
    let TrustContext::Mtls {
        cn,
        fingerprint_sha256,
    } = ctx
    else {
        panic!()
    };
    assert_eq!(cn, "client.example.org");
    assert_eq!(fingerprint_sha256[0], 0xab);
}

#[test]
fn studio_variant_with_step_up() {
    let now = std::time::SystemTime::now();
    let ctx = TrustContext::Studio {
        user: "ops@example.org".into(),
        scope: StudioScope::ReadOnly,
        step_up_until: Some(now),
    };
    assert!(matches!(ctx, TrustContext::Studio { .. }));
}

#[test]
fn is_authenticated_helper() {
    assert!(!TrustContext::Unauthenticated.is_authenticated());
    assert!(
        TrustContext::BearerToken {
            kid: "k".into(),
            aud: "a".into(),
            sub: "s".into(),
            scopes: vec![],
            tenant_id: "main".into(),
        }
        .is_authenticated()
    );
}
