//! Le principal est typé `TenantId` sur toute la chaîne.
//!
//! Prouve trois propriétés (sous flag `multi_tenant` OFF, byte-identical) :
//! 1. `TrustContext::tenant_id()` retourne `Option<&TenantId>` — le principal est
//!    typé au compile-time (plus de `&str` nu ambigu avec le namespace `VaultId`).
//! 2. La valeur reste `"main"` byte-identique.
//! 3. Le claim JWT `tenant_id` sérialisé en JSON reste un `String` nu
//!    (`#[serde(transparent)]` → wire inchangé, aucune migration).

use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::scope::TenantId;
use gradatum_core::trust::TrustContext;

/// Un `BearerToken` construit depuis les claims d'un JWT signé+vérifié (miroir de
/// `middleware::extract_trust`) expose un `tenant_id()` typé `TenantId`, valeur `"main"`.
#[test]
fn bearer_from_jwt_exposes_typed_tenant_id() {
    let svc = JwtService::new_ephemeral();
    let token = svc
        .sign(
            "agent-1",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign doit réussir avec une clé éphémère");
    let claims = svc
        .verify(&token)
        .expect("verify immédiat ne peut pas échouer");

    // (3) Le claim JWT reste un `String` nu côté wire (byte-identical).
    assert_eq!(claims.tenant_id, "main");

    // Construction du contexte, exactement comme le middleware : le `String` du claim
    // (déjà validé par la vérification JWT) est enveloppé dans le newtype principal.
    let trust = TrustContext::BearerToken {
        kid: svc.kid().to_string(),
        aud: claims.aud,
        sub: gradatum_core::scope::AgentId::new(claims.sub),
        scopes: claims.scopes,
        tenant_id: TenantId::new(claims.tenant_id),
        jti: Some(claims.jti),
    };

    // (1) Typage compile-time : l'accesseur retourne `Option<&TenantId>`, pas `Option<&str>`.
    let tenant: Option<&TenantId> = trust.tenant_id();

    // (2) Valeur `"main"` byte-identique, quel que soit l'angle d'observation.
    assert_eq!(tenant.map(TenantId::as_str), Some("main"));
    assert_eq!(tenant, Some(&TenantId::new("main")));
}

/// Le claim JWT `tenant_id` sérialise en JSON comme un `String` nu — champ `Claims`
/// inchangé (byte-identical wire), la conversion vers `TenantId` a lieu en aval.
#[test]
fn jwt_claim_tenant_id_serialises_as_bare_string() {
    // `Claims` est `#[non_exhaustive]` : hors du crate d'origine (gradatum-auth) la
    // construction par littéral est interdite. On l'obtient par le VRAI chemin — le
    // seul par lequel un consommateur externe reçoit des `Claims` : signer puis
    // vérifier un jeton. `tenant_id` reste `"main"`, ce que le test éprouve.
    let svc = JwtService::new_ephemeral();
    let token = svc
        .sign(
            "agent-1",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign doit réussir avec une clé éphémère");
    let claims = svc
        .verify(&token)
        .expect("verify immédiat ne peut pas échouer");
    let json = serde_json::to_string(&claims).expect("sérialisation des claims");
    assert!(
        json.contains("\"tenant_id\":\"main\""),
        "le claim JWT tenant_id doit rester un String nu, obtenu : {json}"
    );
}
