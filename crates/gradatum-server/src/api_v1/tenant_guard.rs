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

//! ## 401 vs 403 (aligné C2, P2-c — valable à flag OFF comme à ON)
//!
//! - **401 Unauthorized** : requête non authentifiée (token absent/invalide) — décidé en
//!   AMONT par `trust.is_authenticated()` dans chaque handler, jamais ici.
//! - **403 Forbidden** : requête authentifiée mais non autorisée — tout refus de ce module
//!   (contexte sans tenant, body divergent, ACL cible deny, grant absent/insuffisant).
//!   À `multi_tenant.enabled = true`, les refus de **grant** portent un message distinct
//!   des refus legacy (dette C1 soldée) ; le statut reste 403.

use axum::http::StatusCode;
use gradatum_acl_auth::has_write_scope;
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::error::GradatumError;
use gradatum_core::scope::{AclCheckedVaultId, AgentId, TenantId, VaultId};
use gradatum_core::trust::TrustContext;

use crate::state::AppState;

/// Refus du garde tenant/vault — distingue les refus legacy des refus de grant (P2 C2).
///
/// `Legacy` couvre les refus possibles à flag OFF (contexte sans tenant, body divergent) :
/// chaque call-site conserve son message historique → byte-identical à OFF.
/// Les variantes grant n'existent qu'à `multi_tenant.enabled = true`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TenantGuardRefusal {
    /// Refus du chemin legacy — le call-site fournit son message historique.
    Legacy,
    /// À ON : aucun grant `write` du tenant sur le vault cible (ou lookup en échec).
    MissingWriteGrant {
        /// Tenant demandeur (JWT).
        tenant: String,
        /// Vault cible de l'écriture.
        vault: String,
    },
    /// À ON : le bearer ne porte aucun scope autorisant l'écriture (EX-C3a-1) —
    /// clé/token lecture-seule stricte.
    MissingWriteScope,
    /// À ON (B8, agent level) : aucun grant `read` de l'agent sur le vault cible.
    MissingReadGrant {
        /// Agent demandeur.
        tenant: String,
        /// Vault cible.
        vault: String,
        /// Section demandée, ou `None` pour vault-entier.
        section: Option<String>,
    },
}

impl TenantGuardRefusal {
    /// Statut HTTP du refus — toujours `403` (le 401 est décidé en amont, cf. doc module).
    pub(crate) fn status(&self) -> StatusCode {
        StatusCode::FORBIDDEN
    }

    /// Erreur sémantique : les refus de grant portent un message distinct (P2-c) ;
    /// `Legacy` reprend le message historique du call-site (byte-identical à OFF).
    pub(crate) fn into_forbidden(self, legacy_msg: &str) -> GradatumError {
        match self {
            Self::Legacy => GradatumError::Forbidden(legacy_msg.to_owned()),
            Self::MissingWriteGrant { tenant, vault } => GradatumError::Forbidden(format!(
                "no write grant for tenant '{tenant}' on vault '{vault}'"
            )),
            Self::MissingWriteScope => {
                GradatumError::Forbidden("write scope required (read-only token)".to_owned())
            }
            Self::MissingReadGrant {
                tenant,
                vault,
                section,
            } => {
                let scope = section
                    .as_deref()
                    .map_or_else(|| "vault-wide".to_string(), |s| format!("section '{s}'"));
                GradatumError::Forbidden(format!(
                    "no read grant for agent '{tenant}' on vault '{vault}' ({scope})"
                ))
            }
        }
    }
}

