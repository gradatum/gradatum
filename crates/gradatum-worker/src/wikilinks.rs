//! Resolution of `[[...]]` wikilinks via [`InternalClient`].
//!
//! This module exposes [`resolve_wikilinks_via_client`], used by:
//! - [`crate::apalis_handlers::handle_curate`] (both `Admitted` and `Pending` branches)
//! - [`crate::dispatch::Dispatcher`] (integration-test compatibility)
//!
//! ## ULID-first resolution strategy
//!
//! Wikilinks written by the vault have the form `[[section:ULID]]` (e.g.
//! `[[decisions:01KVBTMYNK4XXZJAKWMTB4AM9K]]`). `extract_wikilinks` returns
//! the raw target `"decisions:01KVBTMYNK..."`.
//!
//! For each target, the resolver first attempts to parse a ULID (via
//! `gradatum_curator::wikilinks::parse_ulid_target`). If it is a valid ULID,
//! `client.id_lookup` is called (existence check + live status) — direct resolution
//! without fetching the H1 heading. If it is not a ULID, the resolver falls back to
//! `client.title_lookup` (backwards compatibility).
//!
//! ## Resolved links
//!
//! Resolved links are packed into `PersistCuratedRequest.links` so that
//! the server handles `upsert_link` atomically inside `persist_curated`.
//!
//! ## Non-fatal semantics
//!
//! Any failure (`id_lookup`/`title_lookup` unavailable, missing target note, task panic)
//! is logged without propagation. The returned `Vec<LinkDto>` may contain
//! fewer links than the extracted wikilinks.

use std::sync::Arc;

use tokio::sync::Semaphore;

use gradatum_dto::LinkDto;

use crate::internal_client::InternalClient;

/// Maximum number of simultaneous `title_lookup` resolutions in flight.
///
/// Deterministic cap to prevent unbounded fan-out on notes with many wikilinks,
/// protecting the `/internal` server from overload.
const WIKILINK_RESOLVE_MAX_IN_FLIGHT: usize = 8;

