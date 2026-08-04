//! Synchronous wikilink resolution — a deterministic inline variant for contexts with no
//! async runtime (admin backfill, unit tests).
//!
//! Reuses the semantics of [`crate::wikilinks::extract_wikilinks`],
//! [`crate::wikilinks::parse_ulid_target`] and
//! [`gradatum_core::project_map::reserved_node_target`], with no I/O and no HTTP trait.
//!
//! ## Difference with `resolve_wikilinks_via_client`
//!
//! The worker's async version issues HTTP requests to `/internal/v1/id-lookup` and
//! `/internal/v1/title-lookup`. This version takes two `FnMut` closures and resolves
//! directly — useful inside `spawn_blocking` (admin path), where spinning up a Tokio
//! runtime would be an anti-pattern.

/// Resolves the `[[...]]` wikilinks of `body` **synchronously**.
///
/// Applies exactly the same semantics as `resolve_wikilinks_via_client`:
/// 1. reserved nodes (`project:` / `status:` / `kind:` / `version:`) become a direct
///    synthetic edge via [`gradatum_core::project_map::reserved_node_target`];
/// 2. ULID targets (`section:ULID` or a bare ULID) call `id_lookup_fn(tenant, ulid_str)`;
/// 3. anything else (a free-form title) calls `title_lookup_fn(tenant, target)`.
///
/// # Returns
///
/// `(src_note_id, dst)` pairs. Unresolved links — where the lookup returns `None` — are
/// silently skipped, the same non-fatal behaviour as the async version.
///
/// # Failure handling
///
/// Closure failures are surfaced as `None`, logged at `warn` level and skipped; this
/// function never returns an error, matching the non-fatal async version.
#[must_use]
pub fn resolve_wikilinks_sync(
    tenant: &str,
    src_note_id: &str,
    body: &str,
    mut id_lookup_fn: impl FnMut(&str, &str) -> Option<String>,
    mut title_lookup_fn: impl FnMut(&str, &str) -> Option<String>,
) -> Vec<(String, String)> {
    let targets = crate::wikilinks::extract_wikilinks(body);
    let mut edges = Vec::with_capacity(targets.len());

    for target in &targets {
        // Étape 1 : nœud réservé (project/status/kind/version) → arête synthétique
        if let Some(dst) = gradatum_core::project_map::reserved_node_target(target) {
            edges.push((src_note_id.to_string(), dst));
            continue;
        }

        // Étape 2 : cible ULID (section:ULID ou ULID nu) → lookup par identifiant
        if let Some(ulid) = crate::wikilinks::parse_ulid_target(target) {
            let ulid_str = ulid.to_string();
            if let Some(dst) = id_lookup_fn(tenant, &ulid_str) {
                edges.push((src_note_id.to_string(), dst));
            } else {
                tracing::warn!(
                    src_note_id,
                    target,
                    ulid = %ulid_str,
                    "resolve_wikilinks_sync: id_lookup returns None — link ignored"
                );
            }
            continue;
        }

        // Étape 3 : titre libre → lookup par titre
        if let Some(dst) = title_lookup_fn(tenant, target) {
            edges.push((src_note_id.to_string(), dst));
        } else {
            tracing::warn!(
                src_note_id,
                target,
                "resolve_wikilinks_sync: title_lookup returns None — link ignored"
            );
        }
    }

    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nœuds réservés `[[project:X]]` / `[[status:Y]]` → arêtes synthétiques
    /// directes, sans appel aux closures de lookup.
    #[test]
    fn reserved_nodes_produce_synthetic_edges_without_lookup() {
        let mut id_called = false;
        let mut title_called = false;
        let result = resolve_wikilinks_sync(
            "main",
            "src-id",
            "Voir [[project:gradatum]] et [[status:DONE]]",
            |_, _| {
                id_called = true;
                None
            },
            |_, _| {
                title_called = true;
                None
            },
        );
        assert_eq!(result.len(), 2, "doit produire 2 arêtes synthétiques");
        assert!(
            !id_called,
            "id_lookup ne doit PAS être appelé pour les nœuds réservés"
        );
        assert!(
            !title_called,
            "title_lookup ne doit PAS être appelé pour les nœuds réservés"
        );
        // Les deux dst sont les nœuds réservés synthétiques
        let dsts: Vec<&str> = result.iter().map(|(_, d)| d.as_str()).collect();
        assert!(
            dsts.iter().any(|d| d.starts_with("project:")),
            "doit contenir un nœud project: — dsts={dsts:?}"
        );
        assert!(
            dsts.iter().any(|d| d.starts_with("status:")),
            "doit contenir un nœud status: — dsts={dsts:?}"
        );
    }

    /// Cible ULID `[[decisions:01KVBTMYNK4XXZJAKWMTB4AM9K]]` → id_lookup appelé,
    /// title_lookup NON appelé.
    #[test]
    fn ulid_target_calls_id_lookup_not_title_lookup() {
        let mut title_called = false;
        let result = resolve_wikilinks_sync(
            "main",
            "src-id",
            "Voir [[decisions:01KVBTMYNK4XXZJAKWMTB4AM9K]]",
            |_, ulid| Some(ulid.to_string()),
            |_, _| {
                title_called = true;
                None
            },
        );
        assert_eq!(result.len(), 1, "doit produire 1 arête");
        assert!(
            !title_called,
            "title_lookup ne doit PAS être appelé pour un ULID"
        );
    }

    /// Titre humain libre `[[Mon Titre]]` → title_lookup appelé, id_lookup NON appelé.
    #[test]
    fn human_title_calls_title_lookup() {
        let mut id_called = false;
        let result = resolve_wikilinks_sync(
            "main",
            "src-id",
            "Voir [[Mon Titre]]",
            |_, _| {
                id_called = true;
                None
            },
            |_, title| Some(format!("resolved-{title}")),
        );
        assert_eq!(result.len(), 1, "doit produire 1 arête via title_lookup");
        assert!(
            !id_called,
            "id_lookup ne doit PAS être appelé pour un titre humain"
        );
    }

    /// Lookup retournant `None` → lien ignoré silencieusement (comportement non-fatal).
    #[test]
    fn none_from_lookup_is_silently_ignored() {
        let result = resolve_wikilinks_sync(
            "main",
            "src-id",
            "Voir [[decisions:01KVBTMYNK4XXZJAKWMTB4AM9K]]",
            |_, _| None,
            |_, _| None,
        );
        assert!(result.is_empty(), "un lookup None doit produire zéro arête");
    }

    /// Deux appels identiques → résultat identique (déterminisme).
    #[test]
    fn idempotent_same_result_on_two_calls() {
        let make_result = || {
            resolve_wikilinks_sync(
                "main",
                "src-id",
                "Voir [[project:gradatum]] et [[decisions:01KVBTMYNK4XXZJAKWMTB4AM9K]]",
                |_, ulid| Some(ulid.to_string()),
                |_, _| None,
            )
        };
        assert_eq!(
            make_result(),
            make_result(),
            "résultat doit être déterministe"
        );
    }
}
