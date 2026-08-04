//! Tests TDD — JWT Ed25519 + scope-based TTL (T5, R-A1, C1).

use ed25519_dalek::SigningKey;
use gradatum_auth::jwt::{JwtError, JwtService, TokenScope};

fn make_service() -> JwtService {
    let mut csprng = rand::rngs::OsRng;
    let signing = SigningKey::generate(&mut csprng);
    JwtService::new(
        signing,
        "kid-test".into(),
        "gradatum".into(),
        "gradatum".into(),
        3600,
        86400,
    )
}

#[test]
fn sign_and_verify_human_token() {
    let svc = make_service();
    let token = svc
        .sign(
            "user-1",
            &["read".into(), "write".into()],
            TokenScope::Human,
            "main",
        )
        .unwrap();
    let claims = svc.verify(&token).unwrap();
    assert_eq!(claims.sub, "user-1");
    assert_eq!(claims.aud, "gradatum");
    assert_eq!(claims.scopes, vec!["read", "write"]);
    assert_eq!(claims.tenant_id, "main");
}

#[test]
fn human_token_ttl_is_3600s() {
    let svc = make_service();
    let token = svc.sign("u", &[], TokenScope::Human, "main").unwrap();
    let claims = svc.verify(&token).unwrap();
    let ttl = claims.exp - claims.iat;
    assert_eq!(ttl, 3600);
}

#[test]
fn service_token_ttl_is_86400s() {
    let svc = make_service();
    let token = svc.sign("svc", &[], TokenScope::Service, "main").unwrap();
    let claims = svc.verify(&token).unwrap();
    let ttl = claims.exp - claims.iat;
    assert_eq!(ttl, 86400);
}

#[test]
fn tenant_id_preserved_in_claims() {
    // D3-complet (AUTH-T7) : tenant_id propagé fidèlement dans les claims.
    let svc = make_service();
    let token = svc
        .sign("svc", &["admin".into()], TokenScope::Service, "staging")
        .unwrap();
    let claims = svc.verify(&token).unwrap();
    assert_eq!(claims.tenant_id, "staging");
}

#[test]
fn aud_mismatch_rejected() {
    // On signe avec `svc` (audience "gradatum"), puis on vérifie avec un JwtService
    // configuré sur "other-aud" MAIS avec la même clé de signature.
    // Sans la même clé la signature serait invalide — le rejet serait InvalidSignature
    // (Malformed) et non InvalidAudience, car jsonwebtoken vérifie la signature avant les claims.
    // Ce test isole spécifiquement le rejet d'audience.
    let mut csprng = rand::rngs::OsRng;
    let signing = SigningKey::generate(&mut csprng);
    let svc = JwtService::new(
        signing.clone(),
        "kid-test".into(),
        "gradatum".into(),
        "gradatum".into(),
        3600,
        86400,
    );
    let token = svc.sign("u", &[], TokenScope::Human, "main").unwrap();

    // Même clé, kid identique, issuer identique, mais audience différente → InvalidAudience
    let other = JwtService::new(
        signing,
        "kid-test".into(),
        "gradatum".into(),
        "other-aud".into(),
        3600,
        86400,
    );
    assert!(matches!(
        other.verify(&token),
        Err(JwtError::InvalidAudience)
    ));
}

#[test]
fn kid_mismatch_rejected() {
    let mut csprng = rand::rngs::OsRng;
    let s1 = SigningKey::generate(&mut csprng);
    let svc1 = JwtService::new(
        s1.clone(),
        "kid-1".into(),
        "g".into(),
        "g".into(),
        3600,
        86400,
    );
    let token = svc1.sign("u", &[], TokenScope::Human, "main").unwrap();
    let svc2 = JwtService::new(s1, "kid-2".into(), "g".into(), "g".into(), 3600, 86400);
    assert!(matches!(svc2.verify(&token), Err(JwtError::InvalidKid)));
}

#[test]
fn iss_mismatch_rejected() {
    // Même clé, même kid, mais issuer différent → InvalidIssuer (P1 #7).
    let mut csprng = rand::rngs::OsRng;
    let signing = SigningKey::generate(&mut csprng);
    let svc = JwtService::new(
        signing.clone(),
        "kid-test".into(),
        "gradatum".into(),
        "gradatum".into(),
        3600,
        86400,
    );
    let token = svc.sign("u", &[], TokenScope::Human, "main").unwrap();

    let other_iss = JwtService::new(
        signing,
        "kid-test".into(),
        "other-issuer".into(),
        "gradatum".into(),
        3600,
        86400,
    );
    assert!(matches!(
        other_iss.verify(&token),
        Err(JwtError::InvalidIssuer)
    ));
}

#[test]
fn sub_claim_present_in_verified_claims() {
    // Le `sub` est obligatoire et doit être non-vide (P1 #8).
    let svc = make_service();
    let token = svc.sign("agent-1", &[], TokenScope::Human, "main").unwrap();
    let claims = svc.verify(&token).unwrap();
    assert_eq!(claims.sub, "agent-1");
    assert!(!claims.sub.is_empty(), "sub doit être non-vide");
}

#[test]
fn iss_claim_present_in_verified_claims() {
    // Le `iss` est propagé fidèlement dans les claims (P1 #7).
    let svc = make_service();
    let token = svc.sign("u", &[], TokenScope::Human, "main").unwrap();
    let claims = svc.verify(&token).unwrap();
    assert_eq!(claims.iss, "gradatum");
}