/// Vrai si `trust` est autorisé à emprunter un chemin d'ÉCRITURE (EX-C3a-1).
///
/// - `multi_tenant.enabled = false` (défaut) → toujours vrai (byte-identical,
///   aucun enforcement de scope sur le parc existant).
/// - `enabled = true` → un `BearerToken` doit porter au moins un scope de
///   [`gradatum_acl_auth::WRITE_SCOPES`]. Les contextes non-Bearer sont inchangés
///   (leur accès vault est déjà gouverné par l'ACL et l'absence de tenant).
///
/// La liste des scopes d'écriture est la SSOT [`gradatum_acl_auth::WRITE_SCOPES`],
/// partagée avec `gradatum-admin api-key create` : la CLI refuse d'émettre une clé
/// que ce garde rejetterait ensuite en écriture.
///
/// SSOT consommée par [`effective_write_vault`] (chemins write du vault) et par
/// les handlers d'écriture hors vault-notes (jobs, session/event log, unforgot).
#[must_use]
pub(crate) fn write_scope_allowed(state: &AppState, trust: &TrustContext) -> bool {
    if !state.server_config.multi_tenant.enabled {
        return true;
    }
    match trust {
        TrustContext::BearerToken { scopes, sub, .. } => {
            let allowed = has_write_scope(scopes);
            if !allowed {
                tracing::warn!(
                    sub = %sub,
                    scopes = ?scopes,
                    "tenant_guard: write refused — no write/admin/service scope (403)"
                );
            }
            allowed
        }
        _ => true,
    }
}

/// Returns the effective tenant (derived from the JWT) for a vault request, or `403`.
///
/// `body_tenant` is the `tenant_id` field from the request DTO — verified for consistency
/// but never used as a source of truth.
///
/// # Errors
/// - `StatusCode::FORBIDDEN` if the context carries no tenant (non-`BearerToken`)
///   or if `body_tenant` diverges from the JWT tenant.
#[must_use = "the returned effective tenant must replace req.tenant_id for locus/index"]
pub(crate) fn effective_tenant<'a>(
    trust: &'a TrustContext,
    body_tenant: Option<&TenantId>,
) -> Result<&'a str, StatusCode> {
    let jwt_tenant = match trust.tenant_id() {
        // Frontière : `tenant_id()` typé `Option<&TenantId>` (Task 3) ; `effective_tenant`
        // retourne `&str` (typage `effective_*` réservé Task 11). `.as_str()` byte-identical.
        Some(t) => t.as_str(),
        None => {
            // Mtls/Studio/Unauthenticated : pas de tenant porté → pas d'accès vault.
            tracing::warn!(
                "tenant_guard: context without tenant (non-Bearer) — vault access denied 403"
            );
            return Err(StatusCode::FORBIDDEN);
        }
    };

    // Lot A1 : `body_tenant` est désormais `Option` — l'OMISSION (`None`) est le cas
    // nominal (le client ne désigne pas son tenant, le serveur le dérive du JWT). Un
    // `tenant_id` explicite reste vérifié pour cohérence : s'il DIVERGE du JWT → 403
    // (tentative de cibler un locus arbitraire), s'il COÏNCIDE → accepté (écho inoffensif).
    // Le défaut implicite `"main"` (ancien `default_main`) est SUPPRIMÉ : une clé du
    // tenant X qui omet `tenant_id` résout X, plus jamais `"main"`.
    if let Some(body_tenant) = body_tenant
        && body_tenant.as_str() != jwt_tenant
    {
        tracing::warn!(
            body_tenant = %body_tenant,
            jwt_tenant = %jwt_tenant,
            "tenant_guard: body tenant_id diverges from JWT — 403 (tenant derived from JWT)"
        );
        return Err(StatusCode::FORBIDDEN);
    }

    Ok(jwt_tenant)
}