/// Extracts `[[...]]` wikilinks from `body`, resolves them via `client`,
/// and returns a `Vec<LinkDto>` for inclusion in `PersistCuratedRequest.links`.
///
/// The server handles `upsert_link` atomically inside `persist_curated`.
///
/// # Non-fatal
///
/// Any extraction or lookup failure is logged without propagation.
/// The returned `Vec` may contain fewer links than the wikilinks extracted.
///
/// # Missing target note
///
/// `title_lookup` returns `Ok(None)` — logged at `debug` level, link skipped.
///
/// # Concurrency
///
/// Resolutions are launched in parallel via `tokio::task::JoinSet`.
/// Concurrency is capped via a `Semaphore` (currently 8 simultaneous requests)
/// to protect the `/internal` server against large fan-outs.
///
/// Result semantics and collection order are identical to the unbounded version
/// (JoinSet is unordered in both cases).
pub async fn resolve_wikilinks_via_client(
    client: &Arc<dyn InternalClient>,
    tenant_id: &str,
    src_note_id: &str,
    body: &str,
) -> Vec<LinkDto> {
    let wikilinks = gradatum_curator::wikilinks::extract_wikilinks(body);
    if wikilinks.is_empty() {
        return vec![];
    }

    let mut resolved_links = Vec::with_capacity(wikilinks.len());

    // Nœuds réservés project-map (project:/status:/kind:/version:) — arête
    // synthétique vers un hub déterministe, sans lookup réseau (le nœud n'est
    // pas une note). dst est un TEXT libre (note_links migration 0002, pas de FK
    // sur dst_note_id), navigable par vault_graph/vault_trace.
    // spec:/plan:/context: et section:ULID pointent vers de vraies notes → flux
    // ULID/title normal ci-dessous (reserved_node_target → None).
    let to_resolve: Vec<&String> = wikilinks
        .iter()
        .filter(|target| {
            match gradatum_core::project_map::reserved_node_target(target) {
                Some(dst) => {
                    tracing::debug!(
                        src = %src_note_id,
                        dst = %dst,
                        "project-map wikilink typé — arête réservée synthétique"
                    );
                    resolved_links.push(LinkDto {
                        src: src_note_id.to_string(),
                        dst,
                    });
                    false // déjà résolu, ne pas lookup
                }
                None => true, // flux normal (ULID/title)
            }
        })
        .collect();

    // Si tous les wikilinks étaient des nœuds réservés, rien à résoudre côté réseau.
    if to_resolve.is_empty() {
        return resolved_links;
    }

    // Borne la concurrence : au plus WIKILINK_RESOLVE_MAX_IN_FLIGHT requêtes en vol.
    let sem = Arc::new(Semaphore::new(WIKILINK_RESOLVE_MAX_IN_FLIGHT));

    // Résolution parallèle bornée via JoinSet.
    let mut join_set = tokio::task::JoinSet::new();

    for target in to_resolve {
        let client_arc = Arc::clone(client);
        let sem_arc = Arc::clone(&sem);
        let tenant = tenant_id.to_string();
        let target_owned = target.clone();
        // Détecte si la cible contient un ULID (résolution ULID-first).
        let maybe_ulid = gradatum_curator::wikilinks::parse_ulid_target(target);
        join_set.spawn(async move {
            // Acquérir un permit avant d'envoyer la requête.
            // SAFETY-invariant : le Semaphore n'est jamais fermé ici
            // (Arc vivant jusqu'à la fin de la fonction).
            let _permit = sem_arc
                .acquire()
                .await
                .expect("semaphore wikilinks résolution non fermé");
            let result = if let Some(ulid) = maybe_ulid {
                // Résolution ULID-first : [[section:ULID]] → id_lookup (existence + live).
                client_arc.id_lookup(&tenant, &ulid.to_string()).await
            } else {
                // Fallback H1 : [[Titre humain]] → title_lookup (rétrocompat).
                client_arc.title_lookup(&tenant, &target_owned).await
            };
            (target_owned, result)
            // _permit droppé ici → libère le slot pour la prochaine tâche.
        });
    }

    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok((target, Ok(Some(dst_id)))) => {
                tracing::debug!(
                    src = %src_note_id,
                    dst = %dst_id,
                    target = %target,
                    "B5 wikilink résolu — inclus dans persist_curated.links"
                );
                resolved_links.push(LinkDto {
                    src: src_note_id.to_string(),
                    dst: dst_id,
                });
            }
            Ok((target, Ok(None))) => {
                tracing::debug!(
                    target = %target,
                    "B5 wikilink non résolu — note cible absente ou non-live"
                );
            }
            Ok((target, Err(e))) => {
                tracing::warn!(
                    err = %e,
                    target = %target,
                    "B5 lookup failed — wikilink ignoré (non-fatal)"
                );
            }
            Err(e) => {
                tracing::warn!(err = %e, "B5 title_lookup task panicked — wikilink ignoré");
            }
        }
    }

    resolved_links
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gradatum_dto::{
        EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
        PersistForgetRequest, PersistOkResponse,
    };

    use crate::internal_client::{EmbeddingReadDto, InternalClientError, NoteIdDto, NoteReadDto};

    // ── Mock InternalClient — seul title_lookup est implémenté ───────────────
    // Les autres méthodes utilisent unreachable! car ce test n'appelle que title_lookup.

    struct MockClient {
        /// Maximum number of simultaneous in-flight requests observed.
        peak_in_flight: Arc<AtomicUsize>,
        /// Current number of in-flight requests.
        current_in_flight: Arc<AtomicUsize>,
        /// Number of `id_lookup` calls (verifies reserved nodes are not looked up).
        id_lookup_calls: Arc<AtomicUsize>,
        /// Number of `title_lookup` calls (verifies no reserved-node network lookups).
        title_lookup_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl InternalClient for MockClient {
        async fn persist_curated(
            &self,
            _req: &PersistCuratedRequest,
        ) -> Result<PersistOkResponse, InternalClientError> {
            unreachable!("MockClient: persist_curated non utilisé dans ce test")
        }

        async fn persist_embedding(
            &self,
            _req: &PersistEmbeddingRequest,
        ) -> Result<EmbeddingOkResponse, InternalClientError> {
            unreachable!("MockClient: persist_embedding non utilisé dans ce test")
        }

        async fn persist_forget(
            &self,
            _req: &PersistForgetRequest,
        ) -> Result<PersistOkResponse, InternalClientError> {
            unreachable!("MockClient: persist_forget non utilisé dans ce test")
        }

        async fn persist_distill(
            &self,
            _req: &PersistDistillRequest,
        ) -> Result<PersistOkResponse, InternalClientError> {
            unreachable!("MockClient: persist_distill non utilisé dans ce test")
        }

        async fn delete_note(&self, _ulid: &str) -> Result<(), InternalClientError> {
            unreachable!("MockClient: delete_note non utilisé dans ce test")
        }

        async fn get_note(&self, _ulid: &str) -> Result<NoteReadDto, InternalClientError> {
            unreachable!("MockClient: get_note non utilisé dans ce test")
        }

        async fn get_note_embedding(
            &self,
            _ulid: &str,
            _embedder_id: &str,
        ) -> Result<EmbeddingReadDto, InternalClientError> {
            unreachable!("MockClient: get_note_embedding non utilisé dans ce test")
        }

        async fn get_trust(&self, _ulid: &str) -> Result<f32, InternalClientError> {
            unreachable!("MockClient: get_trust non utilisé dans ce test")
        }

        async fn title_lookup(
            &self,
            _tenant: &str,
            title: &str,
        ) -> Result<Option<String>, InternalClientError> {
            self.title_lookup_calls.fetch_add(1, Ordering::SeqCst);
            let current = self.current_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            // Enregistre le pic de concurrence.
            let mut peak = self.peak_in_flight.load(Ordering::SeqCst);
            while current > peak {
                match self.peak_in_flight.compare_exchange(
                    peak,
                    current,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(actual) => peak = actual,
                }
            }
            // Simule un peu de travail pour permettre l'accumulation en vol.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            self.current_in_flight.fetch_sub(1, Ordering::SeqCst);
            // Retourne un id fictif dérivé du titre pour permettre la vérification.
            Ok(Some(format!("id-{title}")))
        }

        async fn id_lookup(
            &self,
            _tenant: &str,
            note_id: &str,
        ) -> Result<Option<String>, InternalClientError> {
            // Résolution ULID directe (existence simulée) — utilisée par le test
            // project-map pour vérifier que les dépendances section:ULID restent
            // résolues normalement à côté des nœuds réservés synthétiques.
            self.id_lookup_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(note_id.to_string()))
        }

        async fn list_notes_by_locus(
            &self,
            _vault: &str,
            _prefix: &str,
        ) -> Result<Vec<NoteIdDto>, InternalClientError> {
            unreachable!("MockClient: list_notes_by_locus non utilisé dans ce test")
        }

        async fn list_by_status(
            &self,
            _vault: &str,
            _status: &str,
        ) -> Result<Vec<NoteIdDto>, InternalClientError> {
            unreachable!("MockClient: list_by_status non utilisé dans ce test")
        }

        async fn list_garbage(
            &self,
            _vault: &str,
            _before_ms: i64,
            _grace_days: u32,
        ) -> Result<Vec<NoteIdDto>, InternalClientError> {
            unreachable!("MockClient: list_garbage non utilisé dans ce test")
        }

        async fn search_fts_for_forget(
            &self,
            _vault: &str,
            _query: &str,
            _limit: usize,
        ) -> Result<Vec<NoteIdDto>, InternalClientError> {
            unreachable!("MockClient: search_fts_for_forget non utilisé dans ce test")
        }

        async fn list_notes_by_agent(
            &self,
            _agent: &str,
            _vaults: &[String],
        ) -> Result<Vec<NoteIdDto>, InternalClientError> {
            unreachable!("MockClient: list_notes_by_agent non utilisé dans ce test")
        }
    }

    /// Verifies that in-flight concurrency never exceeds `WIKILINK_RESOLVE_MAX_IN_FLIGHT`,
    /// even with far more wikilinks than the cap.
    #[tokio::test]
    async fn many_links_resolve_correctly_within_concurrency_cap() {
        let n_links = 30_usize; // > WIKILINK_RESOLVE_MAX_IN_FLIGHT (8)
        let peak_in_flight = Arc::new(AtomicUsize::new(0));
        let current_in_flight = Arc::new(AtomicUsize::new(0));

        let client: Arc<dyn InternalClient> = Arc::new(MockClient {
            peak_in_flight: Arc::clone(&peak_in_flight),
            current_in_flight: Arc::clone(&current_in_flight),
            id_lookup_calls: Arc::new(AtomicUsize::new(0)),
            title_lookup_calls: Arc::new(AtomicUsize::new(0)),
        });

        // Construit un body avec n_links wikilinks distincts.
        let body: String = (0..n_links)
            .map(|i| format!("[[note-{i}]]"))
            .collect::<Vec<_>>()
            .join(" ");

        let links = resolve_wikilinks_via_client(&client, "main", "src-note-id", &body).await;

        // Tous les liens doivent être résolus (résolution totale).
        assert_eq!(
            links.len(),
            n_links,
            "tous les {n_links} wikilinks doivent être résolus",
        );

        // Le pic de concurrence ne doit pas dépasser la limite.
        let peak = peak_in_flight.load(Ordering::SeqCst);
        assert!(
            peak <= WIKILINK_RESOLVE_MAX_IN_FLIGHT,
            "pic de concurrence {peak} > cap {WIKILINK_RESOLVE_MAX_IN_FLIGHT}"
        );
    }

    /// A typed project-map link produces a synthetic reserved edge WITHOUT a network
    /// lookup, while a `section:ULID` dependency is still resolved via `id_lookup`
    /// (non-regressed).
    #[tokio::test]
    async fn reserved_nodes_resolve_without_lookup_deps_unregressed() {
        let id_lookup_calls = Arc::new(AtomicUsize::new(0));
        let title_lookup_calls = Arc::new(AtomicUsize::new(0));

        let client: Arc<dyn InternalClient> = Arc::new(MockClient {
            peak_in_flight: Arc::new(AtomicUsize::new(0)),
            current_in_flight: Arc::new(AtomicUsize::new(0)),
            id_lookup_calls: Arc::clone(&id_lookup_calls),
            title_lookup_calls: Arc::clone(&title_lookup_calls),
        });

        // 3 nœuds réservés (project/status/version) + 1 dépendance section:ULID.
        let dep_ulid = "01KVBTMYNK4XXZJAKWMTB4AM9K";
        let body = format!(
            "[[project:gradatum]] [[status:DONE]] [[version:gradatum/0.6.1]] [[decisions:{dep_ulid}]]"
        );

        let links = resolve_wikilinks_via_client(&client, "main", "src-note", &body).await;

        // 4 arêtes au total (3 réservées + 1 dépendance).
        assert_eq!(links.len(), 4, "3 nœuds réservés + 1 dépendance ULID");

        let dsts: Vec<&str> = links.iter().map(|l| l.dst.as_str()).collect();
        assert!(dsts.contains(&"project:gradatum"), "arête project réservée");
        assert!(dsts.contains(&"status:DONE"), "arête status réservée");
        assert!(
            dsts.contains(&"version:gradatum/0.6.1"),
            "arête version réservée"
        );
        assert!(dsts.contains(&dep_ulid), "dépendance ULID résolue");

        // Les nœuds réservés ne déclenchent AUCUN lookup réseau : seule la
        // dépendance ULID a appelé id_lookup une fois. title_lookup jamais.
        assert_eq!(
            id_lookup_calls.load(Ordering::SeqCst),
            1,
            "seule la dépendance ULID appelle id_lookup"
        );
        assert_eq!(
            title_lookup_calls.load(Ordering::SeqCst),
            0,
            "aucun nœud réservé ni dépendance ULID ne passe par title_lookup"
        );
    }

    /// An invalid status casing is NOT a reserved node: it falls back to the title
    /// lookup path (`title_lookup`), not a silent synthetic edge.
    #[tokio::test]
    async fn invalid_status_casing_falls_back_to_title_lookup() {
        let id_lookup_calls = Arc::new(AtomicUsize::new(0));
        let title_lookup_calls = Arc::new(AtomicUsize::new(0));

        let client: Arc<dyn InternalClient> = Arc::new(MockClient {
            peak_in_flight: Arc::new(AtomicUsize::new(0)),
            current_in_flight: Arc::new(AtomicUsize::new(0)),
            id_lookup_calls: Arc::clone(&id_lookup_calls),
            title_lookup_calls: Arc::clone(&title_lookup_calls),
        });

        // "status:done" (minuscule) n'est pas un nœud réservé → flux titre.
        let links = resolve_wikilinks_via_client(&client, "main", "src", "[[status:done]]").await;

        assert_eq!(title_lookup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(id_lookup_calls.load(Ordering::SeqCst), 0);
        // Le mock title_lookup renvoie un id fictif → 1 arête (pas un nœud réservé).
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].dst, "id-status:done");
    }
}
