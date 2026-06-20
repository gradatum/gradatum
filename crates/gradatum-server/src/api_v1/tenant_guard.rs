//! Reconciliation of the request body tenant against the JWT tenant.
//!
//! ## Invariant
//!
//! While the vault is a single physical vault (`"main"`), the **effective tenant** of a
//! request is DERIVED from the JWT — never from the request body. The `tenant_id` field
//! in request DTOs is kept for deserialisation backward-compatibility, but it no longer
//! drives any server-side behaviour: it is only verified for consistency.
//!
//! ## Behaviour
//!
//! [`effective_tenant`] returns the tenant to use when building the ACL locus and
//! driving the index. It returns `403 FORBIDDEN` when:
//! - The context carries no tenant (`Mtls` / `Studio` / `Unauthenticated`) — these
//!   identities must not access the vault via a tenant locus.
//! - The body `tenant_id` diverges from the JWT tenant (attempt to target an arbitrary locus).
//!
//! `403` (not `422`) is used because this is an authorisation refusal, consistent with
//! the rest of the vault boundary (`vault_forget`, ACL deny). Restrictive-only.

use axum::http::StatusCode;
use gradatum_core::trust::TrustContext;

/// Returns the effective tenant (derived from the JWT) for a vault request, or `403`.
///
/// `body_tenant` is the `tenant_id` field from the request DTO — verified for consistency
/// but never used as a source of truth.
///
/// # Errors
/// - `StatusCode::FORBIDDEN` if the context carries no tenant (non-`BearerToken`)
///   or if `body_tenant` diverges from the JWT tenant.
#[must_use = "le tenant effectif retourné doit remplacer req.tenant_id pour le locus/index"]
pub(crate) fn effective_tenant<'a>(
    trust: &'a TrustContext,
    body_tenant: &str,
) -> Result<&'a str, StatusCode> {
    let jwt_tenant = match trust.tenant_id() {
        Some(t) => t,
        None => {
            // Mtls/Studio/Unauthenticated : pas de tenant porté → pas d'accès vault.
            tracing::warn!(
                "tenant_guard: contexte sans tenant (non-Bearer) — accès vault refusé 403"
            );
            return Err(StatusCode::FORBIDDEN);
        }
    };

    if body_tenant != jwt_tenant {
        tracing::warn!(
            body_tenant = %body_tenant,
            jwt_tenant = %jwt_tenant,
            "tenant_guard: tenant_id du body diverge du JWT — 403 (tenant dérivé du JWT)"
        );
        return Err(StatusCode::FORBIDDEN);
    }

    Ok(jwt_tenant)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::trust::{StudioScope, TrustContext};

    fn bearer(tenant: &str) -> TrustContext {
        TrustContext::BearerToken {
            kid: "k".into(),
            aud: "gradatum".into(),
            sub: "agent".into(),
            scopes: vec!["write".into()],
            tenant_id: tenant.into(),
        }
    }

    #[test]
    fn matching_body_and_jwt_main_ok() {
        let trust = bearer("main");
        assert_eq!(effective_tenant(&trust, "main"), Ok("main"));
    }

    #[test]
    fn divergent_body_denied() {
        let trust = bearer("main");
        assert_eq!(effective_tenant(&trust, "evil"), Err(StatusCode::FORBIDDEN));
    }

    #[test]
    fn non_main_jwt_with_matching_body_returns_jwt_tenant() {
        // Cas théorique (les Lots 1+2 empêchent un tel JWT d'arriver). Le helper
        // retourne le tenant JWT tel quel : c'est le middleware qui garantit "main".
        let trust = bearer("staging");
        assert_eq!(effective_tenant(&trust, "staging"), Ok("staging"));
    }

    #[test]
    fn studio_context_denied() {
        let trust = TrustContext::Studio {
            user: "admin".into(),
            scope: StudioScope::Admin,
            step_up_until: None,
        };
        assert_eq!(effective_tenant(&trust, "main"), Err(StatusCode::FORBIDDEN));
    }

    #[test]
    fn unauthenticated_denied() {
        let trust = TrustContext::Unauthenticated;
        assert_eq!(effective_tenant(&trust, "main"), Err(StatusCode::FORBIDDEN));
    }
}