/// Write-path resolution (C1, F-63 — EX-C1-1/2): effective tenant **and** write grant.
///
/// Generalises [`effective_tenant`] to the grant model:
/// - `multi_tenant.enabled = false` (défaut) → strictly [`effective_tenant`]
///   (byte-identical legacy path — the grant tables are never read);
/// - `enabled = true` → [`effective_tenant`] **puis** exigence d'un grant
///   `(tenant, vault = tenant, Write)` dans l'allow-list `tenant_vault_grants`.
///   En C1 le vault cible d'une écriture EST le vault propre du tenant
///   (INV-P1-3 : aucun 2e vault en écriture avant le fix ACL C2).
///
/// Enforcement PUBLIC de `INV-P1-3` : structurel (aucun paramètre `target` → la cible est
/// toujours le vault propre) + scope + grant. Le pendant INTERNE (listener loopback worker,
/// garde pure `(principal, target)` sans grant/scope/flag) est `resolve_write_namespace`
/// (module `crate::internal::persist`). Invariant nommé partagé (L4), pas de code commun :
/// couches d'auth et sémantiques de refus distinctes.
///
/// # Errors
/// - `StatusCode::FORBIDDEN` : contexte sans tenant, body divergent, scope write
///   absent (EX-C3a-1, token lecture-seule), grant absent, grant read-only, ou
///   erreur de lookup (fail-closed — jamais un grant implicite).
#[must_use = "the returned effective tenant must replace req.tenant_id for locus/index"]
pub(crate) async fn effective_write_vault(
    state: &AppState,
    trust: &TrustContext,
    body_tenant: Option<&TenantId>,
) -> Result<String, TenantGuardRefusal> {
    let tenant = effective_tenant(trust, body_tenant).map_err(|_| TenantGuardRefusal::Legacy)?;
    if !state.server_config.multi_tenant.enabled {
        return Ok(tenant.to_owned());
    }
    // EX-C3a-1 : enforcement des scopes AVANT le grant — un token lecture-seule
    // est refusé sur tout chemin write, quel que soit son grant.
    if !write_scope_allowed(state, trust) {
        return Err(TenantGuardRefusal::MissingWriteScope);
    }
    require_write_grant(state, tenant, tenant).await?;
    // B9 : agent-level write enforcement (intersection tenant_grant ∩ agent_grant).
    // Transition progressive : 0 grant → pass through. Fail-closed sur lookup error.
    if let Some(agent_id) = trust.subject() {
        require_agent_write_grant(state, agent_id, tenant).await?;
    }
    Ok(tenant.to_owned())
}
///
/// Chemin `multi_tenant.enabled = true` **uniquement** — à OFF, le chemin legacy
/// (ACL appelant + garde mono-vault `vid != "main"`) reste inline dans les handlers,
/// inchangé.
///
/// Séquence (fail-closed à chaque barreau) :
/// 1. `VaultId::parse` sur le `vault_id` de la requête (P2-a — frontière non fiable) ;
///    absent → le vault propre du tenant (déjà validé par le middleware JWT).
/// 2. **EX-C2-1** : ACL `Read` recalculée sur le locus de la **CIBLE**
///    (`locus_section` : `Some(section)` pour search, `Some("timeline")` pour timeline) —
///    plus jamais sur le locus de l'appelant seul (trou historique fermé).
/// 3. **P2-b** : grant `read` OU `write` du tenant sur la cible dans l'allow-list
///    (`write` couvre la lecture, cf. [`gradatum_core::scope::GrantAccess`]).
/// 4. **EX-C2-4** : le tenant CIBLE doit être `active` — un vault suspendu ou
///    soft-deleted cesse d'être lisible IMMÉDIATEMENT, y compris par les tenants
///    détenteurs d'un grant (le refus du middleware ne couvre que les requêtes DU
///    tenant gelé, pas les lectures VERS son vault).
///
/// # Errors
/// - `GradatumError::InvalidInput` : `vault_id` mal formé (400).
/// - `GradatumError::Forbidden` : ACL cible deny (`deny_msg`, parité message OFF),
///   grant absent (message distinct `no read grant …`, P2-c), ou tenant cible non
///   actif (message `vault … is not active`) — 403.
pub(crate) async fn effective_read_vault(
    state: &AppState,
    trust: &TrustContext,
    tenant: &str,
    req_vault_id: Option<&VaultId>,
    locus_section: Option<&str>,
    deny_msg: &str,
) -> Result<AclCheckedVaultId, GradatumError> {
    // P2-a — frontière non fiable : le newtype `VaultId` du DTO est `#[serde(transparent)]`,
    // donc la désérialisation ne valide PAS le format ; on RE-parse ici (byte-identical au
    // chemin `Option<&str>` d'origine : même 400 `invalid vault_id` sur entrée malformée).
    let target: VaultId = match req_vault_id {
        Some(v) => VaultId::parse(v.as_str())
            .map_err(|e| GradatumError::InvalidInput(format!("invalid vault_id: {e}")))?,
        None => VaultId::new(tenant),
    };
    let acl_locus = crate::api_v1::logic::locus_for_section(target.as_str(), locus_section);
    if state.acl.evaluate(trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        // B6′b : les deux barreaux suivants (`require_read_grant`, `require_active_target`)
        // loggent déjà leur refus ; celui-ci était le seul muet de la séquence.
        crate::api_v1::logic::log_acl_deny(trust, AclOp::Read, &acl_locus, "effective_read_vault");
        return Err(GradatumError::Forbidden(deny_msg.to_owned()));
    }
    // Portée VAULT-ENTIER exigée (L3, F-121 — `None`) : ce chemin sert la recherche,
    // la timeline et les handlers non section-scopés, dont la lecture n'est PAS bornée
    // à une section. `locus_section` ci-dessus est un segment de LOCUS ACL (il vaut
    // `"timeline"` pour la timeline — pseudo-section qui n'existe pas dans `notes`) :
    // le confondre avec la portée d'un grant ouvrirait un vault entier sur la foi d'un
    // grant borné. Un grant section-scopé ne franchit donc jamais ce garde.
    require_read_grant(state, tenant, target.as_str(), None).await?;
    // B9 : agent-level read enforcement (intersection tenant_grant ∩ agent_grant).
    // Transition progressive : 0 grant → pass through. Fail-closed sur lookup error.
    // Sur ce chemin vault-entier (recherche, timeline), seul un grant vault-entier
    // (section=None) autorise — meme regle que require_read_grant.
    if let Some(agent_id) = trust.subject() {
        require_agent_read_grant(state, agent_id, target.as_str(), None)
            .await
            .map_err(|r| r.into_forbidden(deny_msg))?;
    }
    require_active_target(state, target.as_str()).await?;
    Ok(AclCheckedVaultId::attest_read_checked(target))
}

