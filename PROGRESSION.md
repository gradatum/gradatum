# PROGRESSION — suppression moteur legacy (Dispatcher) + bincode

Journal append-only. Ne jamais réécrire.

## Inventaire & arbitrages (investigation)

- Moteur legacy = `crates/gradatum-worker/src/dispatch.rs` (842 lignes, `Dispatcher`), NON exécuté (en-tête confirme, binaire actif = Apalis Monitor).
- Moteur actif = `apalis_handlers.rs` (`handle_curate`/`handle_embed`/...) lisant `SqliteQueueStore` (JSON, table `gradatum_jobs`).
- PRÉMISSE VÉRIFIÉE : le chemin vivant `vault_write` enfile via `state.job_store` (SqliteQueueStore, JSON) — PAS via `state.queue` (SqliteQueue bincode). Le seul autre encodeur bincode côté serveur est le handler `vault_downgrade` `#[allow(dead_code)]` NON routé (write.rs:458-535).
- `gradatum-queue` (SqliteQueue/Queue) N'APPELLE JAMAIS bincode (aucun `bincode::` dans son src). Le payload y est opaque. => SqliteQueue reste câblée en prod (status endpoint jobs.rs, health.rs, backfill_embeddings) et SURVIT. Seuls les encodeurs/décodeurs bincode partent.
- `gradatum_core::Job` : `#[serde(tag="type",content="data")]` sérialisé en serde_json (jamais bincode réellement). => invariant "ordre positionnel bincode figé" SANS OBJET ; invariant survivant = STABILITÉ DES NOMS de variants (clé `type`). Comments à corriger, pas supprimer.

### Sites bincode:: réels (à éliminer)
- dispatch.rs:286/467/590 (décodage) → supprimé avec le module
- write.rs:494 (handler mort vault_downgrade) → supprimé avec le handler
- tests: helpers/mod.rs, dispatch_runtime.rs, wire_byte_identical.rs, write_synthetic.rs:451, chained_jobs.rs:276
- Manifestes bincode: root Cargo.toml:86, gradatum-queue, gradatum-server, gradatum-dto, v1-parity-tests, gradatum-worker (6 au total)

### Arbitrage tests (transposer / supprimer)
- wikilinks_handle_curate.rs : DÉJÀ actif (handle_curate + TestInternalClient). Refs Dispatcher = commentaire en-tête seulement → nettoyer commentaire.
- helpers/mod.rs : réécrire chemin actif (garder MockInternalClient, remplacer Dispatcher/SqliteQueue/bincode par handle_curate + SqliteQueueStore + process_curate).
- wikilinks_post_curate.rs : transposer via helper actif (Admitted count==1, Pending, non-résolu).
- wikilinks_ulid_resolution.rs : transposer (couverture UNIQUE : résolution ULID-first via id_lookup).
- wikilinks_parallel.rs : transposer (couverture UNIQUE : N=5 parallèle).
- helpers_compile.rs : mettre à jour smoke test.
- dispatch_runtime.rs : SUPPRIMER. curate couvert par handle_curate tests ; reclassify couvert par curate_temporal_anchor (make_reclassify_job) ; classify/downgrade-as-queue-job et run_once-empty = comportements moteur legacy uniquement (classify/downgrade sont synchrones dans le chemin actif).
- embed_pipeline.rs : transposer 2 tests vers handle_embed (success, dim-mismatch) ; SUPPRIMER "noop_skip_without_embedder" (embedder optionnel = concept Dispatcher legacy ; handle_embed exige toujours un embedder).
- curate_temporal_anchor.rs / temporal_anchor_e2e.rs : nettoyer commentaires en-tête (déjà actifs).
- e2e_write.rs : retirer le step "Dispatcher legacy run_once" (assert queue jobs_v2 vide) devenu sans objet ; garder l'assert vault_write → gradatum_jobs.
- chained_jobs.rs : traiter l'encode bincode (contexte à lire).
- write_synthetic.rs : test_22 (Dispatcher run_once round-trip) + bincode — à traiter (contexte à lire).
- wire_byte_identical.rs : retirer les assertions bincode, garder JSON.

### Doc-comments à corriger
- gradatum-core/src/job.rs (invariant bincode positional → serde_json name-stability)
- gradatum-dto/src/{lib.rs, vault_write.rs, vault_downgrade.rs, vault_classify.rs}
- gradatum-queue/src/queue.rs:68 (comment "bincode-encoded by the caller")
- gradatum-worker/src/wikilinks.rs:5 (intra-doc link vers dispatch::Dispatcher — cassera rustdoc)
- gradatum-worker/src/lib.rs (header dispatch)

NB : refresh.rs:54 "Dispatcher proactive-refresh" = concept DIFFÉRENT, ne pas toucher.

