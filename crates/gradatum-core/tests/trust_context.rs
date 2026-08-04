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
        jti: None,
    };
    let TrustContext::BearerToken {
        kid,
        aud,
        sub,
        scopes,
        tenant_id,
        jti,
    } = ctx
    else {
        panic!("expected BearerToken variant");
    };
    assert_eq!(kid, "agent-backend-2026-05");
    assert_eq!(aud, "gradatum");
    assert_eq!(sub, "service-backend");
    assert_eq!(scopes, vec!["read".to_string(), "write".to_string()]);
    assert_eq!(tenant_id, "main");
    assert_eq!(jti, None);
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
        jti: None,
    };
    // `tenant_id()` typé `Option<&TenantId>` (Groupe B Task 3) : on compare via `as_str`
    // (valeur byte-identique) ; `None` reste `None` sur les variantes sans principal.
    assert_eq!(
        ctx.tenant_id().map(gradatum_core::scope::TenantId::as_str),
        Some("staging")
    );
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
            jti: None,
        }
        .is_authenticated()
    );
}

/// L'accesseur `subject()` rend une identité **typée**, et le `sub` reste un
/// `String` nu sur le wire.
///
/// Discriminant (B6′a) : avec l'ancien `sub: String`, `subject()` rendait
/// `Option<&str>` — l'annotation `Option<&AgentId>` ci-dessous ne compile pas, et
/// la comparaison avec un `TenantId` de même contenu ne compile pas non plus.
/// C'est la propriété visée : `agent_id` et `tenant_id` cessent d'être
/// interchangeables une fois sortis du credential.
#[test]
fn subject_is_a_typed_agent_id_distinct_from_the_tenant() {
    use gradatum_core::scope::{AgentId, TenantId};

    let ctx = TrustContext::BearerToken {
        kid: "k".into(),
        aud: "gradatum".into(),
        sub: "main-agent".into(),
        scopes: vec!["write".into()],
        tenant_id: "main".into(),
        jti: None,
    };

    // (1) Typage compile-time : l'accesseur rend `Option<&AgentId>`, pas `Option<&str>`.
    let subject: Option<&AgentId> = ctx.subject();
    assert_eq!(subject, Some(&AgentId::new("main-agent")));

    // (2) Les deux dimensions restent distinctes, même valeur textuelle mise à part.
    let tenant: Option<&TenantId> = ctx.tenant_id();
    assert_eq!(tenant.map(TenantId::as_str), Some("main"));
    assert_ne!(
        subject.map(AgentId::as_str),
        tenant.map(TenantId::as_str),
        "agent_id et tenant_id ne désignent pas la même chose"
    );

    // (3) Aucune variante sans credential ne porte d'identité d'agent.
    assert_eq!(TrustContext::Unauthenticated.subject(), None);
}

/// Un `TrustContext::BearerToken` sérialisé reste **byte-identique** au format
/// d'avant le typage de `sub` (`#[serde(transparent)]`, aucune migration de wire).
#[test]
fn bearer_token_serialises_sub_as_a_bare_json_string() {
    let ctx = TrustContext::BearerToken {
        kid: "k".into(),
        aud: "gradatum".into(),
        sub: "claude-code".into(),
        scopes: vec!["read".into()],
        tenant_id: "main".into(),
        jti: None,
    };
    let json = serde_json::to_value(&ctx).expect("sérialisation TrustContext");
    assert_eq!(
        json["BearerToken"]["sub"],
        serde_json::json!("claude-code"),
        "`sub` doit rester une string JSON nue"
    );

    // Un payload écrit AVANT le typage (sub String) se relit inchangé.
    let pre_typing = serde_json::json!({
        "BearerToken": {
            "kid": "k",
            "aud": "gradatum",
            "sub": "claude-code",
            "scopes": ["read"],
            "tenant_id": "main",
            "jti": null
        }
    });
    let back: TrustContext =
        serde_json::from_value(pre_typing).expect("payload pré-typage doit se désérialiser");
    assert_eq!(back, ctx);
}