/// Exige que le tenant CIBLE d'une lecture soit `active` (C2, EX-C2-4).
///
/// Tenant cible inconnu (jamais provisionné), suspendu, soft-deleted, statut
/// illisible : refus au MÊME message (fail-closed, pas d'oracle sur l'état interne
/// — la distinction est loggée, jamais révélée à l'appelant).
///
/// # Errors
/// - `GradatumError::Forbidden` avec message `vault … is not active`.
async fn require_active_target(state: &AppState, vault: &str) -> Result<(), GradatumError> {
    let refusal = || GradatumError::Forbidden(format!("vault '{vault}' is not active"));
    match state.search.get_tenant_status(vault).await {
        Ok(Some(gradatum_core::scope::TenantStatus::Active)) => Ok(()),
        Ok(status) => {
            tracing::warn!(
                vault = %vault,
                status = ?status,
                "tenant_guard: read to an inactive vault refused (403)"
            );
            Err(refusal())
        }
        Err(e) => {
            tracing::error!(
                vault = %vault,
                err = %e,
                "tenant_guard: get_tenant_status lookup failed — fail-closed (403)"
            );
            Err(refusal())
        }
    }
}

/// Témoin de lecture pour le vault **propre** du tenant (cible == appelant).
///
/// À n'utiliser qu'en aval d'un contrôle ACL Read sur le locus du tenant (tête de
/// handler, cf. doc des modules `context`/`logic`) — JAMAIS pour un `vault_id` issu
/// de la requête (chemin cross-vault → [`effective_read_vault`]).
pub(crate) fn own_vault_checked(tenant: &str) -> AclCheckedVaultId {
    AclCheckedVaultId::attest_read_checked(VaultId::new(tenant))
}