## Exécution
- dispatch.rs SUPPRIMÉ + dispatch_runtime.rs SUPPRIMÉ (rm).
- lib.rs (worker) : `pub mod dispatch` retiré + en-tête corrigé.
- main.rs (worker) : `mod dispatch` retiré + commentaire corrigé.
- bincode retiré des 6 manifestes : root Cargo.toml + gradatum-queue + gradatum-server + gradatum-dto + v1-parity-tests + gradatum-worker.

- Serveur : handler mort `write::vault_downgrade` (async, non routé, bincode) SUPPRIMÉ ; imports `NewJob` + `VaultDowngradeRequest` retirés de write.rs ; en-tête + logic.rs commentaire corrigés.
- Doc-comments corrigés : wikilinks.rs (lien intra-doc dispatch::Dispatcher retiré) ; queue.rs NewJob (payload serde_json, seul site backfill_embeddings) ; job.rs (invariant "ordre bincode positionnel" → "noms de variants stables, discriminant JSON type" + notes unit→tuple reformulées serde + CurateSpec.expected_sha256) ; dto lib.rs/vault_write/vault_downgrade/vault_classify (distinction JSON/bincode retirée) ; vault_write.rs note_id : INVARIANT ORDRE DE CHAMPS retiré (SANS OBJET sous serde_json — wire indexé par nom).

## Tests worker
- helpers/mod.rs RÉÉCRIT chemin actif : MockInternalClient conservé ; Dispatcher/SqliteQueue/bincode retirés ; `test_curate_fixture` (SqliteQueueStore) + `process_curate` (handle_curate direct) ; `CurateFixture` remplace `DispatcherFixture`.
- wikilinks_post_curate.rs TRANSPOSÉ (Admitted count==1, non-résolu non-fatal, Pending) → process_curate.
- wikilinks_ulid_resolution.rs TRANSPOSÉ (couverture UNIQUE ULID-first via id_lookup : résolu / ghost / fallback titre) → process_curate.
- wikilinks_parallel.rs TRANSPOSÉ (couverture UNIQUE N=5 parallèle + mixte + vide) → process_curate.
- helpers_compile.rs MIS À JOUR (smoke process_curate).
- wikilinks_handle_curate.rs : DÉJÀ actif — commentaire en-tête "Dispatcher legacy" nettoyé, aucun code changé.
- curate_temporal_anchor.rs + temporal_anchor_e2e.rs : commentaires en-tête nettoyés (déjà actifs, SqliteQueue conservée pour wiring AppState).
- embed_pipeline.rs TRANSPOSÉ vers handle_embed : cas succès (Ok + embedding persisté) + dim-mismatch (handle_embed renvoie Err — le monitor marque failed, vs ancien run_once Ok(true)). Cas "noop-skip sans embedder" SUPPRIMÉ (mode embedder-optionnel = Dispatcher legacy ; handle_embed exige toujours un Embedder). EmbedTestClient.get_note implémenté (handle_embed lit le body via le client).

## Tests serveur / parité / dto
- e2e_write.rs RÉÉCRIT : le test prouve désormais vault_write → 202 + job dans gradatum_jobs (job_store actif) + queue legacy jobs_v2 vide. Bloc Dispatcher legacy + NeverCalledClient + audit worker retirés ; NoopAuditSink local ajouté (l'ancien venait de dispatch.rs).
- chained_jobs.rs SUPPRIMÉ : testait le chaînage curate→embed_note via Dispatcher legacy (payload legacy {note_id, body_text} dans SqliteQueue). Subsumé — et plus complètement — par curate_embed_chaining.rs (actif : Job::Embed enqueué, note_id==note créée, tenant, force_regenerate, lineage). Le champ body_text legacy n'existe plus (handle_embed lit le body via get_note).
- write_synthetic.rs : test_22 (round-trip Dispatcher::run_once legacy, bincode) SUPPRIMÉ — couvert par gradatum-worker/tests/curate_* (handle_curate). Tests 11-21 (HTTP→job_store, concurrent) conservés. test_22b (leader election, #[ignore], corps vide) conservé. En-tête + commentaire test_14 corrigés.
- wire_byte_identical.rs : helper bincode + test newtypes_bincode_transparent SUPPRIMÉS (transparence JSON couverte par tenant_id/vault_id_json_transparent + test miroir struct) ; assertion bincode du test miroir retirée (test renommé *_json_matches_string_mirror) ; invariant d'ordre de champs CONSERVÉ mais re-motivé (serde_json émet les clés dans l'ordre de déclaration).
- SNAPSHOT mcp_native__mcp_tools_input_schema_golden.snap : 3 descriptions MCP (vault_write/downgrade/classify, dérivées des doc-comments DTO via schemars) mises à jour pour matcher le nouveau texte. ⚠️ À re-générer/valider via `cargo insta test` côté orchestrateur.

## FIN — récapitulatif
- Aucun code/dep bincode ne subsiste (seul un commentaire explicatif dans vault_write.rs).
- Aucun code Dispatcher/dispatch:: ne subsiste (seuls des commentaires descriptifs + le stub #[ignore] test_22b).
- Non compilé (interdit) : à valider par l'orchestrateur.