/// Résout le vault de lecture EFFECTIF d'un handler `GET /api/v1/*` non body-scoped,
/// gaté sur `multi_tenant.enabled` (A3-handlers, T9 — read-path).
///
/// - **OFF** (défaut) : chemin legacy inline **byte-identical** — ACL `Read` sur
///   `legacy_vault/section` (l'`acl.evaluate` historique du handler), AUCUN grant consulté ;
///   retourne `legacy_vault` (= `target_vault()` du handler, `"main"`).
/// - **ON** : [`effective_read_vault`] sur le vault PROPRE du principal JWT
///   (`req_vault_id = None`) — ACL cible + grant read + statut actif (fail-closed) ; retourne
///   le vault effectif.
///
/// L'appelant a déjà refusé les non-authentifiés en 401 (`trust.is_authenticated()`) en
/// amont. Le byte-identical OFF vient de l'ABSENCE d'appel au grant (RÈGLE READ-PATH
/// OFF-GATING), pas d'un retour `main` d'`effective_read_vault` — à OFF ce dernier n'est
/// JAMAIS atteint.
///
/// # Errors
/// - `StatusCode::FORBIDDEN` : ACL deny (OFF/ON), grant absent ou cible non active (ON),
///   ou contexte JWT sans tenant (ON).
/// - `StatusCode::BAD_REQUEST` / `NOT_FOUND` / `INTERNAL_SERVER_ERROR` : mappés depuis
///   [`GradatumError`] via `err_to_status` sur le chemin ON.
#[must_use = "the resolved read vault must scope the handler's index reads / enforce ACL"]
pub(crate) async fn resolve_read_vault(
    state: &AppState,
    trust: &TrustContext,
    legacy_vault: VaultId,
    section: &str,
) -> Result<VaultId, StatusCode> {
    if state.server_config.multi_tenant.enabled {
        // ON : le principal JWT gouverne son vault propre (req_vault_id = None).
        let Some(tenant) = trust.tenant_id() else {
            // Contexte sans tenant (Mtls/Studio) : pas d'accès vault par grant.
            return Err(StatusCode::FORBIDDEN);
        };
        let checked = effective_read_vault(
            state,
            trust,
            tenant.as_str(),
            None,
            Some(section),
            "acl deny",
        )
        .await
        .map_err(|e| crate::api_v1::logic::err_to_status(&e))?;
        Ok(VaultId::new(checked.as_str()))
    } else {
        // OFF : ACL Read legacy inline sur `legacy_vault/section`, jamais de grant.
        let acl_locus = format!("{legacy_vault}/{section}");
        if state.acl.evaluate(trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(legacy_vault)
    }
}

/// Requires a `read`-covering grant (`Read` OR `Write`) of `tenant` on `vault` (P2-b),
/// **couvrant la portée `section` demandée** (L3, F-121 — migration 0040).
///
/// Appelé uniquement sur le chemin `multi_tenant.enabled = true`. Grant absent, grant
/// hors portée et erreur de lookup sont des refus (fail-closed) au **même message** —
/// l'état interne (panne DB vs absence de ligne vs mauvaise section) n'est pas révélé
/// à l'appelant, seulement loggé.
///
/// `section` :
/// - `Some(s)` → la lecture porte sur la SEULE section `s` du vault cible ; un grant
///   vault-entier (`section IS NULL`) **ou** un grant borné à `s` l'autorisent ;
/// - `None` → la lecture porte sur le vault ENTIER (recherche, timeline, handlers
///   non section-scopés) ; seul un grant vault-entier l'autorise.
///
/// La règle de couverture est celle de [`gradatum_core::scope::VaultGrant::covers_section`]
/// (SSOT) — jamais ré-implémentée ici.
///
/// # Errors
/// - `GradatumError::Forbidden` avec message distinct `no read grant …` (P2-c).
pub(crate) async fn require_read_grant(
    state: &AppState,
    tenant: &str,
    vault: &str,
    section: Option<&str>,
) -> Result<(), GradatumError> {
    let refusal = || {
        GradatumError::Forbidden(format!(
            "no read grant for tenant '{tenant}' on vault '{vault}'"
        ))
    };
    // Frontière : le garde reçoit `&str` (SSOT partagée avec les handlers), le lookup
    // exige `&TenantId`. `TenantId::new` est non validé et byte-identical — c'est une
    // reconstruction de type, jamais une validation (elle a eu lieu en amont).
    let tenant_id = TenantId::new(tenant);
    match state.search.tenant_grants(&tenant_id).await {
        Ok(grants) => {
            // Read OU Write : `Write` couvre la lecture (GrantAccess, migration 0030).
            // Portée : le grant doit couvrir la section demandée (L3, migration 0040).
            if grants
                .iter()
                .any(|g| g.vault_id.as_str() == vault && g.covers_section(section))
            {
                Ok(())
            } else {
                tracing::warn!(
                    tenant = %tenant,
                    vault = %vault,
                    section = ?section,
                    "tenant_guard: cross-vault read refused — no grant covering the scope in the allow-list (403)"
                );
                Err(refusal())
            }
        }
        Err(e) => {
            tracing::error!(
                tenant = %tenant,
                vault = %vault,
                err = %e,
                "tenant_guard: tenant_grants lookup failed — fail-closed (403)"
            );
            Err(refusal())
        }
    }
}

/// Requires a **vault-wide** `Write` grant of `tenant` on `vault` (EX-C1-2).
///
/// Called only on the `multi_tenant.enabled = true` path. Absence of grant,
/// read-only grant, and lookup error are all refusals (fail-closed).
///
/// L3 (F-121) : un grant **section-scopé** (`section IS NOT NULL`, migration 0040) ne
/// satisfait PAS ce garde — le chemin d'écriture n'est pas section-scopé (il autorise
/// l'écriture sur tout le vault), donc l'ouvrir avec un grant borné serait une
/// élévation de privilège. Exigence exprimée par `covers_section(None)`.
///
/// # Errors
/// - [`TenantGuardRefusal::MissingWriteGrant`] (403, message distinct P2-c) sur tout
///   dénouement non-write-granted, y compris l'erreur de lookup (fail-closed).
pub(crate) async fn require_write_grant(
    state: &AppState,
    tenant: &str,
    vault: &str,
) -> Result<(), TenantGuardRefusal> {
    let refusal = || TenantGuardRefusal::MissingWriteGrant {
        tenant: tenant.to_owned(),
        vault: vault.to_owned(),
    };
    // Frontière : voir `require_read_grant` — reconstruction de type, pas de validation.
    let tenant_id = TenantId::new(tenant);
    match state.search.tenant_grants(&tenant_id).await {
        Ok(grants) => {
            let allowed = grants.iter().any(|g| {
                g.vault_id.as_str() == vault && g.access.allows_write() && g.covers_section(None)
            });
            if allowed {
                Ok(())
            } else {
                tracing::warn!(
                    tenant = %tenant,
                    vault = %vault,
                    "tenant_guard: write refused — no write grant in the allow-list (403)"
                );
                Err(refusal())
            }
        }
        Err(e) => {
            tracing::error!(
                tenant = %tenant,
                vault = %vault,
                err = %e,
                "tenant_guard: tenant_grants lookup failed — fail-closed (403)"
            );
            Err(refusal())
        }
    }
}

// ── Agent-level section enforcement (B8→B9, plan v1.0.0) ────────────────────
// B9 câble ces gardes dans les chemins write/read effectifs. Transition
// progressive (B7, middleware): si l'agent n'a AUCUN grant (empty set),
// pass through — même sémantique que agent_grants_authorize. Dès qu'au
// moins un grant existe, le check devient contraignant.

/// Vérifie que l'agent détient un grant **write** (vault-entier) sur `vault`.
///
/// Miroir de [`require_write_grant`] au niveau agent — l'intersection
/// `tenant_grant ∩ agent_grant` est l'accès effectif.
///
/// # Errors
/// - [`TenantGuardRefusal::MissingWriteGrant`] (403) si l'agent a des grants
///   configures mais aucun ne couvre le vault cible en ecriture.
///
/// Transition progressive : 0 grant → pass through (B7, middleware).
pub(crate) async fn require_agent_write_grant(
    state: &AppState,
    agent_id: &AgentId,
    vault: &str,
) -> Result<(), TenantGuardRefusal> {
    let refusal = || TenantGuardRefusal::MissingWriteGrant {
        tenant: agent_id.as_str().to_owned(),
        vault: vault.to_owned(),
    };
    match state.search.agent_grants(agent_id).await {
        Ok(grants) => {
            if grants.is_empty() {
                // Transition progressive (B7) : 0 grant → pass through.
                return Ok(());
            }
            let allowed = grants.iter().any(|g| {
                g.vault_id.as_str() == vault && g.access.allows_write() && g.covers_section(None)
            });
            if allowed {
                Ok(())
            } else {
                tracing::warn!(
                    agent = %agent_id,
                    vault = %vault,
                    "tenant_guard: agent write refused — no write grant in the allow-list (403)"
                );
                Err(refusal())
            }
        }
        Err(e) => {
            tracing::error!(
                agent = %agent_id,
                vault = %vault,
                err = %e,
                "tenant_guard: agent_grants lookup failed — fail-closed (403)"
            );
            Err(refusal())
        }
    }
}

/// Vérifie que l'agent détient un grant **read** (vault-entier ou section-scopé)
/// sur `vault`.
///
/// Miroir de [`require_read_grant`] au niveau agent. `covers_section(section)`
/// est la source unique de vérité — jamais ré-implémentée aux sites d'appel.
///
/// # Errors
/// - [`TenantGuardRefusal::MissingReadGrant`] (403) si l'agent a des grants
///   configures mais aucun ne couvre la section demandee.
///
/// Transition progressive : 0 grant → pass through (B7, middleware).
pub(crate) async fn require_agent_read_grant(
    state: &AppState,
    agent_id: &AgentId,
    vault: &str,
    section: Option<&str>,
) -> Result<(), TenantGuardRefusal> {
    let refusal = || TenantGuardRefusal::MissingReadGrant {
        tenant: agent_id.as_str().to_owned(),
        vault: vault.to_owned(),
        section: section.map(|s| s.to_owned()),
    };
    match state.search.agent_grants(agent_id).await {
        Ok(grants) => {
            if grants.is_empty() {
                // Transition progressive (B7) : 0 grant → pass through.
                return Ok(());
            }
            let allowed = grants
                .iter()
                .any(|g| g.vault_id.as_str() == vault && g.covers_section(section));
            if allowed {
                Ok(())
            } else {
                tracing::warn!(
                    agent = %agent_id,
                    vault = %vault,
                    section = ?section,
                    "tenant_guard: agent read refused — no matching grant in the allow-list (403)"
                );
                Err(refusal())
            }
        }
        Err(e) => {
            tracing::error!(
                agent = %agent_id,
                vault = %vault,
                err = %e,
                "tenant_guard: agent_grants lookup failed — fail-closed (403)"
            );
            Err(refusal())
        }
    }
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
            jti: None,
        }
    }

    #[test]
    fn matching_body_and_jwt_main_ok() {
        let trust = bearer("main");
        assert_eq!(
            effective_tenant(&trust, Some(&TenantId::new("main"))),
            Ok("main")
        );
    }

    /// PREUVE ROUGE→VERT (Lot A1) — discriminante par construction.
    ///
    /// Une clé appartenant au tenant `tenant-x` qui **omet** `tenant_id` (`None`) doit
    /// résoudre `tenant-x`, et JAMAIS `"main"`.
    ///
    /// Sur le code AVANT A1 (`body_tenant: &TenantId`, défaut `default_main()` = `"main"`),
    /// l'omission produisait `&TenantId::new("main")` ; `effective_tenant(bearer("tenant-x"),
    /// &"main")` renvoyait alors `Err(FORBIDDEN)` (divergence `"main"` ≠ `"tenant-x"`).
    /// Le test échouait donc (attendu `Ok("tenant-x")`, obtenu `Err`). Après A1 l'omission
    /// est `None` → aucune vérification de divergence → le tenant JWT gouverne.
    #[test]
    fn key_x_omitting_tenant_resolves_x_not_main() {
        let trust = bearer("tenant-x");
        assert_eq!(effective_tenant(&trust, None), Ok("tenant-x"));
    }

    /// Corollaire : `tenant_id` explicite COÏNCIDANT avec le JWT reste accepté (écho).
    #[test]
    fn explicit_matching_tenant_still_accepted() {
        let trust = bearer("tenant-x");
        assert_eq!(
            effective_tenant(&trust, Some(&TenantId::new("tenant-x"))),
            Ok("tenant-x")
        );
    }

    /// P2-c : les refus de grant portent un message distinct du message legacy.
    #[test]
    fn refusal_messages_are_distinct() {
        let legacy = TenantGuardRefusal::Legacy.into_forbidden("tenant cross mismatch");
        assert_eq!(
            legacy.to_string(),
            GradatumError::Forbidden("tenant cross mismatch".into()).to_string(),
            "Legacy reprend le message historique du call-site"
        );

        let grant = TenantGuardRefusal::MissingWriteGrant {
            tenant: "main".into(),
            vault: "research".into(),
        }
        .into_forbidden("tenant cross mismatch");
        let msg = grant.to_string();
        assert!(
            msg.contains("no write grant for tenant 'main' on vault 'research'"),
            "message grant distinct attendu, obtenu : {msg}"
        );
    }

    /// P2-c : le statut d'un refus tenant_guard est toujours 403 (401 = amont).
    #[test]
    fn refusal_status_is_always_forbidden() {
        assert_eq!(TenantGuardRefusal::Legacy.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            TenantGuardRefusal::MissingWriteGrant {
                tenant: "t".into(),
                vault: "v".into(),
            }
            .status(),
            StatusCode::FORBIDDEN
        );
    }

    /// Le témoin du vault propre porte le tenant tel quel (helper own_vault_checked).
    #[test]
    fn own_vault_checked_carries_tenant() {
        assert_eq!(own_vault_checked("main").as_str(), "main");
    }

    #[test]
    fn divergent_body_denied() {
        let trust = bearer("main");
        assert_eq!(
            effective_tenant(&trust, Some(&TenantId::new("evil"))),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn non_main_jwt_with_matching_body_returns_jwt_tenant() {
        // Cas théorique (les Lots 1+2 empêchent un tel JWT d'arriver). Le helper
        // retourne le tenant JWT tel quel : c'est le middleware qui garantit "main".
        let trust = bearer("staging");
        assert_eq!(
            effective_tenant(&trust, Some(&TenantId::new("staging"))),
            Ok("staging")
        );
    }

    #[test]
    fn studio_context_denied() {
        let trust = TrustContext::Studio {
            user: "admin".into(),
            scope: StudioScope::Admin,
            step_up_until: None,
        };
        assert_eq!(
            effective_tenant(&trust, Some(&TenantId::new("main"))),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn unauthenticated_denied() {
        let trust = TrustContext::Unauthenticated;
        assert_eq!(
            effective_tenant(&trust, Some(&TenantId::new("main"))),
            Err(StatusCode::FORBIDDEN)
        );
    }
}
