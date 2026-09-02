# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **A note on internal milestones.** Several entries below document internal milestones that
> were never independently published — no crates.io release, no GitHub release, no public tag.
> Public releases are the ones that carry a tag; an internal milestone's title line is marked
> `internal milestone, published in X.Y.Z`, naming the first tagged version above it in this
> file, the public release that carried its changes forward. That mapping follows the document's
> order, not a change-by-change diff — see the `[2.0.0]` note below for a case worked out in full.

## [2.1.1] — 2026-09-01

### Fixed — `upsert_note` laissait des postings FTS périmés, la recherche levait `SQLITE_CORRUPT` (F-267)

- **`notes_fts` (FTS5 external-content, `content=notes`, sans trigger) laissait des
  postings périmés après une mise à jour de note.** `SqliteIndex::upsert_note` réécrivait
  `notes.body_text` **puis** exécutait `INSERT OR REPLACE INTO notes_fts` : le retrait
  implicite de postings relisait le **nouveau** contenu et laissait orphelins ceux de
  l'**ancien**, jusqu'à ce que `snippet()` lise une position inexistante dans le corps
  courant — `SQLITE_CORRUPT` (« database disk image is malformed »), rendu au client en
  `internal error`. Le même défaut existait sur `write_note_derived_batch` (branche
  `ON CONFLICT DO UPDATE`).
- **Correction, sur les deux chemins** : figer les anciennes valeurs (`rowid`, `body_text`,
  `tags`) avant toute mutation, émettre
  `INSERT INTO notes_fts(notes_fts, rowid, body_text, tags) VALUES('delete', …)`, puis
  insérer les nouveaux postings — le patron documenté de synchronisation FTS5
  external-content (celui d'un trigger `AFTER UPDATE`), désormais indépendant de l'ordre
  des statements vis-à-vis de la réécriture de `notes`. Prouvé par
  `upsert_in_place_leaves_no_stale_fts_postings` : le token de l'ancienne version ne rend
  plus jamais de posting ni de `snippet()` corrompu.
- **Impact mesuré sur l'instance de référence (2026-09-01)** : 3,6 % des documents
  portaient au moins un posting périmé, dont 66 % concentrés dans une même section —
  une seule ligne corrompue dans le top-N suffisait à faire échouer l'appel entier.
- ⚠️ **2.1.1 empêche la corruption future, il ne répare pas l'existante.** Un index créé ou
  mis à jour avant 2.1.1 peut porter des postings périmés ; le symptôme est un **échec de
  recherche**, jamais une perte de données — le corps des notes en base n'est pas altéré.
  Réparation (services arrêtés, backup fait au préalable) :
  ```sql
  INSERT INTO notes_fts(notes_fts) VALUES('rebuild');
  ```
- ⚠️ **Quatre contrôles usuels rendent VERT sur un index pourtant corrompu** : la
  comparaison `count(notes)` / `count(notes_fts_docsize)`, `PRAGMA integrity_check`,
  `PRAGMA quick_check`, et `INSERT INTO notes_fts(notes_fts) VALUES('integrity-check')`
  **sans** `rank`. Seule la forme rangée détecte cette classe de corruption :
  ```sql
  INSERT INTO notes_fts(notes_fts, rank) VALUES('integrity-check', 1);
  ```
  Un utilisateur qui se fie à l'un des quatre premiers contrôles conclut à tort que son
  index est sain.
- **Garde interne ajoutée, aucune API publique nouvelle** : `SqliteIndex::fts_integrity_check`
  (`pub(crate)`) exécute la forme rangée ci-dessus et rend un verdict de contenu —
  `Ok(true)`/`Ok(false)`, distinct d'une panne de la vérification elle-même (`Err`). Prouvé
  par `fts_integrity_check_detects_stale_postings` : VERT sur index sain, ROUGE dès qu'un
  posting périmé subsiste, y compris sur la reproduction en SQL brut de l'ancien défaut.
  2.1.1 reste un correctif — aucun symbole public ajouté.
- **`SqliteIndex::upsert_note` est désormais transactionnel.** Ses quatre statements
  (snapshot des anciens postings, `INSERT`/`UPDATE` `notes`, `'delete'` FTS, `INSERT` FTS)
  tournaient auparavant en autocommit, alors que `write_note_derived_batch` était déjà
  enveloppé dans `BEGIN IMMEDIATE` + COMMIT/ROLLBACK — l'asymétrie était l'oubli. Si un
  statement FTS échouait **après** que `notes.body_text` portait déjà la nouvelle version,
  un retry relisait cette nouvelle version et émettait un retrait FTS contre des postings
  qui n'existent plus — le même sous-flux FTS5, « results undefined », que ce correctif
  traite par ailleurs. Reprend le patron du chemin batch (`BEGIN IMMEDIATE` +
  COMMIT/ROLLBACK), sans changer signatures, sémantique d'erreur ni ordre des statements.
  Prouvé par `upsert_note_rolls_back_fts_failure_and_retry_stays_consistent` : échoue
  transaction neutralisée, passe transaction en place.

## [2.1.0] — 2026-08-29

> Cette section liste les changements de comportement mesurés sur ce jalon. À l'exception des cartes
> F-248 et F-177 (ruptures de surface publique assumées, voir `### Removed` ci-dessous) et de
> F-145 (sous-lots 1 et 2 : champs de variantes `#[non_exhaustive]` qui changent de type ;
> sous-lot 3 : la signature des constructeurs et helpers de `gradatum-db-sqlite` change de type
> de connexion ; lot final : montée de rusqlite `0.32.1` → `0.40.2` — les sous-lots 1 à 3 visibles
> à `cargo public-api`, invisibles à `cargo-semver-checks`, le lot final ne modifiant AUCUNE
> signature mais changeant le moteur SQLite embarqué, voir `### Changed` ci-dessous), aucun ne
> **modifie** une signature d'API Rust publique — le critère
> 10 **ajoute** deux symboles `gradatum-search`, tous deux additifs, visibles à
> `cargo public-api` ; les changements de comportement du classement restent, eux, invisibles à
> `cargo-semver-checks`. Ils sont déclarés ici pour cette raison précise.
>
> Suivi : cartes F-162, F-248, F-177, F-145.

### Guide de migration public — 2.0 → 2.1 (F-249)

Une **mineure est adoptée sans aucune action du consommateur** : si votre build casse après
l'adoption de 2.1.0, lisez d'abord le guide de migration — il inventorie chaque rupture, son
message d'erreur probable, ce qu'il faut écrire à la place, et un script de migration pour les
substitutions mécaniques : [`docs/UPGRADING-2.0.0-to-2.1.0.md`](docs/UPGRADING-2.0.0-to-2.1.0.md)
(script : `scripts/migrate-2.0-to-2.1.sh`).

### Changed — la base de révocation abandonne sqlx pour rusqlite (F-145, sous-lot 1 sur 3)

- **`gradatum-auth` ne dépend plus de sqlx** : `SqliteRevocationStore` passe de
  `sqlx::SqlitePool` à une connexion `rusqlite::Connection` unique sous verrou
  `tokio::sync::Mutex`, exécutée sur fil bloquant (`spawn_blocking`) — le même motif de pont
  synchrone/asynchrone que les magasins du serveur (`proactive_recall_store`,
  `note_usage_store`, `read_usage_store`). La dépendance `sqlx` est retirée du `Cargo.toml`
  de `gradatum-auth` ; elle reste dans le graphe pour les bases qui l'utilisent encore (file,
  clés d'API, index).
- **Voie de remplacement** : `RevocationError::Sqlite` porte désormais une `rusqlite::Error`
  au lieu d'une `sqlx_core::error::Error`, et une variante `RevocationError::Blocking` couvre
  la panne du fil bloquant. L'énumération étant `#[non_exhaustive]`, le changement de type du
  champ est invisible à `cargo-semver-checks` (mesure F-145 : aucune rupture rendue) mais
  apparaît dans la baseline `public-api` de `gradatum-auth`.
- Le schéma reste créé idempotemment (`CREATE TABLE IF NOT EXISTS`), WAL conservé,
  `busy_timeout` 5 s, `synchronous` au défaut SQLite (FULL) — cette base ne porte aucune
  table de suivi de migration sqlx, rien à honorer côté rejeu.

### Changed — la base des clés d'API abandonne sqlx pour rusqlite (F-145, sous-lot 2 sur 3)

- **`gradatum-acl-auth` ne dépend plus de sqlx** : `SqliteApiKeyStore` passe de
  `sqlx::SqlitePool` à une connexion `rusqlite::Connection` unique sous verrou
  `tokio::sync::Mutex`, exécutée sur fil bloquant (`spawn_blocking`) — le même motif de pont
  synchrone/asynchrone que le sous-lot 1 et les magasins du serveur (`proactive_recall_store`,
  `note_usage_store`, `read_usage_store`). La dépendance `sqlx` est retirée du `Cargo.toml`
  de `gradatum-acl-auth` ; elle reste dans le graphe pour les bases qui l'utilisent encore
  (file, index).
- **Voie de remplacement** : `ApiKeyError::Sql` porte désormais une `rusqlite::Error` au lieu
  d'une `sqlx_core::error::Error`, et deux variantes sont ajoutées — `ApiKeyError::Blocking`
  (panne du fil bloquant) et `ApiKeyError::Migration` (base de migration sale ou checksum
  modifié). L'énumération étant `#[non_exhaustive]`, le changement de type du champ est
  invisible à `cargo-semver-checks` (même mesure qu'au sous-lot 1 : aucune rupture rendue)
  mais apparaît dans la baseline `public-api` de `gradatum-acl-auth`.
- **Migrations honorées (piège P0)** : cette base est la seule des trois bases sqlx à porter
  une table de suivi (`_sqlx_migrations`, `sqlx::migrate!`). Le remplaçant la lit : une
  migration déjà appliquée (colonne `version`) n'est JAMAIS rejouée, son checksum SHA-384 est
  vérifié (une migration appliquée est immuable), et une base sale (`success = false`) refuse
  le démarrage. Prouvé sur base jetable reproduisant l'état de production — tests
  `init_does_not_replay_migrations_on_production_like_base` et `migration_runner_*`.
- WAL conservé, `busy_timeout` 5 s, `synchronous` au défaut SQLite (FULL) — identiques aux
  réglages sqlx d'origine.

### Changed — la file abandonne sqlx pour rusqlite (F-145, sous-lot 3 sur 3)

- **`gradatum-db-sqlite` et `gradatum-queue` ne dépendent plus de sqlx** : `SqliteQueueStore`
  passe de `sqlx::SqlitePool` à un handle `QueueDb` (connexion `rusqlite::Connection` unique
  sous `Arc<tokio::sync::Mutex>`, ouverte et opérée sur fil bloquant via `spawn_blocking`,
  verrou `blocking_lock()` tenu au minimum — le même motif de pont que les sous-lots 1 et 2 et
  les magasins du serveur). `sqlx` est retiré du **graphe de dépendances entier** : `cargo tree
  -i sqlx` ne rend plus rien (27 crates, zéro occurrence).
- **Voie de remplacement** : les signatures publiques de la file changent de type de connexion —
  `SqliteQueueStore::new(db: QueueDb)`, `apply_sqlite_pragmas(&QueueDb)`,
  `run_migrations(&QueueDb) -> Result<usize, QueueError>`, helpers `idempotency_*` sur
  `&QueueDb`. Les constructeurs `open_queue_db` (crée si absente, WAL + `busy_timeout` 5 s),
  `open_queue_db_existing` (fail-fast si absente, parité `create_if_missing(false)`) et
  `open_queue_db_in_memory` (tests) remplacent l'ouverture sqlx. Ces changements de signature
  sont **visibles à `cargo public-api`** (baseline `gradatum-db-sqlite` : 65 → 85 items) mais
  **invisibles à `cargo-semver-checks`** (faux vert, même mesure qu'aux sous-lots 1 et 2 :
  aucune rupture rendue) — **AUCUNE dérogation** n'est inscrite à `RELEASE-MANIFEST.yaml`, le
  présent journal est le seul porteur.
- **Migrations honorées (piège P0)** : la file porte 7 migrations (006 → 012), dont les
  **non-idempotentes** 007 et 011 (`ALTER TABLE … ADD COLUMN`). Le remplaçant lit la table de
  suivi `_sqlx_migrations` : une version présente n'est JAMAIS rejouée, le checksum SHA-384 de
  chaque migration appliquée est vérifié (une migration appliquée est immuable), une base sale
  (`success = false`) refuse le démarrage. Prouvé sur base jetable reproduisant l'état de
  production (7 lignes de suivi, `gradatum_jobs` à l'état post-012) :
  `init_does_not_replay_migrations_on_production_like_base` rend 0 application, table intacte.
- **Format sérialisé des travaux intouché (propriété F-248)** : le payload stocké dans
  `gradatum_jobs.payload` reste `serde_json::to_string(&JobRecord)`, écrit VERBATIM dans la
  colonne par `enqueue`. Prouvé par
  `serialized_job_payload_format_is_pinned_and_stored_verbatim` (JSON figé + lecture brute de
  la colonne). La base de production n'est pas touchée.
- Les consommateurs (`gradatum-worker`, `gradatum-server`, `gradatum-admin`, tests de parité
  `v1-parity-tests`) sont portés sur `QueueDb` ; l'élection leader passe sur la même connexion
  partagée. WAL conservé, `busy_timeout` 5 s, `synchronous` au défaut SQLite (FULL) —
  identiques aux réglages sqlx d'origine. `rusqlite` restait épinglé à 0.32.1 à ce stade — la
  montée (lot final, moteur SQLite 3.46.0 → 3.53.2) est documentée dans la section suivante.

### Changed — montée de rusqlite `0.32.1` → `0.40.2` (F-145, lot final)

- **`rusqlite` monte de `=0.32.1` à `=0.40.2`** (workspace `Cargo.toml`), dernier lot de la carte
  F-145 — celui qui porte le bénéfice de la carte. Le blocage historique qui épinglait 0.32.1
  (sqlx 0.8 exigeait `libsqlite3-sys ^0.30.1`, inconciliable avec rusqlite 0.33+) est levé depuis
  que sqlx a été retiré du graphe entier (sous-lots 1-3). **Aucun appel n'a dû être adapté** :
  `cargo check --workspace --all-targets` passe sur 0.40.2.
- **Version du moteur SQLite embarqué : `3.46.0` → `3.53.2`** (`libsqlite3-sys` `0.30.1` →
  `0.38.2`). Mesurée par `SELECT sqlite_version()` via le test `sqlite_engine_version` de
  `gradatum-db-sqlite` — jamais déduite du numéro de crate. Le moteur **monte**, pas de recul ;
  un plancher anti-recul `(3, 46, 0)` est désormais asserté dans ce test. ⚠️ Ce changement de
  moteur touche le format des bases de production (montée de version, pas de régression) —
  déclaré ici conformément à la vigilance F-145 : une substitution de moteur non déclarée est
  exactement le défaut qui a fait écarter la tentative précédente.
- **Mesure de performance AVANT/APRÈS sur la file de travaux** : banc `b11_queue_cycle`
  (`gradatum-bench`), cycle `enqueue → dequeue → complete` ×500 sur base in-memory
  (WAL + migrations 006→012), table vidée hors chronométrage. Verdict : **pas de différence
  mesurable** — protocole entrelacé (4 paires AVANT/APRÈS, binaires exécutés directement, sans
  rebuild), l'écart (~2–5 % poolé) est sous le bruit de mesure (charge externe load 3–7 ; une
  même version oscille ±30 %). La montée **ne dégrade pas** la file → **maintien de la montée**
  (branche « monté » de la carte). Aucune amélioration n'est revendiquée (un écart sous le bruit
  est un « pas de différence », pas une amélioration). Chiffres et protocole dans le commit.
- **Recherche vectorielle intacte** : `sqlite-vec 0.1.9` (static lib `sqlite_vec0`, headers
  embarqués) compile et fonctionne sur le moteur 3.53.2. Validation : `ann_recall` (recall@10 =
  1.000, seuil 0.90) + nouveau test CI `vec_search_returns_exact_match_with_real_sqlite_vec`
  (`gradatum-bench`).
- **Surface publique inchangée** : baseline `public-api` identique (27 crates / 5017 items) —
  **aucune rupture rendue** ⇒ **aucune dérogation** à `RELEASE-MANIFEST.yaml`, le présent journal
  porte seul l'avertissement de changement de moteur (règle des sous-lots précédents).
- **Critère « rapport de rupture NON VIDE »** : honoré par `test-public-surface-break-report.sh`
  (`tests/release-gate/`), écrit contre le dispositif qui voit réellement la rupture — le diff de
  baseline de surface publique (`cargo public-api`). `cargo-semver-checks` rend « no semver update
  required » sur les changements de type portés par une variante `#[non_exhaustive]` (mesure
  établie 3× le 2026-08-25/26, reproduite sur fixture) — rapport VIDE là où la baseline est
  NON VIDE. Le test reproduit la classe de rupture sur un fixture autonome et vérifie que le diff
  de surface est NON VIDE.

### Changed — `HealthSnapshot` et `DriftScanResult` deviennent `#[non_exhaustive]` (F-245)

`gradatum_engine::health::HealthSnapshot` et `gradatum_index::drift::DriftScanResult` portent
désormais l'attribut `#[non_exhaustive]`.

**Ce que ça casse** : un littéral de construction chez un consommateur ne compile plus
(`error[E0639]: cannot create non-exhaustive struct using struct expression`). Ces deux structures
étaient constructibles depuis l'extérieur, et chaque champ ajouté entre `2.0.0` et `2.0.8` était donc
une rupture majeure à déclarer — trois l'ont été. L'annotation les **absorbe toutes**, mesuré :
`cargo-semver-checks` rend `struct_marked_non_exhaustive` et **aucun**
`constructible_struct_adds_field`.

**Ce qu'il faut faire** : passer par le constructeur ou l'API fournie
(`HealthState::snapshot()`, `scan_phase_a()`, `DriftScanResult::default()` avec
`..Default::default()`). Détail et cas non automatisables : `docs/UPGRADING-2.0.0-to-2.1.0.md` §7.

**Pourquoi maintenant et pas plus tard** : ajouter `#[non_exhaustive]` *après* une publication est
soi-même une rupture. La fenêtre se referme au tag de `2.1.0`, qui porte déjà des ruptures assumées
— le coût marginal pour le consommateur est nul. À partir de cette version, toute addition de champ
sur ces deux structures est **additive**.

Arbitrage opérateur du 2026-08-26. Inscrit à `semver_deviations` (`RELEASE-MANIFEST.yaml`),
appariement prouvé dans les deux sens. Inventaire des dérogations : 22 → 21.

Suivi : carte F-245.

### Removed — `compute_distill_trust` retiré de `gradatum_core::provenance` (F-248)

- **`gradatum_core::provenance::compute_distill_trust` est retiré** de la surface publique de
  `gradatum-core`. La fonction est déplacée telle quelle dans la nouvelle crate
  **`gradatum-distill`** (`gradatum_distill::compute_distill_trust`) — même signature, même
  comportement (`mean(trust des sources) × confidence`, clamp `[0,1]`, neutre `0.5` si aucune
  source connue). Rupture de surface publique assumée, déclarée à `RELEASE-MANIFEST.yaml`
  (`semver_deviations`, carte F-248).
- La même carte regroupe dans `gradatum-distill` l'ensemble de la logique de distillation
  jusqu'alors dispersée : le clustering cosinus (`distill_cluster::cosine_similarity` /
  `cluster_by_cosine`, ex-`gradatum-worker`) et l'abstraction de synthèse
  (`DistillSynthesizer` / `TemplateSynthesizer` / `ClusterSynthesis` / `SynthesisError`,
  ex-`gradatum-worker::apalis_handlers`). Ces derniers symboles étaient `#[doc(hidden)]` dans le
  worker : leur retrait de là-bas n'est pas une rupture de surface publique supplémentaire.
- **Migration** : remplacer l'import `gradatum_core::provenance::compute_distill_trust` par
  `gradatum_distill::compute_distill_trust`. Le trait `TrustLookup` reste dans
  `gradatum_core::provenance` ; le vocabulaire de job (`DistillMode`, `DistillSource`,
  `Job::Distill`) reste inchangé dans `gradatum-core::job` (contrats de charge utile, pas de
  traitement).

### Removed — la file legacy `jobs_v2` et tout le chemin de queue sqlx associé (F-177)

La table `jobs_v2` et le module `queue` de `gradatum-queue` qui la lisait sont
**supprimés**. La table conservait le contenu complet de notes supprimées (payload
BLOB) hors de tout cycle de vie du vault : aucune suppression ni purge ne
l'atteignait — de la **rémanence**, pas de l'historique (2 804 lignes figées depuis
le 2026-05-29). Retraits nominatifs :

- **`GET /api/v1/jobs/:id`** (route de rétrocompat, handler `jobs::get_job`, lecture
  de `jobs_v2`) — supprimée. **Voie de remplacement** : `GET /api/v1/jobs/{ulid}/v2`
  (`jobs_v2::get_job_v2`), le `poll_url` retourné par `vault_write`/`vault_forget`
  pointe déjà vers cette route depuis Phase 1.2.
- **Module `gradatum_queue::queue`** (crate publiée) — supprimé : `SqliteQueue`,
  trait `Queue` async, `NewJob`, `LeasedJob`, `JobInfo`, `JobId`, `QueueError`.
  **Voie de remplacement** : `GradatumQueue` (implémentation de
  `gradatum_core::QueueStore`) sur la file LIVE `gradatum_jobs`.
- **Conversion de statut héritée** `gradatum_queue::JobStatus` (`as_str`/`from_str`,
  états `Pending`/`Leased`/`Done`/`Dead`) — supprimée avec le module `queue`.
  **Voie de remplacement** : `gradatum_core::job::JobStatus` (le statut des
  `JobRecord` de `gradatum_jobs`) et le vocabulaire des handlers `jobs_v2`.
- **Table `jobs_v2`** — supprimée par la **migration 012** (`DROP TABLE IF EXISTS
  jobs_v2`, idempotente, sûre sur instance fraîche). La file rusqlite `jobs`
  (`LegacyQueue`) et `worker_leadership` sont conservées.

Déclarées à `RELEASE-MANIFEST.yaml` (`semver_deviations`, carte F-177) : les 15
ruptures mesurées de surface publique `gradatum-queue` sous `cargo-semver-checks`
0.50 en régime mineur vs `internal/2.0.9`.

### Removed — `KindKind::Chore` et `KindKind::Spike` retirés pour de bon (F-220)

Les deux variantes `gradatum_core::project_map::KindKind::Chore` et
`KindKind::Spike` — retirées par le jalon interne `[2.0.6]`, puis **restaurées en
dépréciation** au jalon `[2.0.8]` pour laisser aux consommateurs de la `2.0.0`
publiée le temps de migrer — sont **supprimées pour de bon** en `2.1.0`. C'est la
version effectivement publiée qui expose enfin la rupture annoncée : l'entrée
`Removed` du jalon `[2.0.6]` prévoyait explicitement son report ici.

- **Surface Rust** : les deux variantes disparaissent de l'énumération `KindKind`,
  ainsi que le bras `as_wire` qui les sérialisait. Un consommateur qui nomme
  `KindKind::Chore` ou `::Spike` ne compile plus — c'est l'échéance annoncée par le
  `#[deprecated(since = "2.0.8")]` de la `2.0.8`.
- **Vocabulaire réseau** : inchangé, à quatre valeurs (`FEATURE` / `ENHANCEMENT` /
  `FIX` / `TASK`). `KindKind::from_wire` retourne `None` pour `"CHORE"` / `"SPIKE"`
  depuis la `2.0.6`, comportement inchangé ; écrire `[[kind:CHORE]]` ou
  `[[kind:SPIKE]]` reste **rejeté** par le registre (`SchemaError::InvalidKind`,
  message de migration inchangé).
- **Migration** : utiliser `KindKind::Task`, le fourre-tout délibéré qui absorbe la
  maintenance, l'outillage, l'exploration bornée et le travail non catégorisé.
- **Corollaire mesuré** : retirer les deux variantes **décale le discriminant
  implicite** de `KindKind::Task` (mesure `cargo-semver-checks` 0.50 vs
  `internal/2.0.9` : **5 → 3**). L'énumération n'a pas de `#[repr]` ; seul du code
  aval castant la variante via `as isize` / `as u8` en serait affecté (aucun
  consommateur connu).
- Déclaré à `RELEASE-MANIFEST.yaml` (`semver_deviations`, carte F-220) : les deux
  ruptures `enum_variant_missing` (`Chore`, `Spike`) **et** la rupture
  `enum_no_repr_variant_discriminant_changed` (`KindKind::Task`), mesurées en régime
  mineur vs `internal/2.0.9`.

### Changed — la fusion hybride conserve la magnitude : pondération des scores normalisés, plus RRF pur (F-162, critère 10)

- Quand les **deux bras** (lexical BM25 et sémantique) répondent à `vault_search`, la fusion par
  **rang** (RRF, `1/(k+rank)`) est remplacée par une **fusion pondérée sur scores normalisés** :
  `0.5 × normalize_bm25(bm25) + 0.5 × normalize_semantic(cosine)`. La magnitude des signaux
  cesse d'être jetée dans le cas nominal — elle ne l'était déjà plus à bras unique (critère 6).
- **Échelle des scores** : le composite plafonnait à ≈ 0.04 (RRF `2/(k+1)` × facteurs ≤ 1.32),
  avec un écart 1ᵉʳ↔10ᵉ inexploitable de ≈ 8 % en moyenne (6 requêtes à deux bras du banc, mesuré
  sur le binaire `13765377` pré-critère-10). Après pondération, la fusion ∈ `[0,1]` × composite ≤
  1.32, et l'écart 1ᵉʳ↔10ᵉ mesuré au même banc passe à **45 %** en moyenne (les requêtes dont les
  notes sont réellement ex-æquo — ex. `synthétique`, présent dans les 200 notes — descendent à 0 %
  d'écart : le score dit honnêtement qu'elles sont indistinguables ; les requêtes à notes à zéro
  (aucun bras) montent à 100 %).
- **Classement déplacé** (voulu) : une note qui matche fortement les deux bras dépasse une note
  qui n'en matche qu'un. Mesuré sur le banc : g01/g09 (4 notes remplacées par un meilleur cosine),
  g03 (2 notes), g07 (ex-æquo massif → top-5 par ordre d'insertion BM25), g02/g08 (même ensemble,
  ordre interne changé). `expected_corpus_match_count` est **inchangé** sur les 9 requêtes du
  golden — la fusion ne touche pas au décompte lexical.
- **Nouveaux symboles publics** (`gradatum-search`, additifs) : `rrf::hybrid_fuse_weighted` et
  `scoring::weighted_fusion_score`. Le paramètre `k` de `rrf_fuse_short_circuit` est conservé pour
  compatibilité de signature, **sans effet** dans le cas deux bras (plus de rang).
- **Propagation au contexte** : le chemin d'assemblage de contexte LLM
  (`context/retrieval.rs`) bascule de `rrf_fuse` (RRF pur) à `rrf_fuse_short_circuit` — le
  reweighting s'applique aussi à l'injection de contexte, conformément à la décision opérateur
  (le court-circuit était, lui, resté scopé à `vault_search`). Le chemin Noop (embedder éteint)
  reste sur `rrf_fuse` pur, bit-à-bit inchangé.

### Changed — les requêtes multi-mots gagnent en rappel, jamais en le perdant (F-162)

- Une requête `vault_search` (endpoint HTTP `/api/v1/search` et outil MCP équivalent) composée
  de plusieurs mots séparés par des espaces était auparavant enveloppée **dans son ensemble** en
  une seule phrase FTS5 contiguë dès qu'un seul de ses caractères sortait de l'alphanumérique et
  de l'espace (un tiret, un point, une apostrophe…). Un seul caractère de ce type dans toute la
  requête suffisait à exiger que tous les mots apparaissent **collés, dans cet ordre exact** —
  la requête `cargo-semver-checks baseline` ne matchait par exemple plus rien, alors que les deux
  termes existaient séparément dans le corpus.
- Chaque mot de la requête est désormais cité **indépendamment**, puis les mots sont combinés par
  un ET implicite entre eux. L'ensemble de résultats obtenu est, pour toute requête donnée, un
  **sur-ensemble** de l'ancien — il ne peut que s'élargir, jamais se réduire.
- Ce chemin est partagé par la recherche directe et par l'assemblage automatique de contexte
  (utilisé notamment pour l'injection de contexte fournie à un LLM) : les deux bénéficient du
  même élargissement.
- Rien à faire côté consommateur dans le cas général — c'est une augmentation stricte du rappel.
  Seul un usage qui dépendait implicitement de l'ancien comportement pour forcer une **phrase
  exacte contiguë** (mots strictement adjacents, dans cet ordre) doit désormais citer sa requête
  explicitement entre guillemets pour obtenir ce résultat.

### Changed — `OR`, `NOT`, `NEAR` (et `AND`) sont désormais cherchés comme des mots littéraux (F-162)

- Toujours dans `vault_search`, les mots réservés `AND`, `OR`, `NOT` et `NEAR` — que le moteur de
  recherche texte intégral interprète normalement comme des opérateurs de requête — sont
  désormais systématiquement cités, donc **cherchés comme des mots ordinaires**, au même titre
  que n'importe quel autre terme de la requête.
- Une requête qui s'appuyait sur ces mots comme opérateurs (par exemple pour une union ou une
  recherche de proximité) ne produit plus l'effet recherché : le mot-opérateur devient un terme
  cherché au même titre que les autres, ce qui change les résultats renvoyés.
- Ce choix est délibéré : gradatum n'a jamais documenté ni garanti de langage de requête à
  opérateurs pour ses consommateurs. Si votre usage dépendait de ces mots comme opérateurs,
  aucun substitut n'existe aujourd'hui côté API — pour une union, effectuez deux requêtes
  séparées.

### Changed — le message accompagnant `corpus_match_count=0` distingue désormais deux cas, plus un troisième (F-162)

- L'indice textuel accolé à `corpus_match_count` en mode `compact: true` (sur `vault_search` et
  les surfaces équivalentes) accompagnait tout compte `0` du même message, quelle que soit la
  requête :
  `(corpus_match_count=0 -> 0 lexical match: absence proven, semantic neighbours only)`.
  Une requête ne comportant que de la ponctuation (dont aucun mot ne peut structurellement
  matcher quoi que ce soit dans le corpus) recevait donc le même message qu'une requête d'un
  vrai mot simplement absent du corpus.
- Deux messages désormais, selon la forme de la requête, à la place de l'unique message
  ci-dessus :
  - Requête comportant au moins un caractère alphanumérique, zéro résultat lexical : l'absence
    est prouvée, mais **seulement pour la voie lexicale** — le message ne dit plus « semantic
    neighbours only » et n'invite plus à écarter les résultats sémantiques renvoyés à côté :
    `(corpus_match_count=0 -> 0 lexical match: lexical absence of the term in the filtered
    surface is proven; the semantic relevance of the returned results is NOT disproven)`.
  - Requête faite uniquement de ponctuation ou de mots-opérateurs (aucun caractère
    alphanumérique) : le message indique que le compte est **sans objet** pour cette forme de
    requête, et ne prétend prouver aucune absence :
    `(corpus_match_count=0 -> count not applicable to this query form: every token normalises
    to empty (punctuation/operators only), so no document can match lexically whatever the
    corpus; the returned results are semantic-only and this zero says nothing about them)`.
- La clé JSON `corpus_match_count` (le nombre lui-même) est inchangée. Si votre code parse ce
  texte d'accompagnement pour distinguer les états, adaptez-le aux deux formulations
  ci-dessus — l'ancienne chaîne n'est plus émise.

## [2.0.9] — 2026-08-23 — *internal milestone, published in 2.1.0*

### Changed — le gate semver de release escalade par rang et baseline, au lieu d'un littéral (F-162 lot 0)

- Le job `semver` de `release.yml` et le gate G8 ne comparent plus à une version écrite en dur :
  un **résolveur de rang et de baseline** dérive la baseline du déclencheur (espace `v*` pour le
  rang), et l'escalade est décidée à partir de ce rang. Le littéral est retiré et le code de
  retour du gate est désormais **consommé** au lieu d'être traversé.
- L'inventaire des dérogations passe d'un champ objet à une **liste** de tuples
  `(crate, symbol, lint, rendered, …)`, et l'appariement entre une rupture rendue par
  `cargo-semver-checks` et son entrée d'inventaire se fait par le **triplet**
  `(crate, lint, rendered)` — plus par le seul nom de symbole.
- Une **clause de dérogation datée** encadre la tolérance sur une mineure sous trois conditions.

### Added — éprouvettes des deux régimes, déterminisme et bout-en-bout

- Éprouvettes couvrant les **deux régimes** du résolveur (rang interne / rang publié) et le
  **déterminisme** de la sortie.
- Éprouvette **bout-en-bout** exerçant les vraies sorties de l'outil par le chemin d'extraction
  (P0-1), plutôt que des fixtures recopiées.

### Fixed — CI

- Le job `semver` **désambiguïse la baseline git** : `rc=101` → 26/26.
- Les fixtures du gate de release ne portent plus de nom d'utilisateur (**scrub**).

### Measured — surface publique inchangée vs `internal/2.0.8`

- `cargo public-api` sur les **26 crates** : **26/26 clean** face à `internal/2.0.8` — inventaire
  confirmé, aucune rupture de surface introduite par ce jalon.

## [2.0.8] — 2026-08-21 — *internal milestone, published in 2.1.0*

### Changed — la parité « outil exposé ⇔ usage compté » devient un invariant testé (F-234)

- Le serveur MCP expose une surface d'outils (`tool_catalog()`, ce que renvoie `list_tools`) et
  compte l'usage d'un sous-ensemble (`MCP_TOOL_KEYS`). Rien ne garantissait que les deux
  coïncident : un doc-comment le *disait* sans rien empêcher, et trois capacités exposées —
  `job_status`, `vault_proactive_recall`, `vault_proactive_recall_feedback` — étaient servies
  sans jamais être comptées. Leur usage était donc invisible à toute sonde d'exploitation.
- Un garde de test (`every_declared_tool_is_instrumented`) compare désormais les **deux sources
  de production en ensembles** — jamais un compte en dur. Toute capacité livrée hors compteur
  fait échouer le test *avant* le merge, au lieu d'être découverte plus tard par une ligne
  d'audit muette. Les trois capacités ci-dessus sont maintenant instrumentées et comptées.
- Le cardinal gravé `assert_eq!(MCP_TOOL_KEYS.len(), 23)` est retiré : un compte en dur reste
  vert sur un renommage à effectif constant et casse au premier ajout. Le garde de parité est
  désormais seul propriétaire de l'invariant. Le bornage de cardinalité Prometheus (map
  pré-peuplée, no-op sur nom inconnu) est inchangé.

### Deprecated — `KindKind::Chore` et `KindKind::Spike` restaurées puis dépréciées (F-220, F-225)

- Si votre code nomme `gradatum_core::project_map::KindKind::Chore` ou `::Spike`, il
  **continue de compiler** en `2.0.8` : les deux variantes, retirées par le jalon interne
  `[2.0.6]`, sont restaurées. Elles portent désormais `#[deprecated(since = "2.0.8")]` — votre
  build émet un avertissement nommant `KindKind::Task` comme remplacement. Migrez vers
  `KindKind::Task` : ces variantes seront **retirées en `2.1.0`**.
- Pourquoi cette restauration : `2.0.0` est publiée sur crates.io, un consommateur a pu écrire
  `KindKind::Chore`. Retirer la variante sous un simple incrément de correctif aurait cassé sa
  compilation sans préavis. La dépréciation lui donne le message et l'échéance ; la rupture
  définitive part en `2.1.0`.
- **Le vocabulaire réseau reste à quatre valeurs.** Cette restauration ne touche que l'API de
  type Rust. Écrire `[[kind:CHORE]]` ou `[[kind:SPIKE]]` reste **refusé** par le registre, avec
  le message de migration inchangé. Si vous construisez `KindKind::Chore` et le sérialisez, sa
  valeur wire historique `"CHORE"` **échoue visiblement** à la relecture — elle n'est jamais
  réécrite silencieusement en `"TASK"`.

### Fixed — les drapeaux « standard » du déploiement de release ne pouvaient jamais déployer un bump (F-239)

- `deploy-gradatum-local.sh` est un **consommateur** de `target/release` (« ce script ne build
  pas », hors `--build`). Or une release change toujours la version, et le contrôle de fraîcheur
  (§0d) refuse d'installer un artefact périmé : les drapeaux « standard » ne pouvaient donc
  jamais déployer un bump sans un build manuel préalable — une dépendance implicite, précisément
  le cas d'usage du champ. Ils incluent désormais `--build` (`--build --gateway --engine`).
- `--rebaseline-migrations` quitte les drapeaux standard pour un champ **conditionnel** : un
  drapeau qui touche les migrations ne doit pas passer systématiquement parce qu'il logeait dans
  la ligne « standard ». Sa règle d'emploi est conservée, pas perdue.

## [2.0.7] — 2026-08-21 — *internal milestone, published in 2.1.0*

### Fixed — le verdict du registre affichait une alarme sans jamais la faire échouer (F-207)

- `project-map scope` imprimait « NON RÉCONCILIÉ » en cas d'écart entre le total annoncé et
  la somme des statuts, puis rendait toujours un code de sortie 0 — une ligne d'alarme à
  laquelle aucun contrôle automatisé ne pouvait s'accrocher.
- La commande sort désormais en **code 2** sur un écart non réconcilié, dans les deux
  sens : total supérieur à la somme des statuts (cartes disparues de la ventilation) et
  somme supérieure au total (cartes comptées deux fois) déclenchent tous deux ce code — les
  deux formes traduisent le même défaut de comptage.
- Le code 2 reprend la convention déjà portée par `drift-scan` dans le même binaire pour
  distinguer « travail fait, verdict négatif » (2) de « le binaire n'a pas pu travailler »
  (1). La sortie complète, y compris la ligne d'écart, est écrite avant que le code de
  sortie ne soit rendu.

### Fixed — quatre outils MCP refusaient une forme de référence de note acceptée partout ailleurs (F-215)

- `vault_history`, `vault_history_get`, `vault_restore` et `vault_diff` rejetaient la forme
  préfixée `section/ULID` que `vault_read` et le reste de l'API acceptent, avec une erreur
  interne opaque plutôt qu'un refus explicitement nommé.
- Les quatre outils acceptent désormais exactement les mêmes formes de référence que
  `vault_read`, avec un rejet nommé citant la valeur en cause pour toute forme non résolue.
- `vault_classify`, `vault_downgrade` et `vault_write` restent volontairement sur un
  comportement distinct — chacun pour une raison propre à son usage (cible de mutation
  résolue par titre = ambiguë pour `vault_downgrade` ; identifiant pré-alloué honoré tel
  quel pour `vault_write`) — avec un message d'erreur enrichi mais un comportement
  inchangé. Trois routes REST paramétriques ne sont pas concernées : leur forme d'URL ne
  peut structurellement pas porter la forme préfixée.

## [2.0.6] — 2026-08-20 — *internal milestone, published in 2.1.0*

### Removed — retrait des variantes `Chore` et `Spike` de `KindKind` (F-220)

- **Deux symboles disparaissent de la surface publique de `gradatum-core`** :
  `gradatum_core::project_map::KindKind::Chore` et
  `gradatum_core::project_map::KindKind::Spike`.
- ⚠️ **Rupture source d'API publique dans une version mineure, délibérée.**
  `#[non_exhaustive]` protège l'**ajout** de variantes, jamais leur **retrait** : un
  consommateur qui nomme `KindKind::Chore` ne compile plus. L'écart au SemVer est assumé et
  a été **arbitré par l'opérateur** (2026-08-19, confirmé le 2026-08-20 avec la cible
  ramenée de 2.1.0 à ce jalon). Il est consigné ici parce que le CHANGELOG est le registre
  durable et publiquement lisible de ce qui a été livré — taire une rupture réelle
  contredirait la gouvernance du projet.
- ⏳ **Portée réelle à ce jour : nulle côté consommateur.** Les jalons `2.0.x` ne sont pas
  publiés indépendamment — aucun crate n'en est issu, aucun tag ne les porte. Le retrait
  n'atteindra un consommateur externe qu'à la **prochaine version effectivement publiée** :
  cette entrée `Removed` devra alors être **reportée sous cette version-là**, sans quoi la
  rupture sera livrée sans figurer au changelog de la release qui l'expose.
- **Migration** : utiliser `KindKind::Task`, le fourre-tout délibéré qui couvre déjà la
  maintenance, l'outillage, l'exploration bornée et le travail non catégorisé. Côté wire,
  les valeurs `CHORE` et `SPIKE` sont désormais **rejetées à l'écriture**
  (`SchemaError::InvalidKind`, dont le message nomme le retrait et la valeur de
  remplacement).
- **Registre migré avant le retrait** : les **27 cartes** portant `[[kind:CHORE]]` ou
  `[[kind:SPIKE]]` ont été basculées vers `[[kind:TASK]]` — **0 carte restante** au moment
  du retrait.

## [2.0.5] — 2026-08-18 — *internal milestone, published in 2.1.0*

Douze cartes. Le fil commun de 2.0.4 tenait en une phrase : **un dispositif qui rend un
verdict sur une mesure qu'il n'a pas faite est pire que son absence, parce qu'il supprime la
question.** 2.0.5 en est la suite directe, et elle est plus dure : les sept cartes d'origine
venaient d'un audit d'exploitation, et **trois de leurs quatre prémisses se sont révélées
fausses à la mesure**. Le lot a été construit en corrigeant d'abord les cartes qui le
décrivaient elles-mêmes. Cinq cartes supplémentaires sont nées de l'instruction des sept
premières — aucune par revue de code, toutes par usage réel.

### Instruction du lot — prémisses falsifiées

- **F-204 — deux travaux de curation morts en file sur un refus d'écriture disque.** ⚠️
  **Aucune perte de donnée** : les notes visées existent, dans une version plus avancée que
  celle que le travail transportait. Le travail a vécu 21 secondes, pas les 9 jours annoncés
  par la prémisse initiale — falsifiée à la mesure. La cause historique du refus d'écriture
  reste hors d'atteinte : journaux effacés. Le défaut réel, corrigé ici, est le silence :
  livré via `/health`.
- **F-205 — le journal d'événements n'a jamais été drainé, 1637 lignes.** Deux causes,
  aucune n'était celle annoncée : le consommateur n'a jamais été câblé (`fetch_pending` sans
  appelant), et les moteurs distants basculent en silence sur un puits inerte après un refus
  d'identité depuis le 2026-08-08. Livré : `/health` du moteur expose l'état de télémétrie en
  cinq états, orthogonal au statut de service — un moteur replié sert correctement son trafic
  et ne doit pas être marqué malade. Le repli au démarrage reste une **bifurcation à sens
  unique**, établie par le code.
- **F-206 — moteur d'embedding réputé injoignable 57 jours.** Prémisse falsifiée : les deux
  points d'accès répondent, vérifié par requête réelle. Le lien présumé avec les unités sans
  vecteur est infirmé.

### Registre

- **F-207 — le registre ne comptait que quatre statuts sur six ; 123 cartes n'apparaissaient
  dans aucun.** Livré : tous les statuts comptés, un panier « non classé », et une **ligne de
  réconciliation nommée** — plus jamais deux nombres non comparables sans le dire.
- **F-208 — cinq notes annoncées « absentes de l'index, invisibles à toute recherche ».**
  Prémisse falsifiée : elles sont indexées et cherchables ; il leur manque une empreinte de
  détection de dérive. Le libellé mesurait une chose et se lisait comme une autre. Livré : le
  libellé dit ce qu'il mesure. Réparation des cinq empreintes délibérément non exécutée dans
  ce lot — la cause d'abord.
- **F-214 — la vue des cartes ouvertes en listait 179 quand il y en avait 96.** Cause : une
  note downgradée **conserve ses arêtes** dans le graphe de liens ; 84 cartes mortes se
  faisaient compter comme du travail en attente. Livré : population source restreinte en
  amont, jamais un filtre en sortie. Parité vérifiée en identité, pas en décompte.

### Outils d'administration et API

- **F-209** (investigation) — quatre capacités sans appel en sept jours. Tranchée en les
  appelant : deux sont inutilisées parce qu'**inutilisables**. L'audit proposait trois issues ;
  il en manquait une quatrième.
- **F-215 — `vault_links` et `vault_graph` rendaient zéro arête, sans erreur, sur la forme de
  chemin employée par tout le reste de l'API.** Livré : les deux formes acceptées, à parité
  stricte avec `vault_read`. ⚠️ **Une référence introuvable rend toujours 200 et un graphe
  vide** — c'est le contrat v1, assumé, pas un défaut résiduel. Le silence n'a pas été fermé.
  ⚠️ **Périmètre partiel** : seuls `vault_links` et `vault_graph` acceptent la forme préfixée.
  `vault_history` et `vault_diff` la refusent encore, et leur refus remonte en erreur interne
  opaque plutôt qu'en entrée invalide.
- **F-216 — `vault_tags` rendait 135 Ko sans aucun paramètre pour borner.** Livré : borne par
  défaut, levée sur demande explicite, et un cardinal total qui rend la troncature détectable.
- **F-217 — un travail mort affichait « max_retries atteint » à la place de la cause qui
  l'avait tué.** Huit travaux, deux familles, une seule ligne affichée. Livré : la cause
  survit, **en tête** — l'affichage tronque à 80 caractères, et la conséquence en tête aurait
  écrasé ce qui distingue. ⚠️ Ce correctif a rendu atteignable un **panic de découpe UTF-8**
  jusque-là dormant, corrigé dans le même lot. Les huit travaux restent en échec définitif :
  abandon recommandé, décision opérateur.

### Déploiement et supervision

- **F-210 — un journal sans règle de rotation, une règle sans journal.** Deuxième occurrence
  du même oubli.
- **F-218 — le manifeste de release déclarait 2 binaires quand le déploiement en couvre 5 sur
  8 unités**, et ses drapeaux standards omettaient la passerelle et les moteurs. Sans
  correction, le correctif F-205 aurait été compilé, versionné, annoncé — et **jamais installé
  sur les cinq instances qu'il existe pour rendre visibles**.

### Ce que cette version ne fait pas

- **La cause historique du refus d'écriture de F-204 reste hors d'atteinte** — journaux
  effacés.
- **La non-reprise des échecs permanents est délibérément écartée** : elle changerait la
  sémantique de reprise d'un service en production.
- **La réparation des cinq empreintes de F-208 n'est pas exécutée** — la cause d'abord.
- **Les huit travaux morts de F-217 ne sont pas repris** — abandon recommandé, décision
  opérateur.

## [2.0.4] — 2026-08-18 — *internal milestone, published in 2.1.0*

Lot **anti-faux-vert**. Dix cartes. Le fil commun tient en une phrase : **un dispositif qui rend
un verdict sur une mesure qu'il n'a pas faite est pire que son absence, parce qu'il supprime la
question.** Chacune des dix a trouvé au moins un cas de cette forme — souvent dans le mécanisme
de sécurité censé la prévenir.

### Déploiement

- **F-173 — supprimer l'échec plutôt que le rattraper.** Le binaire engine est un fichier
  *unique partagé* par les moteurs d'un hôte, et n'exposait que son chemin de configuration : un
  refus de démarrage n'était donc détectable qu'**après le point de non-retour**. Aucun
  ordonnancement ne pouvait le supprimer. Deux pièces : un mode de validation sans effet de bord
  sur le binaire (8 causes accumulées, sans lier de port ni lancer le processus supervisé), et
  une **étape de validation en zone de transit placée avant toute mutation** — dépôt sur l'hôte
  cible, exécution *sur place* (ce qui prouve le plancher glibc et les bibliothèques réellement
  présentes, là où une inspection statique ne comparait que des chaînes), validation par
  configuration. Un seul refus et rien n'a bougé : binaires LIVE byte-identiques, aucun service
  arrêté, pas même de répertoire de sauvegarde créé. Filet tout-ou-rien pour le résiduel.
- **F-186 — le repli restaure désormais *en processus*, pas seulement sur disque.** Le repli
  réinstallait les binaires puis appelait `start` — **no-op sur une unité active**. Le processus
  continuait donc sur le binaire refusé pendant que l'outil annonçait « rollback effectué ».
  Silencieux parce qu'installer par-dessus un binaire en exécution **réussit**. La parade descend
  dans la fonction qui porte le contrat, en trois temps indivisibles ; l'enveloppe qui la
  dupliquait est supprimée. Et une garde d'appel légitime fondée sur la **pile d'appels** : un
  marqueur par variable échoue *ouvert* dans son propre cas nominal.

### Registre et surfaces publiques

- **F-180 — registre assaini, 4 critères sur 4.** 224 travaux annoncés ouverts dont 132 sur une
  version dépassée ; 42 titres dupliqués. La cause des doublons n'était pas une garde absente
  mais **une garde présente et inopérante** — elle lisait un champ que l'API ne renvoie pas, donc
  répondait toujours « rien n'existe ». Garde ajoutée sur un second axe, sans implémentation par
  défaut : un défaut rendant « rien n'existe » aurait reproduit la maladie.
- **F-192 — le contrôle de synchronisation du site était vert par hasard d'ordre.** Il indexait
  l'export en gardant la dernière entrée vue, et l'export ne déduplique pas : deux projections de
  la même donnée rendaient l'ordre inverse. Il détecte désormais la multiplicité et **abandonne
  la comparaison** plutôt que de choisir. Contrat de sortie porté à trois états — toute cécité
  emprunte le code « incapable de conclure », y compris l'export vide, qui franchissait la garde
  en étant *truthy*.
- **F-183 — les versions affichées sont vérifiées contre la release publiée**, à deux temps : le
  rendu produit au push, le rendu servi après publication. Marqueur déclaratif plutôt que
  balayage : aucun scanner ne distingue une prétention courante d'un fait historique.
- **F-172 — l'appartenance d'un script au dépôt public devient une propriété de son
  emplacement**, plus une liste tenue à la main. Elle existait en quatre copies, dont une avait
  déjà divergé.

### Configuration et validation

- **F-190 — la surcharge de configuration par variables d'environnement était documentée et sans
  effet.** Retirée plutôt que réparée : le préfixe est déjà pris par un secret, et le réparer
  l'aurait fait transiter par un désérialiseur qui expose la valeur fautive.
- **F-191 — la validation répond désormais pour l'identité qui fera tourner le service**, non
  pour celle qui l'invoque. L'écart est asymétrique : un appelant plus capable que le service
  produit un faux vert qui autorise la bascule que la validation existe pour interdire.
- Angle mort refermé dans la foulée : un fichier de configuration **illisible** était rendu comme
  **absent**, avec un message envoyant chercher au mauvais endroit pendant un pré-vol.

### Supervision

- **F-179 — audit d'utilisation à 24 h après chaque mise en production, puis hebdomadaire.** Le
  rapport est **sans état** : une anomalie de 99 jours y figure au même rang qu'une née ce matin.
  Un rapport en delta masque par construction ce qui ne bouge pas. Le signal de déploiement
  n'existait pas — il est dérivé de l'empreinte temporelle des binaires LIVE, lisible
  rétroactivement même si la sonde était éteinte.
- **F-182 — sonde d'aptitude à publier.** Le contrôle du site pouvait devenir rouge sans que rien
  ne le signale : ce n'était pas une panne mais un **piège armé**, dont le coût ne se payait qu'au
  moment de publier.

### Sécurité

- **RUSTSEC-2026-0258** levé — `h2` 0.4.14 → 0.4.16, sur le chemin de requête du serveur.
  `spin` 0.9.8 (retiré) → 0.9.9. Le verrou `links=sqlite3` reste intact.


## [2.0.3] — 2026-08-17 — *internal milestone, published in 2.1.0*

Lot de correction **déploiement / publication**. Trois cartes livrées. Le fil commun : **des
surfaces publiques que rien ne relisait, et un outil de déploiement qu'on ne pouvait pas
inspecter sans l'exécuter.**

Un quatrième défaut, découvert en instruisant le lot, a bloqué toute compilation pendant la
soirée et méritait d'être réparé avant le reste — il est documenté plus bas.

### Fixed — le jargon interne fuyait dans la documentation générée (F-178)

- Les doc-comments Rust sont rendus par `rustdoc` pour **chaque version publiée**, et cette
  sortie est **figée par version**. Un lecteur externe y trouvait des identifiants de cartes qui
  ne renvoient à rien — ni ticket public, ni page, ni contexte.
- Jargon ramené à **zéro** sur les deux volets du gate anti-jargon, six crates publiées.
- Les motifs sont **reformulés, jamais supprimés** : « the blind spot F-174 closes » devient
  « the drift scan closes » ; « trois cartes de reprise » devient « reindexing orphaned files,
  backfilling vectorless notes, and drift detection ». Plusieurs blocs sont plus clairs qu'avant.
- Les commentaires d'implémentation `//` ne sont pas touchés : ils ne sont pas rendus, et une
  référence de carte y éclaire légitimement un choix.

### Fixed — charger le script de déploiement déclenchait une mise en production (F-185)

- `scripts/deploy-gradatum-local.sh` comptait 679 lignes **sans garde de source**. Le charger —
  pour tester une fonction, réutiliser un helper, l'inspecter — l'exécutait de haut en bas :
  arrêt de services, construction, installation.
- La logique passe sous une fonction principale invoquée par une garde de source. Charger le
  fichier **définit** désormais sans **exécuter**.
- `set -euo pipefail` devient la première ligne de cette fonction : le laisser au niveau du
  fichier polluerait les options du shell appelant à chaque chargement. La contrepartie est
  fermée — les constantes se résolvent en mode permissif, et la fonction principale **valide
  strictement** que le répertoire projet existe et porte bien le `Cargo.toml` du workspace. Un
  répertoire quelconque muni d'un `Cargo.toml` est refusé.
- Vérifié sous huit formes d'invocation, y compris à travers un lien symbolique et depuis `dash`.
  Le plan `--dry-run --build` est **identique octet pour octet** avant et après.

### Fixed — la documentation générée portait des liens morts (F-188)

- `cargo doc` refusait de documenter deux crates : trois liens pointaient un élément privé depuis
  une documentation publique, trois ne résolvaient pas.
- Les six cibles sont des détails d'implémentation — aucune n'a vocation à être cliquable. Le
  lien est retiré, **la mention conservée**, sauf pour l'entrée du binaire dont la mention ne
  rendait service à personne.
- Aucun élément n'a été rendu public pour satisfaire un lien de documentation, aucun lint n'a été
  désactivé.

### Fixed — le ramasse-miettes du pool de build était arrêté depuis neuf jours

- Le pool de build a saturé à **451 Go sur 451**, bloquant toute compilation : ni tests, ni
  gates, ni release. Cause : une relaxation des seuils marquée **temporaire** pour une passe de
  release antérieure, **jamais révoquée**. Le service n'a pas échoué — il ne tournait pas, zéro
  entrée au journal.
- Second défaut indépendant : le fichier de protection désignait un artefact qui n'existait plus.
  Réactiver le ramasse-miettes tel quel l'aurait laissé **refuser toute éviction en silence**
  jusqu'à la saturation suivante.
- 347 Go récupérés, seuils remis au nominal, minuterie réarmée.

### Note — pourquoi ces défauts n'étaient pas visibles

Le pool saturé faisait échouer le gate des liens morts **sur une erreur d'écriture disque avant
toute compilation**. Le scan paraissait rapide parce qu'il ne faisait rien. Une fois le pool
libéré, il met plus de deux minutes et rend enfin son verdict. Un dispositif qui échoue tôt pour
une mauvaise raison masque ce qu'il aurait dû mesurer.

## [2.0.2] — 2026-08-16 — *internal milestone, published in 2.1.0*

Lot d'assainissement de la cohérence du vault. Huit cartes livrées, chacune déployée et vérifiée
en production avant clôture. Un fil commun les relie : **des dispositifs qui rassuraient au-delà
de ce qu'ils garantissaient.**

### Fixed — la réparation des embarquements enfilait dans une table morte (F-175)

- `backfill-embeddings` déposait ses travaux dans `jobs_v2`, table que plus aucun consommateur
  ne drainait depuis le 2026-05-29, **tout en rapportant un succès**. C'était l'outil de
  réparation lui-même qui était silencieusement dégradé : le lancer aurait clos un défaut sur
  une commande sans effet, en laissant une trace affirmant le contraire.
- Le compteur est désormais **relu dans la table** que le worker draine, jamais rapporté par la
  fonction. Un enfilage sans effet échoue franchement.
- **Garde-fou de volume** : tout tenant autre que le principal exige une borne explicite, et un
  plafond dur s'applique même à lui. Sans cette garde, le correctif rendait praticable un
  enfilage de plusieurs milliers de travaux jusque-là inoffensif parce qu'inerte.

### Fixed — 37 notes existaient sur disque sans être indexées (F-166)

- Nouvelle sous-commande `reindex-orphans`. Elle **appelle l'entonnoir d'écriture** au lieu de le
  réimplémenter : une note ré-indexée obtient donc sa ligne d'index, son empreinte de dérive et
  son travail d'embarquement dans le même geste.
- Vérifié en production : les trois compteurs ont progressé d'exactement +37 chacun. Un
  court-circuit de l'entonnoir aurait produit l'un sans les autres.
- Ces notes portaient `status: live` et étaient invisibles à toute requête depuis 99 jours.

### Fixed — la détection de dérive ne couvrait que 2,4 % du vault (F-174)

- Rétro-remplissage des empreintes : **80 → 3255 entrées, soit 100 % de couverture**. Le scan
  rendait « aucune dérive » sur un vault dont il ne regardait qu'un quarantième.
- Le scan couvre désormais **les deux directions** — il énumérait l'index, donc il ne pouvait par
  construction pas voir un fichier que l'index ignore — **et les trois représentations**, fichier,
  index et vecteur.
- Nouvelle commande `drift-scan`, en lecture seule, qui **sort en code 2** sur dérive et en code 1
  sur erreur d'exécution : « j'ai trouvé » et « je n'ai pas pu regarder » ne sont pas confondus.
- Le prédicat d'embarquabilité est désormais **dérivé d'une source unique** dans `gradatum-core`
  (`NoteStatus::ALL`, `embeddable_default_sql_list`), consommée par le détecteur **et** par le
  réparateur. Ils divergeaient : le détecteur ne comptait que les notes vivantes, le réparateur
  couvrait les trois statuts embarquables — une note en revue sans vecteur n'était donc signalée
  par personne.

### Fixed — la passerelle rétrogradait le rôle `system` en `user` (F-170)

- 183 requêtes par jour voyaient une consigne de cadrage transformée en propos d'utilisateur.
  La cible interne sait porter ce rôle : la conversion n'était pas un repli faute de mieux, mais
  une perte d'information là où la représentation existait.
- Le repli avec avertissement **subsiste** pour les rôles authentiquement inconnus. En sortant
  `system` de ce chemin, l'avertissement redevient un événement rare et signifiant — émis 183
  fois par jour, il n'avertissait plus personne.

### Fixed — `last_indexed_at` rendait toujours `null` (F-169)

- Le champ existait au contrat de sortie avec une valeur câblée en dur, ce qui est **plus
  trompeur qu'une absence** : un consommateur en conclut « jamais indexé ».
- La source retenue correspond au nom du champ. Une date de vérification d'empreinte était
  disponible et a été **écartée** : servie sous un nom d'indexation, elle aurait remplacé un nul
  honnête par une valeur fausse.
- Le nul ne subsiste que pour une seule raison — corpus vide. Une erreur de stockage est
  **propagée** et non dégradée en nul, contrairement aux autres champs du même endpoint : un nul
  sur échec serait indistinguable du défaut corrigé.

### Fixed — l'écriture hors de l'entonnoir était possible (F-176)

- La convergence devient une **propriété imposée** et non une discipline d'appelant. Une garde
  décore la couche de stockage et refuse tout chemin de note ; l'entonnoir écrit par un canal
  privilégié visible seulement à l'intérieur du crate.
- Portée exacte, documentée dans le code : la garde couvre tout écrivain **dans le processus**.
  Elle ne couvre pas — et ne peut pas couvrir — un écrivain hors processus, qui est le vecteur
  ayant causé les orphelins ci-dessus. Celui-là reste fermé par politique d'ingestion.
- Asymétrie assumée : **prévention** pour l'orphelin ici, **détection** pour l'entrée d'index
  fantôme par le scan de dérive.
- `StorageError::WriteRejected` ajoutée — extension additive sur une énumération non exhaustive.

### Added — les cartes du plan produit sont interrogeables par leurs rôles (F-171)

- Deux colonnes typées dérivées à l'écriture, exposées au filtrage. La question « quelles cartes
  de correction sont encore ouvertes ? » n'avait aucune réponse par le chemin prévu.
- L'oracle de vérification applique **la même sémantique d'extraction que le validateur de
  schéma**. Mesuré : un oracle par correspondance de sous-chaîne attribue 406 types pour 329
  cartes ; l'ancré partitionne exactement.

### Investigated — huit travaux en échec définitif, classés sans réparation (F-168)

- Les huit visent des notes absentes de l'index, du disque **et** des archives. Irrécupérables
  par disparition de la cible ; les rejouer n'aurait aucun effet.
- Le lien supposé avec les notes dépourvues de vecteur est **écarté** : populations disjointes.
- Défaut découvert en instruisant : les huit portent le même message, « nombre maximal de
  tentatives atteint ». La cause première est écrasée par le message d'épuisement — **un travail
  mort est donc inanalysable**.

### Ce que cette version ne fait pas

- **La file héritée n'est pas supprimée.** L'instruction préalable a montré qu'elle est recréée à
  chaque démarrage du serveur et du worker, lue par une route de rétrocompatibilité, et recréée
  par une migration immuable. La retirer franchirait l'API publique d'un crate publié — donc un
  cycle majeur, pas un correctif. Reportée, avec son argument intact : cette table conserve le
  contenu de notes supprimées.
- La séparation des scripts et la couverture de déploiement des composants distants sont
  reportées à une version dédiée à la publication.
- Deux commandes livrées ici — `reindex-orphans`, `backfill-checksums` — sont des outils
  d'administration hors ligne : elles écrivent dans l'index, dont le serveur est le seul writer
  déclaré, et s'exécutent services arrêtés.

## [2.0.1] — 2026-08-15 — *internal milestone, published in 2.1.0*

### Fixed — la détection de dérive était inerte depuis la v1.0.0 (F-165, geste 1/4)

- `Vault::write_note_inner` alimente désormais `file_checksums` à chaque écriture de note.
  Cette table est la **seule source d'énumération** du scan de dérive (`scan_phase_a`) ; elle
  contenait **0 ligne**. Le scan rendait donc un résultat entièrement nul à chaque exécution,
  tout en exposant des métriques — la forme la plus coûteuse du faux vert, puisqu'elle ne se
  tait pas, elle rassure. Le dépôt documentait lui-même cette inertie en commentaire.
- Les trois entrées d'écriture (`write_note`, `write_note_with_id`, `write_if_match`) passent
  par `write_note_inner` : toutes alimentent la table.
- **Fail-open délibéré** : un échec du calcul ou de l'écriture du checksum est journalisé et
  n'interrompt pas l'écriture de la note. Perdre un checksum vaut mieux que perdre une note —
  même principe que le découplage curate/embed déjà en place.
- `compute_prefix_4kb_bytes` et `compute_full_sha256_bytes` deviennent publiques dans
  `gradatum-index`. **Extension purement additive.** Motif : le producteur (chemin d'écriture)
  et le consommateur (scan) doivent hacher à l'identique — dupliquer la primitive ferait
  diverger les deux et marquerait chaque fichier comme dérivé.

**Ce que cette version ne fait pas encore**, et qui suit dans le même lot : le rétro-remplissage
des fichiers antérieurs, l'énumération dans le sens disque → index (sans laquelle un fichier que
l'index ignore reste invisible), et la dimension vectorielle.


## [2.0.0] — 2026-08-10

> **Public release note — this is a `1.0.0 → 2.0.0` jump.** `1.0.1` and `1.0.2` were never
> published (no crates.io release, no GitHub release, no public tag), so this version carries
> the accumulated changes of all three: anyone updating from the last public release, `1.0.0`,
> receives the `1.0.1` and `1.0.2` deltas together with the ones below. The upgrade guide covers
> the full path from `1.0.0` — see `docs/UPGRADING-1.0.0-to-2.0.0.md`.

**Identity is carried by the credential.** The owner of the presented API key is the sole
source of a caller's identity: there is no default identity, no client-declared identity, and
no silent fallback. A request that cannot be attributed to a credential is refused rather than
served under a default. See `docs/UPGRADING-1.0.0-to-2.0.0.md` for the migration guide and the
pre-flight check.

### Breaking changes

- **The `main-agent` bootstrap identity is now an installation prerequisite.** The server
  requires the `main-agent` identity to hold an active key; without it, it has no identity to
  serve. `gradatum-admin init` now mints this key while initialising a root, so **new
  installations need no extra step**. Roots created before 2.0.0 that never held a `main-agent`
  key must create one:
  `gradatum-admin api-key create --owner main-agent --scopes vault_read,vault_search,vault_write,write`.
- **A deployment with no active key stops being served.** Before 2.0.0, a request whose caller
  could not be resolved fell back to a default agent identity and was still served. That
  fallback is removed. An installation that never created any API key — and relied on the
  fallback — now has every request refused until a key is minted. On an initialised multi-tenant
  server the refusal is the actionable **503** below; otherwise it is a plain **401**.
- **A client-supplied `author` on a write is refused with 400.** Attribution is derived from the
  credential only. Any write carrying an explicit `author` is rejected with
  `400 InvalidInput` ("author provided: identity comes from the credential, it is not
  self-declared (R2)").
  This is the one change of the three that can break an integrator **silently**: no in-process
  caller injects an `author`, and the published `gradatum-sdk-rs` cannot — it is a placeholder
  with no client surface, so it sends nothing at all. But
  `gradatum-dto`'s `VaultWriteRequest` still exposes `pub author: Option<String>`
  (`crates/gradatum-dto/src/vault_write.rs`), so **any hand-written REST client or DTO** that
  populated that field will start receiving 400. **Migration:** stop sending the `author` field;
  the identity is taken from the API key that authenticates the request. To act under a distinct
  identity, present that identity's own key.
- **`gradatum-storage`: the filesystem-only guard is removed, function and all.**
  `nfs_check::ensure_local_filesystem` no longer exists, and the `GradatumError` variant it
  raised, `VaultOnNfs`, is removed with it. Code that called the function, or matched on the
  variant, fails to compile. See "Changed" below for what replaces the restriction it enforced.
- **`gradatum-vault`: `Vault::storage()` now returns `&dyn Storage` instead of the concrete
  `&FileStorage`.** Code that depended on the concrete filesystem type through this accessor
  fails to compile; migrate to the `Storage` trait, which exposes the same
  read/write/list/delete/stat operations regardless of backend.
- **`gradatum-core`'s `GradatumError` and `gradatum-storage`'s `StorageError` are now
  `#[non_exhaustive]`.** Both error enums carry the attribute from this release on, so every
  future variant addition is a non-breaking change instead of a major. This lands in the same
  release that also changes each enum's variant set — `GradatumError::VaultOnNfs` is removed
  (above) and `StorageError::ConfigInvalid` is added (see "Added" below) — so an exhaustive
  `match` on either from outside its crate needs updating this release regardless, and must add a
  `_` arm to stay future-proof. `GradatumError` is the umbrella error of `gradatum-core` and the
  most widely matched type on the whole public surface: the attribute has to land now, because
  adding `#[non_exhaustive]` *after* the first publish is itself a breaking change and the window
  closes for good at the first `cargo publish`.
- **`gradatum-core`: `IndexStore::persist_curated_index_atomic` changes signature.** The
  parameter that took a slice of `(source, target)` pairs is replaced by a single `CuratedLinks`
  value (see "Added"). `IndexStore` is a public trait — **any external implementation of it
  stops compiling.** The new type carries the edges *and* a flag stating whether they are the
  authoritative, complete set of outgoing links for the note: passing it means explicitly
  declaring whether edges missing from the list should be deleted. The non-destructive choice is
  the one that deletes nothing — `false` keeps the historical upsert-only behaviour, `true` only
  where the full edge set was actually recomputed from the note's current body.
- **`gradatum-core`: `VaultConfig` gains a public field, `storage`.** Same shape as the
  `PersistCuratedRequest` field addition below: `VaultConfig` is not `#[non_exhaustive]`, so a
  struct literal that lists every field explicitly fails to compile until `storage` is added. The
  field itself is the one described under "Vault storage on an S3-compatible object backend"
  below; on the wire it changes nothing — an absent `[storage]` section in `config.toml` still
  loads and defaults to the local filesystem backend, exactly as before. **Migration:**
  `VaultConfig` derives `Default`, so `..Default::default()` in any literal that constructs it
  always compiles; alternatively add `storage: StorageBackendConfig::default()` explicitly.
- **`gradatum-dto`: `PersistCuratedRequest` gains a public field, `links_authoritative`.** A
  struct literal that lists this type's fields explicitly — the idiomatic way to construct a
  public DTO in Rust — fails to compile until the new field is added. **This breaks the Rust
  construction, not the wire contract**: the field carries `#[serde(default)]`, so a JSON
  payload from a caller that predates this release, and omits it, still deserializes, defaulting
  to `false` — the same non-destructive behaviour described above. Add the field to any literal
  that lists this struct explicitly (or build it with `..Default::default()` / a builder);
  callers that only serialize or deserialize the type need no change.
- **`gradatum-dto`: 33 public request structs are now `#[non_exhaustive]`.** Every REST/MCP
  request DTO in the crate carries the attribute now — among the ones integrators reach for most,
  `VaultSearchRequest`, `VaultReadRequest`, `VaultWriteRequest`, and `VaultListRequest`. A struct
  literal built from outside the crate and naming every field — the idiomatic way to construct a
  public DTO in Rust — no longer compiles, even where the field list itself hasn't changed. This
  is separate from, and on top of, the field addition to `PersistCuratedRequest` called out above:
  that type is one of the 33, the other 32 change with no accompanying field addition. **Migration:**
  29 of the 33 ship their own `::new(...)` constructor for the required fields (e.g.
  `VaultSearchRequest::new(query)`); the remaining four — `VaultListRequest`, `VaultTimelineRequest`,
  `VaultArchivesListRequest` and `ProactiveRecallRequest` — expose no `::new` and are built with
  `..Default::default()` (all four implement `Default`). Code that only serializes or deserializes
  these types over the wire,
  rather than constructing them as a Rust literal, is unaffected either way.
- **`gradatum-auth`: `Claims` is now `#[non_exhaustive]`.** The JWT claims struct — returned by
  `JwtService::verify` and the payload of every issued token — no longer accepts a struct literal
  built from outside `gradatum-auth`. Same attribute, same reason as the 33 request DTOs above,
  applied to the one auth type most likely to gain a field for a *security* reason: a future
  token-binding claim (e.g. `cnf`) could be added within `2.x` without a further major bump.
  Adding `#[non_exhaustive]` *after* the first publish would itself be breaking, so it lands now.
  External consumers never construct `Claims` — they receive it from `verify` — so no wire or
  in-process caller is affected; the only literal construction was in a downstream integration
  test, updated in this release to mint-then-verify. **Migration:** obtain `Claims` from
  `JwtService::sign` + `verify` rather than a struct literal.
- **`gradatum-server`: `ConfigError` is now `#[non_exhaustive]`.** The boot-time configuration
  error enum can gain a variant without a major bump for the rest of `2.x` — deliberately, because
  a new class of configuration leak or fail-closed check is the likely reason it grows, and each
  such addition would otherwise cost a major. An exhaustive `match` on `ConfigError` from outside
  `gradatum-server` now needs a `_` arm; matching or constructing a specific known variant is
  unaffected, and no such external exhaustive match exists in-tree.
- **Eight more public types across `gradatum-auth`, `gradatum-warden`, `gradatum-acl-auth` and
  `gradatum-acl-policy` are now `#[non_exhaustive]`.** Same attribute, same reason as `Claims`,
  `ConfigError` and the two error enums above: the attribute has to be locked in before the first
  publish, since adding it afterwards is itself a breaking change. The seven enums —
  `JwtError`, `TokenScope` and `RevocationError` (`gradatum-auth`), `WardenDecision` and
  `WardenError` (`gradatum-warden`), `ApiKeyError` (`gradatum-acl-auth`), `AclError`
  (`gradatum-acl-policy`) — now require a `_` arm in any exhaustive `match` from outside their
  crate; matching or constructing a specific known variant is unaffected. The one struct,
  `WardenConfig` (`gradatum-warden`), no longer accepts a struct literal from outside the crate —
  including one that ends in `..Default::default()`, since functional-update syntax is refused on
  a `#[non_exhaustive]` type across crates just as a full literal is. **Migration:** build it from
  `WardenConfig::default()` (the type derives `Default`) and assign the public fields you need to
  override — every field is `pub`. **Tooling note:** all ten additions in this release
  (`GradatumError`, `StorageError` above, plus these eight) are detected by `cargo-semver-checks`
  0.50 — the lint `enum_marked_non_exhaustive` catches the nine enums and
  `struct_marked_non_exhaustive` catches `WardenConfig`, each pointing at the exact definition
  site. On a `2.0.0` major bump these are expected, allowed changes, so the release-readiness gate
  records them as informational ruptures rather than failures. This changelog entry is the
  human-readable record of the same set the tooling verifies.
- **`gradatum-engine`: `EventLogErrLabels::encode` and `ReqLabels::encode` change signature** —
  their parameter moves from a by-value `LabelSetEncoder` to `&mut LabelSetEncoder`. This is not
  a change this project chose: both types derive their `encode` implementation from the
  `prometheus-client` metrics library, and the new signature simply follows that dependency's
  own upgrade. Any consumer implementing either trait directly must update to match — a reminder
  that a dependency bump can move a crate's public surface even when no first-party line of code
  changed intent.
- **`gradatum-mcp-stub` is removed from the distribution.** It is no longer built by either
  release workflow, so it is absent from the release archives (the `gradatum-mcp-*.tar.gz`
  group no longer exists — see `docs/guides/B-install-binaries.md`) and from the Docker image,
  where it was already never included. The stub was a stdio→HTTP bridge for MCP hosts that
  cannot send a custom auth header — chiefly Claude Desktop, which only takes a URL and drives
  its own auth flow. But the stub was only ever built for `x86_64-unknown-linux-gnu`, a target
  Claude Desktop does not run on (macOS, Windows only): it could not serve the audience it was
  maintained for. `gradatum-server`'s native MCP transport (`/mcp`, Streamable HTTP, API key as
  a `Bearer` credential) is unaffected and is now the only integration path — see
  [Guide D](docs/guides/D-mcp-and-studio.md). **Migration:** switch the MCP client configuration
  from a stdio `command` entry to an HTTP `url` entry:
  ```json
  {
    "mcpServers": {
      "gradatum": {
        "type": "http",
        "url": "http://127.0.0.1:19090/mcp",
        "headers": { "Authorization": "Bearer ak_your_api_key" }
      }
    }
  }
  ```
  The `gradatum-mcp-stub` crate itself remains published at its last version, `1.0.0`, on
  crates.io and is not republished at later tags — the same last-published-version caveat this
  changelog already documents for `gradatum-cli` (see `RELEASE-POLICY.md` §AM3). If your
  toolchain resolves `gradatum-mcp-stub = "2.0.0"` or similar, that version does not exist; pin
  to `1.0.0` only to retrieve the retired binary, and migrate to the native transport above.

### Added

- **Soul-write privilege gated by the `identity_write` scope.** Writing an agent's soul
  (`identity/*`) now requires the dedicated `identity_write` scope, distinct from `admin`,
  `write` and `service`. This narrows soul mutation to explicitly privileged credentials rather
  than any writer. The bootstrap key does not carry it, and it is never inherited from a write
  scope — see `docs/UPGRADING-1.0.0-to-2.0.0.md` ("Managing other agents' souls") for how to grant
  it, and why it stays disjoint.
- **Key registry reset — `gradatum-admin api-key reset`.** Returns an installation to a clean
  credential state (revokes every key, leaving the audit trail intact) and brings the server
  back to the uninitialised state the bootstrap step starts from. It touches the key registry
  only: notes, their content and their attribution are never affected. Requires explicit
  confirmation; treat it as a maintenance operation, not a routine one.
- **One active key per identity.** `api-key create` now refuses an identity that already holds an
  active key and points to `api-key rotate`, which revokes and mints the replacement atomically
  while carrying the identity over unchanged. This makes "one identity = one active credential"
  an enforced invariant.
- **Distinct 503 for an uninitialised registry.** On an initialised multi-tenant server, an
  unauthenticated request against a key registry with no active key receives a **503** whose body
  names the bootstrap identity and the exact `api-key create` command, instead of a bare 401.
  The body never carries the registry's disk path (see `SECURITY.md`).
- **Docker deployment.** Multi-service `docker-compose.yml` orchestrating 5 gradatum services
  (server, worker, init, gateway, engine) alongside 2 external llama.cpp containers, host
  networking (`network_mode: host` on both server and worker — the server's loopback-only
  bind fails closed without TLS, so the worker shares its network namespace to reach it on
  127.0.0.1 instead of through a bridge port), token environment injection, and CPU-only
  llama.cpp configuration for the embedding and chat containers. The `Dockerfile` is a single
  multi-stage build producing five binaries from the workspace (server, worker, admin,
  gateway, engine).
- **Engine deployment formalisation.** `scripts/install-gradatum-services.sh` gains `--with-engine`
  and `--with-gateway` flags; `scripts/deploy-gradatum-local.sh` gains `--engine` for coordinated
  engine+server deployment. Engine documentation and usage examples added to `docs/`.
- **Vault storage on an S3-compatible object backend.** A new `[storage]` configuration section
  (`StorageBackendConfig`, exposed as `VaultConfig::storage`) chooses the backend: `service =
  "fs"` — the default, byte-identical to prior behaviour, including when the section is absent
  entirely — or `service = "s3"` for any S3-compatible provider (AWS, OVH, MinIO, Ceph,
  Scaleway…) reached through a configurable endpoint. The new `gradatum-storage::build_storage`
  factory (module `factory`) builds the configured backend; an unknown service name, or one whose
  Cargo feature isn't enabled in the build, fails at construction with the new
  `StorageError::ConfigInvalid`, naming what's wrong, instead of falling back silently. Only
  non-secret connection parameters (endpoint, bucket, region, root) live in configuration —
  credentials are read exclusively from the process environment via OpenDAL's native credential
  chain (e.g. `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`). `FileStorage` is now exported
  unconditionally rather than behind a feature-gated path. Note bodies themselves are written to
  the backend in plaintext, with no encryption applied by gradatum — see
  [SECURITY.md § Privacy posture](SECURITY.md#privacy-posture).
- **`gradatum-storage::install_object_backend_defaults()`, a new public function.** Installs,
  once and in order, the process-wide crypto provider (`aws_lc_rs`) then the OpenDAL HTTP
  transport (`opendal-http-transport-reqwest`) that every S3/GCS/Azure operation needs — OpenDAL
  0.58 made the transport pluggable, and a build without one installed rejects every
  object-backend call before the first network packet with the permanent `StorageError::ConfigInvalid`
  above, never a silent no-op. `gradatum-server` calls it once at boot, off the TLS path
  (`[server.tls]` is optional and gates nothing here). Any integrator embedding
  `gradatum-storage` outside that binary and constructing an S3/GCS/Azure backend via
  `build_storage` must call it too, before the first operation, or hit that same
  `ConfigInvalid`. Both installs are first-installed-wins and safe to call repeatedly; a no-op
  on a build with no object-backend feature enabled (`fs`-only, the default).
- **`gradatum-admin repair-note-links`**: reconciles a note's recorded links against its current
  body (see "Fixed" below). Dry-run by default; pass `--execute` to apply.
- **`gradatum-core`: `CuratedLinks`, a new public type**, bundling a note's outgoing link edges
  with an `authoritative` flag. It exists to make the breaking signature change of
  `IndexStore::persist_curated_index_atomic` (above) possible: the edges and the authority flag
  now travel together in one value, so a caller cannot pass one and forget the other.
- **`gradatum-admin init` now writes an explicit, commented `[apalis.workers.curate]` section**
  into the `server.toml` it generates, symmetric with the existing `[embed]` and `[curator]`
  sections (`crates/gradatum-admin/src/init.rs`). The curate worker's `concurrency` and
  `timeout_secs` were compiled defaults with no line in any configuration file — a fresh install
  now sees them, and the comment explains what each one is for. The other worker kinds (`embed`,
  `reindex`, `purge`, `forget`, `distill`, `validate`) keep their compiled defaults and are not
  written out; override any of them with a matching `[apalis.workers.<kind>]` table.

### Changed

- **The vault no longer refuses to start on a network filesystem.** Earlier versions ran a
  startup check and refused to start when the vault's local root was detected on an NFS (or
  similar network) mount; that check is removed. Where the vault's data lives — and any
  reliability trade-off a network mount brings — is now the deploying operator's decision, not
  something the product enforces. The local backend remains the default; see "Vault storage on
  an S3-compatible object backend" above for the supported alternative.
- **Dependency refresh across the workspace.** Also removes the worker's legacy processing
  engine, unused since the current queue backend became the active path, and drops the
  unmaintained serialization library it pulled in.
- **The `curate` worker now runs serial by default: `concurrency` 2 → 1, `timeout_secs` 30s →
  300s** (`WorkersConfig::default_curate`, `crates/gradatum-worker/src/monitor.rs`). This is a
  compiled default, so it applies to any deployment that does not set `[apalis.workers.curate]`
  explicitly — before this release, that was every deployment, since `gradatum-admin init` never
  wrote the section (see "Added" above). The curate worker is LLM-bound: it calls out to the
  chat endpoint configured under `[curator.llm]` for every job, and the bundled Docker stack runs
  that endpoint (`llama-chat`) as a single-slot server. Two curate jobs in flight were contending
  for that one slot, and the previous 30s per-job timeout could fire before a classification call
  under load returned at all. Serial execution (1 job at a time) removes the contention outright,
  and the new 300s ceiling leaves margin above the 60s client-side timeout already configured in
  `[curator.llm]`, so that timeout fires first on a genuinely stuck call. **The trade-off is
  throughput: curation now processes at most one note at a time, where it previously processed
  two.** This is deliberate — a concurrency of 1 does not depend on how fast the chat endpoint
  happens to be on a given machine, where a concurrency of 2 does. The bundled
  `docker-compose.yml` pins the matching `llama-chat` service to `--parallel 1`, replacing
  llama.cpp's automatic slot layout (which otherwise split the unified KV cache into multiple
  narrower slots) with the single full-context slot the now-serial worker actually uses;
  `--ctx-size` is untouched.
- **`llm_review_max_tokens` written by `gradatum-admin init` lowered from 1024 to 128**, matching
  the arbitrated value already documented in `examples/configs/curator.toml` (L-01). This only
  changes what a fresh `init` generates — an existing `server.toml` keeps whatever value it
  already has.

### Fixed

- **Multi-tenant hardening (production path).** Stale-lease recovery now runs `complete()` and
  `fail()` on `SqliteQueueStore` — the production queue backend — rather than on the `jobs_v2`
  admin path only, closing a critical isolation gap where a stale lease left a job permanently
  stuck. JWT verification now validates `iss` and `sub` claims; revocation is tenant-scoped.
  ApiKey listing filters by tenant. Worker dequeue properly documents tenant attribution via
  `ensure_job_tenant`.
- **A note's recorded links no longer accumulate stale entries.** They were append-only: once a
  link's target changed, the old target stayed recorded indefinitely, so the graph reflected a
  note's history rather than its current body. `gradatum-admin repair-note-links` (see "Added"
  above) backfills existing notes; new writes reconcile automatically.
- **A malformed configuration value no longer leaks into the startup log.** A secret placed in
  the wrong configuration field could previously appear in clear text in the error produced when
  configuration loading failed. Configuration-loading errors are now redacted before being
  logged or returned to the caller.
- **Audit-pass artifacts now go through the storage layer.** They previously wrote to the local
  filesystem directly; on a deployment using the object storage backend (above), they would have
  stayed on local disk while the vault's notes moved elsewhere. They now write through the same
  `Storage` backend and configuration as the vault itself.
- **Studio: `react-router-dom` migrated to its next major version.** No route-level API changes
  for consumers of the Studio UI.
- **Two engine settings documented themselves as active when they were not.** `[engine].max_tokens`
  described itself as a per-request generation cap "clamped by the binary", and `[engine].warm_up`
  as an `eager`/`lazy` strategy. Neither was ever read: generation length is not capped by
  `max_tokens`, and warm-up is always eager. Both are now documented as accepted-but-not-enforced
  and annotated as such in the source. **If you relied on `max_tokens` as a safety limit, you did
  not have one** — pass `--n-predict` through `[engine].extra_args`, which the child process does
  honour. The fields are kept so existing configuration files keep loading; wiring them is a
  deliberate behavioural change deferred to a later version rather than slipped into a patch.

## [1.0.2] — 2026-08-06

### Fixed

- **MCP vault_write : author par défaut = agent du header `X-Gradatum-Agent`.** Le
  `trust.subject()` (sub du token JWT) est partagé pour tous les canaux MCP partageant
  la même api-key — il ne reflète PAS le canal. Le handler MCP résout désormais l'author
  depuis le header `X-Gradatum-Agent` (identité par canal), avec chute sur le
  `trust.subject()` conservée pour le chemin REST. Version LOCALE interne — non publiée
  (pas de crates.io, pas de tag public, pas de GitHub).

## [1.0.1] — 2026-08-05

### Fixed

- **Engine: `--chat-template-kwargs` autorisé en `extra_args`.** Le flag
  `--chat-template-kwargs '{"enable_thinking":false}'` (désactivation du mode thinking des
  modèles Qwen3.6-35B-A3B — économise ~400-1600 tokens de raisonnement/requête) est désormais
  accepté par l'allow-list `ALLOWED_EXTRA_FLAGS` du superviseur `gradatum-engine`. La valeur
  JSON est passée telle quelle en argv direct (pas d'interprétation shell), et le flag est
  sûr car sa valeur est contrôlée par la configuration engine, sans ouverture réseau ni
  lecture/écriture de chemin arbitraire. Version LOCALE interne — non publiée (pas de
  crates.io, pas de tag public, pas de GitHub).

## [1.0.0] — 2026-08-04

First stable release. SemVer strict starts here: public APIs on `1.x` follow the LTS
promise described in `RELEASE-POLICY.md`. This release closes the multi-tenant/multi-vault
isolation foundation, completes the FR→EN user-facing string migration, and aligns the
full workspace (27 publishable crates) to `1.0.0`.

### Breaking changes

- **`gradatum-core`: the four storage extension traits change signature — every out-of-tree
  implementation must be rewritten.** `IndexStore`, `DocumentStore`, `QueueStore` and
  `VectorStore` are the contract boundary for third-party storage backends (RFC-0001), and
  **24 of their methods** now carry an explicit vault or tenant scope. **17 of the 24 have no
  default implementation**, deliberately: a defaulted method would let an existing backend keep
  compiling while silently ignoring the new scope — green tests over unscoped code, which is the
  exact failure mode this release closes. For those 17, an external backend stops compiling until
  every impl is updated.

  **The remaining 7 carry a pre-existing default body, kept as is, and that default is a silent
  success.** `IndexStore::count_fts_matches` returns `Ok((0, false))`; `get_statuses` and
  `get_anchor_ms_batch` return an empty map; `set_note_trust` and `delete_redirect_by_ulid`
  return `Ok(0)`; `QueueStore::count_jobs_by_status` returns an empty map and `latest_job`
  returns `Ok(None)`. A backend that does not update them keeps compiling and answers *no match*
  / *no row touched* against an `AclCheckedVaultId` it never read — the very failure mode
  described above, left open on those seven. **What this release guarantees is therefore narrower
  than a full stop**: a hard compile break on 17 methods, not a proof that every re-scoped method
  honours its scope. Out-of-tree implementors must audit the seven by hand; removing those
  defaults is itself source-breaking and cannot land inside `1.x`.

  The change is not uniform, and the shape decides how each impl adapts:
  - **10 methods take an ACL-checked vault handle** — the new
    `gradatum_core::scope::AclCheckedVaultId`, obtained through `attest_read_checked`,
    `attest_write_checked` or `for_system_task`, never built from a bare string. Three of them
    **gain** the parameter in leading position: `DocumentStore::downgrade_note`,
    `patch_note_status`, `update_note_locus`. The other seven **retype** a vault parameter they
    already carried — `&VaultId` → `&AclCheckedVaultId` for `IndexStore::count_fts_matches`,
    `search_fts_with_snippet`, `timeline`; `&str` → `&AclCheckedVaultId` for
    `IndexStore::get_anchor_ms_batch`, `get_statuses`, `get_titles_sections` and
    `VectorStore::search_semantic`.
  - **9 methods gain an untyped leading `vault_id: &str`** — `IndexStore::get_trust`,
    `set_note_trust`, `resolve_redirect`, `upsert_redirect`, `delete_redirect_by_ulid`,
    `DocumentStore::get_content_hash`, `upsert_note_title`, `VectorStore::get_note_embedding`,
    `insert_note_embedding`. These carry the scope without carrying the attestation: the type
    does not prove the vault was ACL-checked, so the responsibility stays at the call site.
    Converging them onto `AclCheckedVaultId` would itself be a source-breaking retype, so it
    cannot land inside `1.x` — the split between the two groups is frozen until `2.0.0`.
  - **`IndexStore::search_fts_for_forget` retypes** its vault parameter `&str` → `&VaultId`;
    arity unchanged.
  - **The four `QueueStore` methods take a tenant filter, not a vault.** `get`, `cancel` and
    `count_jobs_by_status` gain a trailing `tenant_filter: Option<&str>`; `latest_job`
    converts the `&str` it already had into `Option<&str>`. `None` means no tenant clause and
    is byte-identical to `0.8.0`; `Some(t)` adds `tenant_id = t`, which is what makes a
    cross-tenant `get` read as `None` (404 anti-disclosure at the handler) and a cross-tenant
    `cancel` a no-op on zero rows.
  - **`DocumentStore::upsert_note_title` also changes its return type**, `Result<()>` →
    `Result<usize>`. It is the only method in the group that breaks *callers* and not just
    implementors.
- **`gradatum-core`: `OverrideScope::Locus` and `OverrideScope::Bearer` become struct variants
  carrying their vault.** `Locus(LocusId)` is now `Locus { vault: VaultId, locus: LocusId }`
  and `Bearer(BearerId)` is now `Bearer { vault: VaultId, bearer: BearerId }`;
  `Vault(VaultId)` is unchanged. Construction and pattern matching both break — a tuple
  pattern no longer applies to either variant. The `IndexStore::upsert_override_raw` and
  `get_override_raw` signatures are untouched (they take `&OverrideScope`), so the break
  surfaces at the construction and match sites, not on the trait. **The vault is the isolation
  key**: `upsert_override_raw` used to persist both scopes under the sentinel
  `vault_id = '_unset'`, a bucket shared by every vault, so under the composite primary key
  `(vault_id, note_id, scope_kind, scope_id, override_type)` introduced by migration `0034`
  two vaults holding an override on the same `note_id` and the same `scope_id` clobbered each
  other on write and read across vaults on read. Migration `0036` re-keys the legacy
  `'_unset'` rows to `'main'` — one column value changes, no schema change, and
  `Vault`-scoped overrides never used the sentinel. The serialized form changes with the
  variants:
  `#[serde(tag = "kind", content = "id")]` now emits
  `{ "kind": "locus", "id": { "vault": "main", "locus": "decisions" } }` where it emitted
  `{ "kind": "locus", "id": "decisions" }`.
- **`gradatum-curator`: `CuratorPipelineConfig` loses three public fields** —
  `llm_review_endpoint`, `llm_review_model`, `llm_review_timeout_ms`. They were parsed and
  propagated into the struct but never read by the pipeline. `CuratorPipelineConfig` is not
  `#[non_exhaustive]`, so removing them breaks both literal construction and field access for
  any external consumer. Callers that set or read these fields must drop them: the review
  endpoint, model, and timeout are taken from `[curator.llm]` `base_url` / `model` /
  `timeout_ms`.
- **`server.toml`: `auth.jwt_public_key_path` and `auth.jwt_private_key_path` removed.** The
  JWT signing material is the seed at `<storage.root>/config/jwt-signing-key.secret`
  (`kid = gradatum-v0`), now the single source of truth for both `gradatum-server` and
  `gradatum-admin token issue`. **Existing configs still boot**: a `server.toml` that still
  carries these two keys is accepted and the keys are ignored.
  **One case requires operator action before upgrading.** The key directory used to be the
  parent of `jwt_private_key_path` whenever that parent sat under `storage.root`, and falls
  back to `<storage.root>/config` otherwise; it is now always `<storage.root>/config`. A
  deployment whose seed lived under `storage.root` but outside `config/` — for example
  `jwt_private_key_path = "<storage.root>/secrets/jwt.private.pem"` — will find no seed at
  the new location and **generate a fresh one at first boot, invalidating every JWT in
  circulation** (API keys are unaffected — they are verified against their own store). The
  server logs a `WARN` naming the path, but the boot succeeds and nothing
  distinguishes this case from a legitimate first startup. Move the existing
  `jwt-signing-key.secret` into `<storage.root>/config/` before upgrading, or plan for every
  consumer to re-exchange its API key. Deployments already using the default layout are
  unaffected. `gradatum-admin init` no longer generates the PEM pair, and any previously
  generated PEM files are inert.
- **`gradatum-worker --db` is now validated.** The flag stays optional; when supplied, a value
  that diverges from the path derived from `storage.root` makes the worker refuse to start.
  Previously a divergent `--db` silently created a second, empty queue database — the worker
  then acquired leadership and processed zero jobs without reporting an error.
- **`gradatum-admin api-key create`: `--scopes` is now required, and a scope set that grants
  no write access is refused.** Write access comes only from the exact scopes `write`,
  `admin` or `service` (`gradatum_acl_auth::WRITE_SCOPES`); anything else — `vault_write`
  included — grants none. The command previously defaulted `--scopes` to `vault_read` and
  accepted any string, so `api-key create --owner x` minted a key that looked writable and
  was rejected on every write once `multi_tenant.enabled = true`. Two invocations change:
  omitting `--scopes` is now a usage error, and a read-only key must say so explicitly —
  `--scopes vault_read --read-only`. Scripts that already pass a write scope are unaffected.
  The check covers **creation only**: `api-key rotate` carries the source key's scopes over
  unchanged, and keys already in the store are not revalidated, so an existing key may still
  carry a scope that grants nothing — list them with `api-key list` and rotate deliberately.
- **`gradatum-gateway`: `token_counter::estimate_total_tokens` removed.** It returned
  `estimate_input_tokens(request)` plus the requested `max_tokens`, and had no production
  caller left. **There is no drop-in replacement**; `token_counter::estimate_input_tokens`
  stays public and callers recompose the old result explicitly:
  `estimate_input_tokens(req).saturating_add(u64::from(req.max_tokens.unwrap_or(0)))`.
  Note that the gateway itself no longer sums that way — the request path now reserves a
  per-axis output floor (`max(max_tokens, reasoning reserve)`) so a reasoning block is
  counted even when `max_tokens` is absent. That reserve is crate-internal, so an external
  consumer cannot reproduce the gateway's exact cap arithmetic; only the input estimate is
  public.
- **`gradatum-vault`: `Vault::tenant_id()` renamed to `Vault::vault_id()`.** Same signature
  (`-> &VaultId`) and same returned value — the rename resolves a real ambiguity, not a
  cosmetic one. The accessor never returned the authenticated principal: it returns the
  **physical namespace** of the vault, the `<vault_id>/` directory on disk. Callers that
  wanted the namespace rename the call and are done. Callers that read `tenant_id()`
  expecting the *caller's identity* were reading the wrong value already: the principal is
  a `TenantId`, carried by the request's `TrustContext` and obtained through
  `TrustContext::tenant_id() -> Option<&TenantId>` — never from a `Vault` handle. That
  accessor changed signature in this release too; see the next entry.
- **`gradatum-core`: `TrustContext::tenant_id()` now returns `Option<&TenantId>` instead of
  `Option<&str>`.** Same typed-principal move as `Vault::vault_id()` above: the authenticated
  principal is a `TenantId` newtype rather than a bare string, so the namespace/principal
  confusion cannot be reintroduced silently. Callers needing the bare string append one call
  — `ctx.tenant_id().map(TenantId::as_str)` — byte-identical to the previous return value.
  **The JSON wire format is unchanged**: `TenantId` is `#[serde(transparent)]`, so any
  payload carrying it serializes exactly as the string did; this is a source-only break.
  The type is not re-exported by `gradatum-dto` — import it from `gradatum_core::scope`.
- **`gradatum-dto`: `tenant_id` moves from `TenantId` to `Option<TenantId>` on 23 public
  request DTOs, and the wire default `"main"` disappears from the contract.** The field
  carried `#[serde(default = "default_main")]`, so an omitted `tenant_id` deserialized as the
  `"main"` principal; it now carries `#[serde(default, skip_serializing_if = "Option::is_none")]`
  and an omission stays `None`. **Omitting the field is the nominal case**: the server derives
  the effective tenant from the credential identity (JWT / API key) in
  `api_v1::tenant_guard::effective_tenant`, and a context carrying no tenant — anything
  non-Bearer — is refused with `403`. A `tenant_id` that *is* supplied stays checked for
  consistency, unchanged: accepted when it equals the credential's tenant (a harmless echo),
  refused with `403` when it diverges. The field becomes *ignorable*, not *mandatory*: it is
  no more required than before, it has simply stopped standing in for the principal.
  **Wire consumers gain a case rather than lose one**: a key belonging to tenant `x` that
  omitted `tenant_id` used to deserialize as `"main"`, diverge from the credential and take a
  `403`; it now resolves `x`. The JSON Schema published by the MCP tool surface drops the
  `"default": "main"` annotation on every affected input accordingly.
  **The break is source-level, for Rust callers of the DTO structs**, which are not
  `#[non_exhaustive]`: literal construction and field reads both need adapting. Wrap the value
  — `tenant_id: Some(TenantId::new("x"))` — or, preferably, write `None` and let the server
  derive the principal from the credential.
- **`gradatum-dto`: `default_main()` is removed.** It existed only as the
  `#[serde(default = "default_main")]` helper behind the `tenant_id` fields above; with the
  wire default gone it has no remaining call site, and is deleted rather than kept as a typed
  no-op on a surface SemVer would freeze for the whole `1.x` line. An external DTO that used it
  must supply its own helper, or follow the same move and make its own `tenant_id` field
  `Option<TenantId>`. The companion `default_main_vault() -> VaultId`, covering the `vault_id`
  namespace axis, is untouched: it is new in `1.0.0`, still public, and still the
  `#[serde(default)]` helper behind `ArchiveEntryDto::vault_id`.
- **`VaultGrant.tenant_id` is now a `TenantId`, and the grant lookup takes `&TenantId`.**
  Three source-breaking signatures, same typed-principal move as above:
  `gradatum_core::scope::VaultGrant::tenant_id` moves from `String` to `TenantId`;
  `gradatum_core::index_store::IndexStore::tenant_grants` and
  `gradatum_index::SqliteIndex::tenant_grants` take `&TenantId` instead of `&str`.
  Callers holding a bare string wrap it — `TenantId::new(s)`, unvalidated and
  byte-identical — and callers reading the field append `.as_str()`.
  This is a **public-surface consistency pass, not a security hardening**: no production
  code reads `VaultGrant::tenant_id` at all (the tenant is matched by the
  `WHERE g.tenant_id = ?1` clause of the SQL lookup, never by comparing the field in
  Rust), so nothing is enforced that was not enforced before. Its value is that the
  principal can no longer be confused with a `VaultId` at a call site, on a surface that
  SemVer would otherwise freeze for the whole `1.x` line.
  **The wire format is unchanged and covered by a test**: `TenantId` is
  `#[serde(transparent)]`, so a serialized `VaultGrant` is byte-identical to the one
  `0.8.0` produced, and the SQLite column stays a plain `TEXT` — no migration.
  One minor narrowing comes with it: the two constructors `VaultGrant::new` and
  `VaultGrant::new_scoped` move from `impl Into<String>` to `impl Into<TenantId>`, which
  accepts `&str` and `String` but no longer `Cow<str>`, `Box<str>` or `char`. Internal
  impact is nil — `VaultGrant` is constructed at a single production site.
- **`gradatum-admin`: `VaultRenameArgs` fields `ancien` / `nouveau` renamed to
  `current_title` / `new_title`.** The struct is public on both export paths
  (`gradatum_admin::VaultRenameArgs` and `gradatum_admin::vault_rename::VaultRenameArgs`),
  so any external caller constructing it must rename the two fields. Types and semantics are
  unchanged (`String` / `String`), as are the `root` and `tenant` fields. These were the last
  French identifiers on the public API surface; they are renamed now rather than frozen by
  SemVer for the whole `1.x` line. The FR→EN entry under *Changed* covers user-facing
  **messages** — it does not imply this field rename, which is a separate source-breaking
  change.
  **The `gradatum-admin vault rename` CLI is not broken**: both arguments stay positional, in
  the same order and count, so existing operator scripts keep working unchanged. Only the
  help labels move, from `<ANCIEN> <NOUVEAU>` to `<CURRENT_TITLE> <NEW_TITLE>`.
- **`gradatum-server`: `api_v1::dto::SearchHit` gains a public `vault_id` field and becomes
  `#[non_exhaustive]`.** The attribute is the wider of the two changes: from another crate,
  `SearchHit` can no longer be built as a struct literal nor destructured exhaustively — for
  *any* field, not only the new one — so there is no source-compatible migration for a
  downstream literal. That is deliberate and matches how the type is used: `SearchHit` is a
  response type, produced by the server and read by consumers, and the attribute is what lets
  further fields land during `1.x` without another major bump. There is no public constructor
  and none is planned; a `SearchHit` is obtained by deserializing a `vault_search` response.
  Reading fields, pattern matching with a trailing `..`, `Debug` and `serde` are all
  unaffected. Wire consumers are not affected either — see the `vault_search` entry under
  *Added*; this is a source-level break, and only for Rust callers that use the type directly
  rather than through the HTTP or MCP surface.
- **`#[non_exhaustive]` added to five public enums** — `TrustContext` and `StudioScope`
  (`gradatum_core::trust`), `JobScope` (`gradatum_core::job`), `AclOp` and `AclDecision`
  (`gradatum_acl_policy`). Deliberate hardening ahead of the `1.x` API freeze: new variants
  can then be introduced within `1.x` without a major bump. It is itself a breaking change:
  downstream `match` expressions over these enums must carry a `_` arm — exhaustive matching
  no longer compiles. Constructing the existing variants is unaffected. On the authorization
  path the `_` arm must be **fail-closed** (`evaluate` denies any unwired variant). Note the
  asymmetry with `CuratorPipelineConfig` above: this pass targeted the authorization/ACL/job
  enums, plus the `SearchHit` response DTO listed separately above; it did **not** target
  configuration structs, so `CuratorPipelineConfig` enters `1.0.0` *without*
  `#[non_exhaustive]` — every field added to it during `1.x` will cost a major bump. That was
  not an oversight left open: the struct has neither a `Default` impl nor a public
  constructor, and it is built as a literal from another crate — `impl
  From<&WorkerCuratorConfig> for CuratorPipelineConfig` in `gradatum-worker` — which
  `#[non_exhaustive]` forbids. Adding the attribute would have required shipping a builder or
  a constructor — an API addition — on the eve of the freeze.
- **`gradatum-worker`: a job whose scope carries no vault is refused when
  `multi_tenant.enabled = true`.** A scope determines a vault only if it *carries* one:
  `JobScope::Vault(v)` does; `VaultWide`, `Locus`, `Notes` and `Session` do not — they say
  *what* the work covers, never *where* it lives. Those four used to fall through to `"main"`;
  with the flag on they now fail the job terminally (`HandlerError::Business`, message
  `ambiguous job vault: … the enqueue site must carry JobScope::Vault(v)`).
  **With the flag off nothing changes**: `"main"` is the only vault, so it is *the* answer and
  not an arbitrary pick, and resolution stays byte-identical to `0.8.0`. The resolved vault is
  what scopes destructive access — `delete_note` in the purge handler, `persist_forget` in the
  forget handler — so silently electing one vault out of N was a corruption path, not a
  convenience: a job could list its candidates in one vault and mutate homonyms in another.
  Three handlers consume the resolution (`handle_purge`, `handle_forget`, `handle_distill`);
  `handle_curate` and `handle_embed` do not and are unaffected.
  **Every enqueue path feeding those handlers carries `Vault(v)` — with one conditional
  exception, named here rather than glossed over.** The paths, enumerated: `POST /api/v1/jobs`
  (`api_v1::jobs_v2`), which takes the vault from the authentication context and never from the
  request body; `POST /api/v1/vault_forget` and the `vault_forget` MCP tool, which share
  `api_v1::forget::build_forget_job_record`; and `gradatum-admin vault forget`, which has its
  own builder — see the next entry. Each of those carries `Vault(v)` unconditionally.
  **The exception is the distill cron** (`gradatum-worker::schedules::build_distill_job_record`):
  its outer `JobSpec.scope` carries `Vault(tenant_id)` **only when `[multi_tenant] enabled =
  true`**, and stays `JobScope::Locus(locus)` when the flag is off. That is not a hole — with
  the flag off, `Locus` resolves to `"main"`, the only vault, so the job is scoped exactly as
  before and byte-identically to the single-vault behaviour. The distinction matters only if
  the flag is read as irrelevant to this path; it is not. That function's own doc-comment
  states the same condition, and is the authority on it.
  **The curate path still enqueues
  `JobScope::VaultWide`** (`build_curate_job_record`, reached from `vault_write`): it is not one
  of the three consumers of the resolution, and it carries its tenant in `CurateSpec.tenant_id`
  instead. Read the guarantee as scoped to the handlers that consume `resolve_job_vault`, not as
  a property of every job record in the queue.
- **`gradatum-admin vault forget`: one `Job::Forget` is enqueued per vault, and the enqueue
  line changed.** The command used to build a single record scoped `JobScope::VaultWide`; it
  now emits `JobScope::Vault(v)`, one job per targeted vault. Three consequences for
  operators, one of them affecting *every* invocation:
  **(1) The enqueue line now names the vault.** It moved from `Job::Forget enqueued : <ulid>`
  to `Job::Forget enqueued (vault <v>) : <ulid>`, still followed by
  `Poll : gradatum-admin jobs get <ulid>`. This applies to the ordinary single-vault case
  too — any script matching the old line breaks. A `vault forget agent --vaults a,b`
  invocation now prints **one such pair per vault** rather than one for the whole run;
  duplicate vaults are collapsed and input order is preserved. The other sub-commands
  (`topic`, `locus`) target a single vault and enqueue a single job.
  **(2) `vault forget locus --tenant X` with `X ≠ "main"` now fails when
  `multi_tenant.enabled = false`.** The targeting flag is `--tenant` and is shared by all
  three sub-commands; there is no `--vault` flag. **The previous behaviour was wrong, not
  merely different**: the preview listed candidates in `X` (its SQL filters on
  `notes.vault_id`) while the worker derived the mutation vault from `VaultWide` and resolved
  it to `"main"` — so the command forgot `main`'s homonyms, or nothing at all, and reported
  success either way. Note *where* the failure surfaces: the CLI still exits `0` after
  enqueuing, and the terminal error (`unsupported vault scope (mono-vault): 'X' ≠ 'main'`) is
  read on the job, with `gradatum-admin jobs get <job_ulid>`. The symmetric case is the
  point of the change: with multi-tenant mode on, `--tenant X` now genuinely operates on `X`.
  **(3) The preview is grouped by vault.** Its header gained a vault count —
  `=== vault forget preview (N eligible, M excluded, K vault(s)) ===` — and each vault gets a
  `-- vault: <v> --` block. The `[DRY-RUN]` hint is unchanged and still lists the union of
  eligible ULIDs, so the double-confirmation handshake is unchanged: the operator confirms the
  whole set once with `--confirm-ulids`, and the command splits it per vault, each job
  confirming only its own ULIDs.
- **`gradatum-worker`: `InternalClient::get_note` gains a leading `vault_id` parameter** —
  `get_note(&self, vault_id: &str, ulid: &str)`. It was the last note read left unscoped on
  the trait; `get_note_status`, `get_note_embedding`, `get_trust` and `delete_note` already
  carried a vault. An unscoped read-back resolves to `"main"` server-side
  (`resolve_read_back_reader`, `vault.unwrap_or("main")`), so a caller working in a secondary
  vault read `main`'s homonym and the note *read* was not necessarily the note *mutated*. The
  method is called on protection guards, which is what made the divergence silent. **No
  default implementation is provided, deliberately**: a defaulted method would have let
  existing implementations keep compiling while ignoring the new parameter — green tests over
  unscoped code. External implementors must add the parameter and forward it; callers pass the
  vault they are scoped on. In a mono-vault deployment `vault_id` is always `"main"`, which is
  the server's own default, so the request on the wire is byte-identical to the previous one.
- **Public API surface, measured against the published baseline.** `cargo public-api` on
  `gradatum-core`, `0.7.6` → `1.0.0`, run with `--all-features` and the three
  `--omit` filters (`blanket-impls`, `auto-trait-impls`, `auto-derived-impls`) — the exact
  invocation of `public-api/regen.sh`.
  **The item counts are deliberately not reproduced here.** They live in
  `public-api/baseline/_INDEX.tsv`, which carries the per-crate totals and is regenerated and
  committed together with the surface files themselves. Read that file.
  **That omission is a correction, not a stylistic preference.** This entry has twice carried
  counts that were correct when written and false days later. It first read
  `0 removed, 56 changed, 113 added` — still reproducible at `17294c76` (2026-07-28) — and was
  then re-measured and re-anchored to a release-head commit. Anchoring a figure to a commit was
  not enough: the head moved again, `crates/gradatum-core/src/` moved with it, and the count
  drifted a second time. The `create_feature_card` API described under *Added* is itself part
  of that drift — the same document announced a feature and, a few paragraphs above, a surface
  count that excluded it. A count that must be re-anchored at every commit is a maintenance
  burden that has already failed twice; the generated index does not have that property.
  **The removals are the whole `gradatum_core::acl` module, enumerated in full:**
  ```
  pub mod   gradatum_core::acl
  pub trait gradatum_core::acl::ACLFilter
  pub fn    ACLFilter::filter(&self, &'a [Note], &BearerId) -> Vec<&'a Note>
  pub trait gradatum_core::acl::AclPolicy
  pub fn    AclPolicy::allow_read  (&self, &Note, &BearerId) -> bool
  pub fn    AclPolicy::allow_write (&self, &Note, &BearerId) -> bool
  pub fn    AclPolicy::allow_delete(&self, &Note, &BearerId) -> bool
  ```
  **Migration — `ACLFilter`: removed, no replacement, nothing to do.** The trait had no
  implementor and no caller anywhere in the workspace, and no listing endpoint ever went
  through it, so implementing it hid nothing from anyone. There is no successor trait and
  none is needed.
  **Migration — `AclPolicy`: removed, and the extension point goes with it.** The access
  control that actually runs is `gradatum_acl_policy::AclEngine` —
  `evaluate(&TrustContext, AclOp, &str) -> AclDecision`, glob-based, deny-wins, built from a
  preset via `AclEngine::from_preset_str`. Read that as a statement of *where the check lives
  now*, not as a migration path, because it is not one: `AclEngine` is a concrete struct, not
  a trait, so an out-of-tree crate that **implemented** `AclPolicy` to supply its own policy
  has **no supported way to do so on `1.x`** — third-party rules engines are not currently
  pluggable. `AclOp` moreover carries only `Read` and `Write`: `allow_delete` has no
  equivalent at all. Whether external policy implementations come back is unresolved
  (tracked as RFC-0001 §12 Q4 / RFC-0002 at the time; both design notes are since retired —
  see `GOVERNANCE.md` § Structural change tracking). The module was removed rather than
  frozen because it was dead: its traits documented themselves as the core of access control
  while holding zero implementations, and freezing that for the whole `1.x` line was the
  worse option.
  **Scope of the measurement, and what keeps it from going stale again.** The narrative above
  covers `gradatum-core`. The other publishable crates are covered by a mechanism rather than
  by hand: `public-api/baseline/` holds one committed surface
  file per **library** crate — 26 files against 27 publishable crates, because
  `gradatum-mcp-stub` is bin-only and has no baseline: its published surface is covered by the
  rustdoc gate but by no API-surface gate. `public-api/baseline/_INDEX.tsv` carries the
  per-crate item counts and their total; read that file rather than any figure quoted here.
  The CI `public-api` job runs `./public-api/regen.sh --check` over all of them, blocking on
  push to `main` as well as on pull requests. **What that gate guards is the baseline files —
  not any figure written in this changelog.** A surface change landed without a matching
  re-baseline fails CI; a re-baseline landed while a count quoted in prose goes unupdated does
  not fail, and cannot, because the gate never reads this file. That asymmetry is exactly how
  the counts this entry used to quote went stale while CI stayed green, and it is why the entry
  now points at the generated index instead of restating it. Two blind spots are documented in
  `public-api/README.md` and are not closed: `#[doc(hidden)]` items are invisible to the tool
  (four published crates measure 1 item each and are effectively uncovered), and
  `--omit auto-derived-impls` hides the `impl From` generated by `#[from]`.
  The changed items resolve to a much smaller set of distinct symbols than their raw count
  suggests, each trait method counting twice because it is re-exported at the crate root — a
  further reason not to read a raw item count as a change count. The additions are dominated
  by the rights-model newtypes (`TenantId`, `AclCheckedVaultId`, `VaultGrant`, `GrantAccess`,
  `TenantStatus`), the `AgentId` newtype, and the `AuditScanRow` and `AnnPartitionDeficit`
  row types.
  **Removals in the other crates were re-measured against `0.7.6`, and every one of them is
  already described above**: `gradatum-curator` 3 (the `llm_review_*` fields),
  `gradatum-dto` 1 (`default_main`), `gradatum-vault` 2 (`Vault::tenant_id`, counted twice
  for the crate-root re-export), `gradatum-index` 20 — sixteen being the `SqliteIndex`
  inherent methods that mirror the trait re-scoping, the remaining four being
  `delete_temporal_entry` and `get_replaced_by`, which are not removals but gain a vault
  parameter and are present at the release head with the new signature. No removal outside
  `gradatum_core::acl` was left undocumented. Three crates could not be diffed against
  `0.7.6` at all — `gradatum-studio` was never published at that version, and the `0.7.6`
  baselines of `gradatum-engine` and `gradatum-search` are not buildable — so for those
  three, no breaking change was established, which is not the same as none existing.

- **`gradatum-markdown`: `MarkdownError::Yaml` no longer exposes the YAML backend's error
  type.** The variant now carries an opaque `gradatum_markdown::YamlError` instead of
  `serde_yml::Error`, and the `impl From<serde_yml::Error> for MarkdownError` is removed.
  Code that matches the variant (`Err(MarkdownError::Yaml(_))`) and code that reads the
  message (`Display`, `to_string()`) are **unaffected**: the message is unchanged and still
  embeds the backend diagnostic verbatim, line and column included. Only two things break —
  naming the inner type (`Yaml(e) => /* e: serde_yml::Error */`) and relying on the `From`
  conversion through `?`. Both were leaks of an implementation detail: the backing YAML
  crate has now changed three times (`serde_yaml` → `serde_yml` → `serde_norway`) and each
  change was a breaking one purely because of this exposure. `1.0.0` is the last opportunity
  to close it without a further major, which is why it is done here rather than deferred.
  `YamlError` deliberately does not forward `Error::source` to the backend error, so the
  concrete type cannot be recovered by `downcast_ref` either.
- **Minimum supported Rust version (MSRV) raised from `1.88` to `1.91`.** A toolchain older than
  `1.91` no longer builds any crate of the workspace. The bump is not a choice of style: it
  is the requirement of `opendal` `0.58`, itself the first release line whose `opendal-core`
  demands `quick-xml >= 0.41` — the threshold that closes RUSTSEC-2026-0194 and
  RUSTSEC-2026-0195 (see *Security*). Every consumer pinned to an older toolchain must
  upgrade it before upgrading to `1.0.0`; there is no feature flag that avoids this.
- **`AuthConfig.revocation_store` (server config) is now a typed value (`sqlite` | `memory`)
  instead of a plain string.** Any other value — a typo, a wrong case — is now a startup
  error. Previously, the guard that refuses to run the in-memory revocation store in
  production only rejected the exact string `"memory"`, while store selection only matched
  the exact string `"sqlite"`: a third value passed the guard *and* silently selected the
  in-memory store, which loses every token revocation on restart with a single `warn!` log
  line as the only trace. Deployments already using `"sqlite"` or `"memory"` are unaffected.
- **`gradatum-worker`'s DLQ cleanup schedule now refuses `retention_days = 0` at config parse
  time.** `0` used to set the cleanup cutoff to "now" and irreversibly purge the entire
  dead-letter queue on the next tick. The 30-day default is unaffected and still applies
  when the key is omitted.

### Deprecated

- **`[curator] llm_review_endpoint` / `llm_review_model` / `llm_review_timeout_ms`** are still
  parsed by `gradatum-worker` but have no effect. The worker logs a `WARN` at boot naming the
  keys present and the settings that actually apply (`[curator.llm]` `base_url` / `model` /
  `timeout_ms`). Remove them from your configuration.
- **`gradatum-server`: `api_v1::dto::SearchHit::trust` enters `1.0.0` already deprecated**
  (`#[deprecated(since = "0.4.8")]`). It is hardcoded to `0.5` and has never reflected the
  actual trust of a source; the real values are `scores.trust_raw` and `scores.trust_decayed`,
  emitted when the request carries `include_scores: true`. The field is **not** removed here:
  it is part of the `1.0.0` public surface, so under the `1.x` LTS promise it stays on the
  wire and in the Rust struct until `2.0.0`. Reading it emits a deprecation warning at compile
  time; serialization is unchanged, so existing wire clients keep working untouched.

### Added

- **Multi-tenant vault isolation foundation (F-63/F-18).** A `VaultGrant` substrate
  (`TenantId`/`VaultId` newtypes, migration `0030`) backs a per-vault handle registry with
  vault lifecycle management (provision, suspend, soft-delete, purge). Every read/write path
  — notes, search, ANN, archive, temporal index, cache, jobs, config — is scoped through an
  ACL-checked vault handle instead of a single implicit vault. The ANN index is partitioned
  by `(vault_id, embedder_id)` (migration `0038`); child tables carry composite foreign keys
  tying rows to `(vault_id, note_id)` (migrations `0033`, `0034`, `0039`). Job rows carry a
  tenant (migration `011`, `gradatum_jobs.tenant_id`); note that the worker's `dequeue` and
  `dequeue_by_kind` select the next pending job without a tenant clause, so the queue is
  tenant-attributed but not tenant-partitioned at dequeue time. Per-vault config
  overrides fall back to the global config when unset. Gated behind
  `multi_tenant.enabled` (default `false` — opt-in; single-vault deployments are
  byte-identical to `0.x` behavior).
- **Multi-user identity (F-45).** JWT identity issuance is governed by a configurable
  allow-list of keys, and the JWT `jti` is propagated into the audit trail for real
  per-identity attribution. Per-key **write**-scope enforcement is gated behind
  `multi_tenant.enabled` (default `false`): with multi-tenant mode off, a key's scopes are
  recorded but not checked on the request path — a key created with `--scopes vault_read` can
  still write. With multi-tenant mode on, a write requires the key to carry one of `write`,
  `admin`, `service` (`WRITE_SCOPES`, exact string match); any other scope value is
  read-only. **Read access is never governed by key scopes**, in either mode — it is
  governed by vault grants and the locus ACL. Write-scope enforcement outside multi-tenant
  mode is planned for a `1.x` release.
- **Per-note usage salience in `vault_search` (opt-in, default off).** A usage-weighted
  salience factor (reads, search hits, top-3 surfacing, recall acceptance) can be folded
  into ranking; a companion audit path detects notes that have become irrelevant and can
  auto-downgrade them, and a `distill_pressure` cron counts live, non-`processed` notes per
  locus and enqueues a `Job::Distill` for a locus once that count crosses a threshold
  (weekly by default, capped per tick). Off by default like the salience factor itself.
- **`build_sha` in `--version`** for `gradatum-server` and `gradatum-worker`, plus
  deploy-time build-SHA verification, to make "what's actually running" a checkable fact.
- **`vault_search` results carry their source vault.** Every hit gains a `vault_id` field
  naming the vault it was read from. It is always present — on the cross-vault path it is
  the request's target, not the caller's own vault — so a result states its origin instead
  of leaving the caller to infer it from the request it sent. That inference was the
  problem: it breaks the moment a single hit is quoted, cached or merged away from its
  response, and a client aggregating several vaults had no way to tell the results apart.
  `vault_id` and `path` together form the full address of a note; `path` alone is only
  unique within a vault. On the wire the change is purely additive — a JSON string on each
  item — and clients that ignore unknown fields are unaffected. The MCP surface passes it
  through unchanged: `vault_search` declares no `outputSchema` and returns no
  `structuredContent`, so there is nothing for an MCP client to validate the extra field
  against.
- **`create_feature_card`, a new MCP tool: project-map feature cards whose `F-XX` number is
  assigned by the server.** The caller no longer picks the number. The body carries the
  non-feature roles (`project`, `status`, `kind`, `release`, `version`) and must **not**
  contain a `[[feature:…]]` link — a body that does is rejected, the number being the
  server's to assign. The call is asynchronous: it returns
  `{ feature, number, job_id, note_id, poll_url }`, and the card is only confirmed written
  by polling `job_status`.
- **`job_status`, a new MCP tool: the state of an asynchronous job, by `job_id`.** It returns
  `{ status, terminal, error, conflict, result_note }` and answers the one question a caller
  of an async write has — keep polling, or conclude? **`terminal` is the field to branch on**,
  never a hardcoded set of state names: `terminal = true` means conclude, `terminal = false`
  means keep polling, and the set behind the flag can grow within `1.x` without any client
  change. The distinction is not intuitive in one case: **`Failed` is not terminal** — a retry
  is still pending — so a client that concludes on `Failed`, or on anything that is not
  `Done`, reports an outcome for a job that is still running. The response is a snapshot at
  an instant; the caller re-polls, there is no server-side wait.
- **Configuration fallbacks became observable: `gradatum_config_degraded`.** When a worker
  configuration section is absent or fails to parse, the worker falls back to that section's
  defaults and keeps running — a deliberate choice, but one that used to be silent, so a typo
  in a section name was indistinguishable from a section left out on purpose. The worker now
  publishes a gauge per section, labelled with the cause of the fallback. **The gauge is
  published even when nothing is degraded** (value `0`, `cause="none"`): a monitoring probe
  can therefore distinguish *healthy* from *not reporting*, which an alert-on-presence design
  cannot. An absent series is not a zero.
- **`gradatum-worker` applies the database migrations it depends on.** The worker created its
  own queue tables but relied on `gradatum-server` having applied migrations `006`–`011`
  first. Against a database where that had not happened, every Apalis task failed on a missing
  table, the monitor drained, and the process **exited with status 0** — a startup failure
  indistinguishable from a clean shutdown. The worker now runs the migrations itself. On a
  fresh database, whichever of the two processes loses the race exits with an error and is
  restarted against a migrated schema.
- **Opt-in `compact` response mode on the MCP read tools.** `vault_search`, `vault_read`,
  `vault_timeline` and `vault_lessons_recall` each accept a boolean `compact` request field.
  When set, the server returns `{ "compact": "<text>" }` — a plain-text rendering of the same
  result — instead of the full typed response. It exists for LLM consumers, for which the
  JSON scaffolding and the metadata blocks are pure token cost. **The default is `false` and
  the full response is unchanged**, so this is additive for every existing client. Note this
  is a *different* mechanism from `vault_context`'s `mode: "compact"` introduced in `0.7.6`:
  that one selects a *retrieval strategy* (which notes are inlined versus returned as stubs),
  this one selects a *rendering* of an unchanged result set. They do not interact.
- **The CycloneDX SBOM is attached to the GitHub Release and is byte-reproducible.** It was
  previously generated as a short-lived CI artifact that nothing consumed. Two properties are
  now guaranteed. *Scope*: only publishable crates enter the artifact, the list being derived
  from `cargo metadata`'s `publish` field rather than from a text search, and a CI gate fails
  if the number of files produced diverges from the number of publishable crates. *Determinism*:
  `SOURCE_DATE_EPOCH` is pinned to the tagged commit's date, without which the tool assigns a
  random `serialNumber` per run — a third party can regenerate the SBOM from the tag's sources
  and compare byte for byte. Component `bom-ref`s, which used to embed the absolute build-machine
  path, are normalised to a relative form uniformly, leaving the `dependencies` graph consistent.

### Changed

- **`gradatum-worker`, `gradatum-gateway` and `gradatum-admin`: public API surface cut down
  to their entry points.** These three crates are meant to be run as binaries, not imported
  as libraries, and almost everything that used to be `pub` is now hidden from the published
  API. Measured by the project's own API-surface count, across this release:
  `gradatum-worker` 324 → 1, `gradatum-gateway` 630 → 1, `gradatum-admin` 406 → 1.
  **This is a documentation and measurement change, not a visibility reduction.** The items stay
  `pub` behind `#[doc(hidden)]`: code that already imports them keeps compiling. What changes is
  that they no longer appear in the rendered rustdoc, and that `cargo public-api` — which skips
  `#[doc(hidden)]` — stops measuring them, which is why the counts collapse to 1 and why these
  crates fall into the `public-api` blind spot recorded under *Breaking changes*, in the entry
  on the surface-count scope. Running the binaries themselves is
  unaffected.
- **All remaining user-facing strings migrated FR→EN (F-102 completion).** CLI, HTTP API,
  tracing, and typed `#[error]` messages across the workspace are now English. A CI gate
  (`scripts/scan-fr-strings.sh`) guards against regressions, **within a bounded scope it states
  itself**: it matches a vocabulary, not a language, so French built entirely from words outside
  its detection core passes. Its own header records the consequence — a hit is never fixed
  alone; search the family by the fragment, not by the word that triggered the match. Read a
  green run as *no French from the known vocabulary*, not as *no French*. This covers
  **message text only** and
  is not source-breaking; the one French→English rename that does touch a public API — the
  `VaultRenameArgs` fields — is listed under *Breaking changes*.
- **Every published crate aligned to `1.0.0`** — the workspace version plus 34 inter-crate
  version constraints (22 root `[workspace.dependencies]` + 12 across 8 crates) were bumped
  together; all 27 publishable crates verified at a single version. The `gradatum-cli` crate
  is a placeholder (not yet implemented) and is **not republished at `1.0.0`**. Its `0.7.6`
  release remains on crates.io and stays installable — published crate versions are never
  removed. A real implementation is expected with the agent runtime at `2.0.0`.
- **CI toolchain pinning** consolidated to a single source of truth instead of duplicated
  per-job versions.
- **The `gradatum` facade crate now enables its `core` feature by default.** A plain
  `gradatum = "1.0.0"` (or `cargo add gradatum`) re-exports `gradatum-core` as
  `gradatum::core`; previously a bare install exposed only the `VERSION` constant, and
  `features = ["core"]` had to be requested explicitly. Opt out with
  `default-features = false`.

### Fixed

- Closed a class of cross-vault isolation gaps found during the multi-tenant hardening pass
  (ACL-checked vault resolution on read-back, cache partitioning by vault, archive/temporal
  index scoping, cross-tenant job visibility, ANN cross-vault eviction).
- **`gradatum-admin token issue` produced tokens the server rejected with `401`.** The CLI
  signed with `config/jwt.private.pem` (`kid = gradatum-admin-issued`) while the server signs
  and verifies with the `jwt-signing-key.secret` seed (`kid = gradatum-v0`). The CLI now signs
  with the server's key; token verification on the `/auth/exchange` path is unchanged.
- `cargo-cyclonedx` SBOM generation: removed an invalid `--output-cdx` flag rejected by
  `cargo-cyclonedx` 0.5.9.
- **The forget handler listed its candidates in one vault and mutated them in another.**
  `handle_forget` took the listing vault from `ForgetScope.vault` and the mutation vault from
  `JobSpec.scope`; the two could disagree, and did on every `gradatum-admin vault forget`
  invocation naming a `--tenant` other than `"main"`. `JobSpec.scope` is now the single
  source of vault truth and drives listing and mutation alike, while `ForgetScope.vault*` is
  demoted to a consistency assertion (`ensure_forget_scope_vault`): a disagreement fails the
  job terminally instead of electing one of the two. A multi-vault `ForgetScope::Agent`
  arriving on a single job is refused for the same reason — one job targets exactly one
  vault, and the enqueue site fans out. The `dispatch` path was aligned in the same pass: its
  two note reads now pass the job's own tenant, the same namespace the downstream persist
  already used. See the two `vault forget` entries under *Breaking changes* for the
  operator-visible half of this fix.
- **`recall_lessons`, reachable through the publicly re-exported `gradatum_core::IndexStore`
  trait, panicked when called with `limit > 100`.** A crates.io consumer requesting more
  than 100 results would crash the call; the bundled HTTP path caps its own requests at 20
  and never hit the bug. Also fixed: `limit = 0` used to return one result instead of zero.
  Both are now correct — a `limit` above 100 returns its full requested count, and
  `limit = 0` returns nothing.
- **The MCP `initialize` handshake now negotiates the protocol version instead of ignoring
  the client's.** The handler answered with the server's default version unconditionally: the
  version requested by the client fed peer bookkeeping and never the response, so any client
  on an earlier protocol version was rejected with no recourse. Behaviour now follows the
  specification — a supported requested version is echoed back verbatim; otherwise the server
  answers with its most recent one and leaves the client to decide whether to disconnect,
  which is what the spec prescribes for an unknown dated version rather than a JSON-RPC error.
  The fallback emits an observable `WARN` instead of staying silent. The set of servable
  versions is read from the MCP library's own list, not from a local copy. The defect lived
  **only on the HTTP path**: the stdio transport renegotiates after the handler, whereas the
  stateless Streamable HTTP transport serialises the handler's answer verbatim — a test on a
  duplex transport would have passed without the fix, so the regression test drives the real
  HTTP service.

### Security

- **The YAML backend moved from `serde_yml` to `serde_norway`, resolving two unpatched
  `unsound` advisories instead of silencing them.** `serde_yml` 0.0.12 carried
  RUSTSEC-2025-0068 (unsound emitter) and its `libyml` 0.0.5 backend carried
  RUSTSEC-2025-0067, both with `patched = []` and both repositories archived upstream — no
  bump could have fixed either. The `deny.toml` exemption that covered this was wrong twice
  over, and both errors were measured before removal: it asserted that "only the
  `Deserializer` is used", whereas the note **write** path (`gradatum-markdown`'s
  `write_parsed`) called `to_string`, i.e. the very emitter the advisory targets, on
  frontmatter partly controlled by the client; and it covered only `serde_yml`, leaving
  RUSTSEC-2025-0067 on `libyml` covered by no entry at all. Both crates have left the
  resolved graph (`cargo tree -i` reports "did not match any packages"), and the exemption is
  removed rather than reworded — both advisories disappear from the audit output because the
  crates carrying them are gone, not because they are silenced.
  `serde-yaml-ng`, named as the intended
  target in `CONTRIBUTING.md`, was rejected on measurement: its `unsafe-libyaml` backend has
  been archived since March 2024 and would have reproduced the same dead end.
  `serde_norway` 0.9.42 carries no advisory and maintains its own backend fork.
  **Note integrity is unaffected**: hashes are computed over the RFC 8785 JCS canonical form
  of the deserialized `Frontmatter`, never over the rendered YAML. Verified across the
  2 618 notes of the production vault — 0 `Frontmatter` divergence, 0 `ContentHash`
  divergence, 0 round-trip failure in either direction. The on-disk *rendering* does change
  (timestamps are no longer quoted), which is a diff, not a migration: the rendering was
  already not an invariant, 586 of those 2 618 notes still carrying the pre-`serde_yml`
  format — a format this change restores.
- **RUSTSEC-2026-0194 and RUSTSEC-2026-0195 (`quick-xml`) are closed by an upgrade, not
  exempted.** Both are denial-of-service advisories (CVSS 7.5 — quadratic attribute
  duplicate-checking, unbounded `NsReader` allocation) whose only fix threshold is
  `quick-xml >= 0.41.0`, with no backport on the `0.39`/`0.40` lines. They reached the tree
  through `opendal`, which while pinned to `0.51.0` could not satisfy that threshold at any
  version: no `opendal` below `0.58` requires `quick-xml >= 0.41`. Two load-bearing `ignore`
  entries in `deny.toml` covered them in the meantime. `opendal` is now pinned to `=0.58.1`
  — the first line whose `opendal-core` demands `quick-xml ^0.41` (since `0.56` the crate is
  split into a facade plus `opendal-core` and `opendal-service-*`, and the requirement is
  read on core, not on the facade; `0.58.0` was yanked upstream). `quick-xml` is now present
  in `Cargo.lock` at a single version, `0.41.0`, and the monolithic `reqsign` is gone from
  the lockfile entirely, replaced by the split `reqsign-core` / `reqsign-*` crates. **Both
  `ignore` entries were removed from `deny.toml` rather than reworded**: the advisories are
  absent because the vulnerable versions are absent. The cost of this upgrade is the MSRV
  bump to `1.91` listed under *Breaking changes*.
- **RUSTSEC-2023-0071 (`rsa` 0.9.10, the "Marvin" timing side-channel key recovery, CVSS 5.9)
  remains in `Cargo.lock`, deliberately un-exempted in `deny.toml`.** No patched release
  exists at any version. It is arbitrated in `.cargo/audit.toml` instead, and the distinction
  matters: `cargo audit` reads the lockfile flat, whereas `cargo deny check` resolves the
  crate graph with the features actually selected. `rsa` reaches **no** resolved graph —
  `cargo tree -i rsa@0.9.10 --target all` prints nothing, across every target and edge kind
  — because both of its paths sit behind unselected features: `sqlx` is pulled with the
  SQLite driver only and never `mysql`, and the `reqsign-*` crates arrive only through
  `opendal`'s cloud backends. Those backends are opt-in features of `gradatum-storage`
  (`s3`, `gcs`, `azure`), the default is `["fs"]`, and no crate in the workspace enables any
  of them. Adding an `ignore` entry to `deny.toml` would be worse than useless: it currently
  produces a dead entry (`advisory-not-detected`), and it would *disarm the gate for the
  future* — today, enabling `sqlx`'s `mysql` driver or any cloud backend would bring `rsa`
  into the resolved graph and fail `cargo deny check`, which is exactly the signal wanted.
  **`gradatum-storage` is a published crate**, and it is the workspace's only `opendal`
  consumer: a downstream user who turns on `s3`, `gcs` or `azure` pulls that code into their
  own build graph and inherits this advisory with it, for which no fix is available at any
  version. Enable a cloud backend with that in mind. The consequence is that the two tools
  disagree on this one entry — one finding versus zero — which is each tool behaving
  correctly, not a regression.
- **`event-listener` upgraded `5.4.1` → `5.4.2`, closing RUSTSEC-2026-0221.** Unlike the
  `quick-xml` advisories, this one bites on a path that is genuinely compiled and linked —
  the crate is reached both through `async-lock` → `moka` (the cache layer) and through
  `sqlx-core` (the database layer). It is fixed by the bump, not exempted; `deny.toml` is
  untouched. `patched = [">= 5.4.2"]` is an intra-semver patch bump, so no manifest changed.
  Side effect: `5.4.2` dropped its `concurrent-queue` dependency, which nothing else
  referenced, so it leaves the lockfile too.

### Known limitations

- **`expected_sha256` guards the update path, and only the update path.** On
  `VaultWriteRequest` the field is honoured whenever `note_id` designates a **live** note:
  before writing, the curate worker compares it against that note's stored content
  (`Vault::write_if_match`, reached in production through
  `Registry::write_if_match_internal`). A stale hash **aborts the write** — the note is left
  intact, the job terminates in `JobStatus::Conflict`, and a `WriteConflictDto` carries the
  winning `current_sha256`. Four limits bound that guarantee:
  - **Creation is not guarded.** With `note_id` omitted the write targets a freshly
    pre-allocated ULID and the field is ignored. With `note_id` set to a never-indexed ULID
    there is no current content to compare against, and the write is unconditional.
  - **The conflict is asynchronous.** `POST /api/v1/vault_write` answers `202`; a hash
    mismatch produces no synchronous `409`. A caller that does not poll `job_status` until
    `terminal = true` never observes the conflict.
  - **A missing hash is a different, synchronous rejection.** Writing over a live note with
    no `expected_sha256` is refused with `409` (intent guard), as is supplying the field on a
    ghost note (indexed, `.md` missing) whose hash cannot be verified against any content.
  - **The guard is scoped to the curate path, and is not atomic.** `write_if_match` reads,
    compares, then writes, with no storage-layer lock holding that sequence together. The
    hash is consulted by the curate handler only: it guards a note against a competing
    *curate* writer, not against every writer. No deployment shape turns it into a lock on
    the note — in particular, running a single worker process does not make it one, since
    the `curate` worker itself defaults to a concurrency of 2 and the `forget`, `distill`
    and `embed` workers run alongside it.

  `PersistForgetRequest` carries no `expected_sha256` at all, and
  `PersistDistillRequest.expected_sha256` stays inert: the distill handler never reads it.
  Both write paths are unconditional. That exposure is **directional** — one of the two
  orderings is in fact covered, and it is not the one a reader tends to assume.
  `VaultWriteRequest::expected_sha256` documents which is which; being the field the MCP
  tool schema renders, it is the copy a calling agent actually reads.
- **Repeated `vault forget` is idempotent.** Before mutating a note the forget handler re-reads
  it and skips the ones already forgotten, preserving the original `forgotten_at` and
  `forgotten_by` instead of overwriting them with the second run's values. The skip still
  re-synchronises the index, so a note whose file and index disagree converges rather than
  staying stale.

## [0.8.0] — 2026-07-14 · internal milestone, published in 1.0.0

Train "stability memory vault". On-demand delete is now **archival** (reversible), gated
behind an operator-only surface; all agent-facing mutations of the archive lifecycle stay
off the public API and MCP. User-facing strings are now English.

### Added

- **On-demand delete = archival (F-100).** A delete moves the note's `.md` + `.history/`
  under `.archive/` (mirror layout) and records a row in the registry-driven `archive_index`
  table, instead of destroying data. Recoverable for the retention window; a **durable JSONL
  audit tombstone** is written before the cascade (hard precondition: no irreversible cascade
  without a durable recovery trace).
- **Retention GC, registry-driven (F-100).** Archives past their 60-day (configurable)
  retention deadline are physically destroyed by a boot/interval GC selecting from the
  registry — never a filesystem scan. Destroyed/restored rows survive as history traces.
- **Restore to quarantine (F-100).** Restoring an archive re-indexes the note as
  `pending-review` (re-enters the curator pipeline, not live) with a 409 on ULID collision;
  promotion back to live goes through the curator, never automatically.
- **Operator CLI `gradatum-admin` archive lifecycle (F-100).** `delete`, `archives list`,
  `archives purge`, and `archives restore` (single ULID or `--from/--to [--section]` range),
  all dry-run-by-default with server-side confirmation. Reaches the internal loopback admin
  namespace (`127.0.0.1`, dedicated admin token distinct from the worker token) — never a
  public route.
- **MCP `vault_archives_list` (read-only, F-100).** Agents can *see* archives (to prepare
  operator CLI commands) but can never delete, restore, or purge via MCP or the public API.
- **`archive_index.vault_id` (F-100).** The archive registry now carries the owning vault
  (mirror of `notes.vault_id`) plus an optional vault filter on listing — anticipating the
  multi-vault work of the 1.x line.
- **Audit / dedup job (F-51), default OFF.** Opt-in background job (Option A) — no behavior
  change unless explicitly enabled.

### Changed

- **User-facing strings migrated to English (F-102).** Operator/CLI/API-facing messages are
  now English for the public release surface.
- **Curator instrumentation (F-66).** Curator outcomes are now instrumented per
  section × outcome, improving observability of the classification path.
- **Code-map resolution qualified (F-70).** Already shipped LIVE in 0.7.9; recorded here for
  completeness in the public train.

## [0.7.7] — 2026-07-06 · internal milestone, published in 1.0.0

### Added

- **`gradatum-engine`: `--backend-sampling` accepted in the `extra_args` allow-list.**
  Permits the llama.cpp b9780+ GPU-side token sampling flag, removing a CPU sampling
  bottleneck (host-visible write-combined logit copy) — up to ~3.6× decode throughput
  on large-vocabulary models. Operators must ensure the target `llama-server` is b9780+
  for the flag to take effect (older binaries reject the argument and the child fails to
  start — fail-closed).

## [0.7.6] — 2026-07-03

Upgrade from v0.6.4: one breaking server API change — `vault_context` is redesigned; its
response schema changes from `{ context, estimated_tokens, sources }` to `{ assembled_text,
included, budget_used, diagnostics, … }` and the default output is now assembled Markdown
(`mode: "raw"` retains the v0.6.4 dump text, inside the new schema). All other endpoints are
drop-in: new request fields are optional and omitting them preserves prior behavior. Operators
running `gradatum-engine` should review the breaking changes below before upgrading.

### Breaking changes (API)

- **`vault_context`: response schema replaced and default output changed.**
  - **Old response (v0.6.4)**: `{ context, estimated_tokens, sources }`.
  - **New response (v0.7.6)**: `{ assembled_text, included, budget_used, diagnostics,
    references, counts, cache_breakpoint_hint }`. `references` and `counts` are always
    present (`[]` / zeroed when `reference_mode` is off).
  - **Default output changed**: the request `mode` field defaults to `"assembled"` —
    `assembled_text` is a structured Markdown context block, not the raw FTS dump.
  - **Migration**: clients parsing the old shape must read `assembled_text` (and
    `included` instead of `sources`). To keep the v0.6.4 dump text itself, send
    `mode: "raw"` — `assembled_text` then reproduces the old `context` value
    byte-for-byte, inside the new schema.

### Breaking changes (operator)

- **`gradatum-engine`: `extra_args` is now validated against an allow-list**
  (`ALLOWED_EXTRA_FLAGS`). Flags that are managed by dedicated configuration fields are
  rejected — in particular `--n-gpu-layers` (and its aliases), which is controlled
  exclusively by the `gpu_layers` config field. An existing configuration such as
  `extra_args = ["--n-gpu-layers", "0"]` now fails at boot with
  `EngineError::BadRequest` from the `LlamaServerSupervisor`; migrate it to
  `gpu_layers = 0`.
- **`gradatum-engine`: new loopback-only `/metrics` listener on `port + 1` by default**,
  configurable via `metrics_port`. The listener always binds `127.0.0.1` regardless of
  `bind_addr`. When running multiple engine instances on contiguous ports (e.g. 11435
  and 11436), the `port + 1` default can collide with the neighbouring instance's main
  port — set `metrics_port` explicitly in that case.

### Added

#### Context assembly pipeline (`vault_context`)

`vault_context` is redesigned from a raw FTS dump into a full retrieval and assembly pipeline.

- **Retrieval**: Reciprocal Rank Fusion over BM25 (FTS5) and semantic embedding signals
  (`k=60`, configurable candidate cap).
- **Composite scoring**: `recency × PageRank × trust` applied after RRF fusion; reuses the
  `gradatum-search` scoring infrastructure.
- **Budget-aware selection**: notes are sorted by score and inlined until the `budget_tokens`
  limit; bodies are fetched lazily (only for retained notes).
- **Structured Markdown output**: per-note heading (`### title · section · date · score=X`),
  `---` separators, `[[ULID]]` source references.
- **Skill injection** (opt-in): when `inject_skills: true` and `skill_query` are set, top
  matching notes from the `skills/` section are appended to the assembled context (index-only
  lookup, no LLM call). Governed by `max_skills` and `skills_budget_fraction`.
- **New request fields** (all optional, fully backward-compatible): `budget_tokens`,
  `scoring` (`ScoringWeights`), `mode` (`Assembled` | `Raw`), `inject_skills`, `skill_query`.
- **New response fields**: `assembled_text`, `included` (`Vec<IncludedNote>` — `ulid`, `title`,
  `section`, `date`, `score`), `budget_used`, `diagnostics` (`candidates_considered`,
  `included_count`, `embed_fallback`, `skills_injected`).
- `mode=Raw` preserves the prior FTS-dump byte-for-byte (backward-compatibility fallback).
- **`ContextConfig`** TOML block (`[context]`): `default_budget_tokens`, `top_n_candidates`,
  `max_skills`, `skills_budget_fraction`, `embed_timeout_ms`.

#### Context efficiency — reference mode and session window

- **Reference mode** (`reference_mode: bool`, default `false`): notes are inlined up to the
  token budget (`budget_tokens` per request, `default_budget_tokens` in config); those beyond
  are returned as lightweight stubs `{ ulid, title, section, snippet }` up to the
  `stub_budget_tokens` config limit; stubs are dereferenceable via `vault_read(ulid)`.
- **Session window** (`session_id`): notes already sent inline in the current session are
  returned as stubs on repeat calls, never re-inlined. `mode=compact` re-ranks the freshest
  top-K notes inline and returns all prior-sent notes as stubs — useful for context compaction
  at session boundaries. Folded notes remain dereferenceable.
- **`cache_breakpoint_hint`**: boolean hint emitted when assembled context exceeds a configured
  threshold, signalling the consumer to insert a prompt-cache boundary.
- **New response field `references`**: `Vec<ReferenceStub>` (additive, default `[]`).

#### Proactive recall

- **Background refresh scheduler**: a `tokio::interval` task enqueues a `ProactiveRefresh`
  job every 900 s (configurable via `[proactive_recall] refresh_interval_secs`, floor 60 s).
  The job derives an implicit salience query from the K most recently written notes, runs
  cross-section retrieval over `lessons-learned`, `reasoning`, and `decisions`, applies
  composite scoring, and stores the top-N surface (default 8) in `proactive_surface`.
- **`POST /api/v1/proactive_recall`** — pull endpoint with two modes:
  - `proactive` (no `context` field) — reads the pre-computed surface (cheap path).
  - `contextual` (with `context` field) — on-demand RRF over the same sections.
  - Response: `{ recall_id, mode, items: [{ulid, title, section, snippet, score}] }`.
- **`POST /api/v1/proactive_recall/feedback`** — acceptance feedback; records which surfaced
  notes were used (`accepted_ulids ⊆ surfaced_ulids`, 400 otherwise); idempotent.
- **MCP tools**: `vault_proactive_recall`, `vault_proactive_recall_feedback` (bring MCP
  surface to **23 tools**).
- **Lessons recall enrichment**: `/api/v1/lessons/recall` gains two optional parameters —
  `rank` (`relevance` | `recency-boosted`) and `semantic` (`false` | `true`; degrades
  gracefully to BM25 when the embedding service is unavailable).

#### Agent identity via MCP

- **`identity` section** (13th canonical section): migration 0024 creates the section;
  migration 0025 backfills `title` for existing identity notes from their first H1.
- **Soul validator** (`validate_soul()`): checks structural sections (INVARIANTS / GATES /
  NARRATIVE); handles `extends:` resolution (bounded depth); accepts `scope` field.
- **MCP `initialize` identity injection**: on MCP `initialize`, the server injects the
  tenant's agent identity note from the `identity` section into the MCP `instructions` field.
  Access to `identity` from `vault_search` is fail-closed for non-privileged callers.
- **Write ACL for `identity`**: only the bearer whose `agent_id` matches the JWT `sub` may
  write their own identity note; `doc_kind` is forced to `Static`.
- **`write_check` drift detection**: `write_check::check_category_section()` detects
  category↔section drift on note ingestion (warn-only). Metric
  `gradatum_write_check_total{rule}` incremented on each detected drift.
- **Worker guard**: reclassification of an `identity` note is a no-op.

#### Temporal search and decay

- **`vault_search` temporal range filter**: new optional request fields `from_ms` and `to_ms`
  (epoch milliseconds). Applied on both the FTS and semantic paths via `LEFT JOIN
  temporal_index`. `anchor_ms` is now included in every `SearchHit`.
- **`vault_write` `occurred_at` field**: optional string (ISO 8601). Validated at write time;
  propagated through the curation pipeline to populate `anchor_ms` in `temporal_index`.
- **Recency factor uses `anchor_ms`**: the exponential-decay recency signal in composite
  scoring now uses the canonical `anchor_ms` from `temporal_index` (fallback: `created_at`
  when no temporal anchor is set). Applied consistently across `vault_search` and
  `vault_context`.

#### Review auto-promotion job

- **`review-promote` scheduled job**: notes left in `staging` or `pending-review` are
  automatically promoted to `live` after 14 days (`age_days`), on an hourly tick
  (`interval_secs`, floor 60 s) capped at 200 notes per tick (`max_per_tick`).
  **Enabled by default** — set `[review_promote] enabled = false` to opt out.

#### Scheduled task health observability

- **Migration 0026**: two new tables in `index.db`:
  - `scheduled_task_health` — one row per task: `task_name` (PK), `last_run_ms`,
    `last_outcome` (`ok`/`error`), `last_duration_ms`, `last_error`, `run_count`, `updated_at`.
  - `scheduled_task_error` — append-only errors table; indexed on `(task_name, occurred_ms)`;
    lazy 7-day purge on each error insert.
- **`record_task_run` helper** (`gradatum-index`): upserts `scheduled_task_health`, appends to
  `scheduled_task_error` on error; never panics.
- **Boot seeding**: all 8 scheduled task names are seeded with `last_run_ms: null` at startup
  so the System page shows all tasks immediately, before the first tick fires.
- **All recurring tasks instrumented**: each task body captures duration and outcome and calls
  `record_task_run`. Task behavior is unchanged — instrumentation is purely additive.
- **`GET /api/v1/system/scheduled`** (JWT auth): returns all registered tasks with
  `{ name, last_run_ms, last_outcome, last_duration_ms, last_error, run_count, errors_24h,
  interval_secs }`. `errors_24h` is a `COUNT` over `scheduled_task_error` in the last
  86 400 000 ms. `last_error` is sanitized before emission.
- **Studio System page**: new nav item; renders all tasks with per-task badges
  (ok / error / overdue), `errors_24h` highlighted in red when > 0, last run as relative
  time, duration, and last error message.
- **Studio Dashboard scheduler widget**: compact summary (task count, how many in error or
  overdue) linking to the System page.

#### Curated metrics timeseries

- **Migration 0027**: table `metric_sample` in `index.db`:
  ```sql
  metric_sample (series TEXT NOT NULL, ts_ms INTEGER NOT NULL, value REAL NOT NULL,
                 PRIMARY KEY (series, ts_ms)) WITHOUT ROWID;
  CREATE INDEX idx_metric_sample_ts ON metric_sample(ts_ms);
  ```
- **Curated collection**: `collect_curated_samples()` re-encodes the Prometheus registry and
  parses a static allowlist of ~60 series in 4 groups (read-path usage, context efficiency,
  server health, write pipeline). Counters → direct value; histograms → two separate series
  (`_sum` / `_count`); high-cardinality `http.*` labels are aggregated.
- **`metric-sample` scheduled task**: runs every 60 s; collects curated samples, batch-inserts
  into `metric_sample`, and runs a lazy purge of rows older than 14 days. Errors are logged
  at `warn` and do not interrupt the task.
- **`GET /api/v1/system/metrics/catalog`**: returns the full static curated series list
  `{ series: [{ key, group, kind, unit, instrumented }] }`. No database query.
- **`GET /api/v1/system/metrics/timeseries`**: query parameters — `series` (comma-separated,
  validated against the allowlist; 400 if unknown, > `MAX_SERIES=32`, or duplicated),
  `from_ms` / `to_ms` (inclusive bounds; 400 if `from >= to`), `max_points` (default 500,
  cap 2000). Server-side downsampling via `GROUP BY (ts_ms / bucket_ms)` when the span
  exceeds `max_points` raw points. Response: `{ from_ms, to_ms, bucket_secs, series: [{ key,
  points: [{ ts_ms, value }] }] }`.
- **Studio metrics charts**: `SystemPage` gains a metrics section below the task health grid.
  Range selector (1 h / 24 h / 7 d / 14 d, default 24 h), auto-refresh toggle (default on,
  60 s), and 4 collapsible groups of interactive uPlot charts. Series not yet instrumented
  are rendered grayed out rather than hidden.

#### Activity and notes browsing in Studio

- **`GET /api/v1/system/traces`** (JWT auth, read scope): paginated, filterable read of the
  `session_trace` table. Filters: `agent_id`, `session_id`, `action_type`, date range.
- **Studio Activity page**: table view of session trace records with expandable detail rows
  and auto-refresh. Accessible from the main navigation.
- **`GET /api/v1/notes/by-status`** (JWT auth, read scope): paginated listing of notes
  grouped by status (live, downgraded, pending-review, etc.) using keyset pagination.
- **Studio Notes page**: lists notes by status bucket, including archived (downgraded) notes;
  links to per-note detail.

#### Distill validation gate

Synthesized notes now pass through a deterministic scoring gate before being stored.

- **`Job::Validate` + `ValidateSpec`** (`gradatum-core`): new job variant carrying the
  synthesis note id, body, source texts, source trusts, and base trust. Bincode positional
  encoding preserved (new variant at position 8; all prior positions stable).
- **`quality_score` scorer** (`gradatum-worker/src/quality_score.rs`): pure, deterministic,
  zero I/O. Composite formula: `grounding × recency_sources × trust_sources × num_penalty ×
  entity_penalty`, clamped to `[0.0, 1.0]`:
  - `grounding` — cosine similarity between the synthesis embedding and the mean centroid of
    source embeddings.
  - `recency_sources` — exponential-decay weight on source `anchor_ms` values.
  - `trust_sources` — mean trust across sources.
  - `num_penalty` — numeric-coherence penalty: each number in the synthesis traceable to no
    source subtracts 0.15 (floor 0.5).
  - `entity_penalty` — orphan-entity penalty: each uppercase-initial token in the synthesis
    absent from all sources subtracts 0.10 (floor 0.5).
- **`handle_validate` worker**: disposition after scoring:
  - `score ≥ 0.75` → stored with `base_trust`; no extra tag.
  - `score < 0.75` → stored with `trust = base_trust × score`, `quality-low` tag appended.
    Sources are marked `processed` + `derived-into` (per-source failures are non-fatal).
  - Scoring errors fall back to score 1.0 (pass) — no synthesis is ever discarded due to an
    embedder failure.
- **`handle_distill` refactored**: enqueues `Job::Validate` instead of persisting directly;
  persist and source-marking are delegated to `handle_validate`.
- **`PersistDistillRequest.tags`**: tags supplied in a distill request are now propagated into
  the persisted note's frontmatter (previously dropped).
- **Validate worker**: `max_retries = 2` (persist is idempotent on the pre-allocated
  `note_id`; retries prevent permanent note loss on transient I/O failures).
  `ensure_main_tenant` cross-tenant guard applied at entry.

### Fixed

- **Phantom-write guard**: `vault_write` now returns `409 Conflict` for notes whose Markdown
  file is absent from storage (phantom notes). The response body distinguishes a phantom
  conflict from a `expected_sha256` mismatch.
- **`vault_read` for phantom notes**: returns `404 Not Found` (previously `500 Internal
  Server Error`) when a note's body file is missing from storage.
- **`vault_read` status**: returns the status stored in the index (authoritative) rather than
  the status extracted from a potentially stale Markdown frontmatter.
- **Temporal anchor preserved on note update**: the `vault_write` RMW path and the worker
  reclassify path now preserve an existing `anchor_ms` when updating a note that already has
  one. An anchor is overwritten only when the note body genuinely changes.
- **`vault_context` recency aligned with `vault_search`**: the context assembly pipeline now
  uses `anchor_ms` as the recency reference, in parity with `vault_search`. Previously
  `vault_context` used `created_at`, causing inconsistent recency ranking between the two
  surfaces.

### Security

- **Cross-agent identity isolation**: notes in the protected `identity` section are excluded
  from all search, list, timeline, by-status, review, trace, graph, and link surfaces for
  non-privileged callers, across the full HTTP API and MCP tool surface. Read and write are
  guarded per-handler: an agent may only read or write its own identity note (matched by JWT
  `sub`), and `vault_search` over `identity` is fail-closed for non-privileged callers
  (empty result set rather than an error, to avoid existence-oracle attacks). See
  `SECURITY.md` ("Agent identity security") for the full model and its limitations.
- **PII scrub**: maintainer-specific home-directory paths and personal identifiers removed
  from test fixtures and public source.

### Tests

3038 passed / 0 failed (`cargo nextest --workspace --release`); `clippy --workspace
--all-targets -- -D warnings` clean.

## [0.6.9] — 2026-06-26 · internal milestone, published in 0.7.6

Consolidates gateway and engine fixes shipped since 0.6.8 — no breaking API or
configuration changes.

### Fixed

- **Gateway keep-alive SSE on `/v1/messages`** — ping frames are now emitted across
  the entire prefill window (not only during headers), preventing upstream proxies
  and supervisors from closing the connection during long cold-start prefills on the
  local vision engine.
- **GBNF-bloat sanitisation in `translate_tools`** — `sanitize_schema()` now strips
  GBNF-incompatible JSON Schema constraints (`maximum`, `maxLength`, `pattern`,
  `minItems`, …) recursively at every nesting level, including inside
  `additionalProperties` and `prefixItems`. This allows the full Claude Code
  tool-set (~50 tools, 832 GBNF rules) to be forwarded to the b9780 engine without
  parser saturation, while preserving deterministic schema output for prompt-cache
  reuse.
- **`tool_choice=auto` enforcement on `/v1/messages`** — when the client sends a
  `tools` array without an explicit `tool_choice`, the gateway now injects
  `tool_choice: {type: "auto"}` before forwarding to the engine, preventing
  unexpected forced-tool behaviour.

## [0.6.8] — 2026-06-24 · internal milestone, published in 0.7.6

Consolidates versions 0.6.5–0.6.8 (not previously tagged publicly). Drop-in upgrade
from 0.6.4 — no breaking API or configuration changes.

### Added

- **Anthropic Messages API gateway** (`POST /v1/messages`) — the gateway now speaks
  the Anthropic protocol in addition to the OpenAI-compatible surface, enabling a
  fully local Claude Code experience backed by a local vision model. Includes
  `count_tokens` support and Anthropic-shaped JSON error envelopes.
- **Prompt-cache LCP enablement for the vision engine** — the engine supervisor
  allow-list accepts `--kv-unified`, unlocking llama.cpp's unified-KV prompt cache
  (b9780+). Multi-turn requests reuse the prior turn's KV via longest-common-prefix
  matching, collapsing per-turn prefill from O(full context) to O(new tokens).
- **Project-map integration**: feature backlog cards and roadmap data are now accessible
  on the project map.

### Fixed

- `gradatum-engine` `is_binary_allowed` accepts versioned `llama-server-<ver>`
  wrappers via a bounded suffix check (alphanumeric-only after a single dash),
  while the path-prefix allow-list remains the primary guard.
- Streaming `message_delta` now reports a non-zero `output_tokens` estimate.
- Engine config boot validation for `[messages]` aliases (only when configured).

## [0.6.4] — 2026-06-20

This release catches the public version number up with the real deployed version,
ending a historical gap where internal releases were not tagged publicly. v0.6.4
is a drop-in upgrade from v0.5.2 — no breaking API or configuration changes.

### Added

#### Native MCP server (`/mcp` — StreamableHTTP)

Gradatum now ships a first-party MCP server endpoint at `POST /mcp`, implemented
via `rmcp` (StreamableHTTP transport). It exposes **21 tools** covering the full
vault API surface (`vault_search`, `vault_write`, `vault_read`, `vault_timeline`,
`code_scope`, `vault_lessons_recall`, `vault_classify`, and more). `vault_classify`
returns a heuristic section classification for an existing note (offline, no LLM
inference) — a fast preview of where the curator would route the note.

Authentication is enforced on both `list_tools` and `call_tool`: any request
without a valid `Authorization: Bearer <api-key>` is rejected before tool
dispatch. The MCP schema for tools that take no parameters emits the
MCP-conformant `{"type":"object","properties":{}}` shape rather than an empty
object, preventing client-side schema validation errors.

The previous stdio MCP stub remains available for setups that require it; the
native endpoint is independent.

#### Queue DAG — job dependency chains (`await_jobs`)

The job queue now supports dependency chains: a job can declare a list of
predecessor job ULIDs in the `await_jobs` field. The worker will not promote a
waiting job to `Pending` until all its predecessors have reached `Done`. Two new
`QueueStore` methods underpin this:

- `find_awaiting` — scans for `Waiting` jobs whose dependencies are fully
  resolved, using `LIKE`-based matching to avoid collisions with partial ULID
  prefixes.
- `set_pending` — idempotent promotion from `Waiting` to `Pending`; re-running
  on an already-`Pending` job is a no-op.

Cascade promotion runs automatically in the worker's `complete` path (best-effort;
failures are logged and do not roll back the completed job). A recovery sweep
runs on each worker cycle to catch any `Waiting` jobs whose predecessors completed
before the cascade was in place.

#### Code index — multi-language support and reverse-dependency queries

**Multi-language parsing** (`LanguageParser` trait): the code index now supports
Bash, TypeScript, TSX (React JSX), and Python in addition to Rust. Each language
is backed by a dedicated tree-sitter grammar; the dispatch layer selects the
correct parser by file extension at ingest time. The `gradatum-admin code ingest`
and `code update` commands index all supported languages in a single pass.

**Reverse-dependency queries** (`include_callers` in `code_scope`): the
`POST /api/v1/code_scope` request now accepts an opt-in `include_callers: bool`
field (default `false`). When enabled, the response includes a `callers` list
— symbols in the index that call or reference the queried symbol. This field is
additive and fully backward-compatible: existing callers that omit it see no
change in response shape.

**Known limitation**: reverse-dependency detection for method calls of the form
`self.method()` has partial coverage (the callee is recorded as a terminal name
rather than a qualified `Type::method` form). Free-function and associated-function
calls are resolved correctly. This limitation is documented in-tree and will be
addressed in a future release.

#### project-map — 12th canonical section

A new `project-map` section provides a structured, graph-backed way to track work
units as vault notes. Each project-map note carries a mandatory typed-wikilink
schema: `[[project:…]]`, `[[status:…]]`, and `[[kind:…]]` are required;
`[[version:project/x.y.z]]` is optional. A write-time validator enforces the
schema when `section_hint="project-map"` is provided, rejecting notes that fail
cardinality or charset constraints.

The wikilink resolver routes typed links to synthetic graph nodes (not to
note ULIDs), keeping the project graph structurally separate from the memory
vault. A pull-based admin command, `gradatum-admin project-map render <project>`,
generates a `TODO.md` view from the work-status graph without using semantic
search.

A backfill command (`gradatum-admin backfill-changelog`) parses `CHANGELOG.md`
entries and writes them as project-map cards, enabling the graph to represent
historical releases.

### Changed

- **MCP schema SSOT**: the helper that builds `inputSchema` objects for MCP tool
  definitions is now a single source of truth in `gradatum-dto`, shared across the
  native server and any future transports. Previously four copies existed; a silent
  divergence in one of them was the root cause of client-side schema rejection
  bugs.
- **Engine path allowlist**: the local inference engine supervisor now accepts
  versioned install paths (e.g. `/opt/llama-server-0.0.0`) via a prefix allowlist
  (`/opt/llama-*`), in addition to the unversioned canonical path.
- **`Section::from_canonical_str` SSOT**: all section-name parsing is now routed
  through a single function, eliminating hardcoded string lists that had to be
  kept in sync manually.
- **Version number alignment**: the public release number now matches the
  internally deployed version. Earlier releases in the `v0.6.x` series were
  deployed internally but not tagged publicly; this release closes that gap.
  No API or behavior changes are implied by the version jump from `v0.5.2`.

### Fixed

- **Graceful shutdown race**: signal handlers (SIGTERM / SIGINT) are now installed
  before the server binds to its port. Previously, a signal delivered in the
  narrow window between handler registration and bind could leave the server
  unresponsive to shutdown requests.
- **`/health` `oldest_age_secs` always zero**: the health endpoint was reading
  from an empty table (`jobs_v2`) instead of the real queue (`gradatum_jobs`),
  reporting `oldest_age_secs: 0` unconditionally. It now reads from the correct
  table and filters to `Pending` jobs only, avoiding false `degraded` signals
  from other statuses.
- **`/health` `build_sha` unknown**: the deployed server reported
  `build_sha: "unknown"` because the value was not injected at compile time.
  A `build.rs` script now captures the Git commit hash and embeds it via
  `VERGEN_GIT_SHA`; the field is populated on every build made from a Git
  repository.
- **`server_smoke` readiness check flakiness**: the startup readiness poll no
  longer uses a fixed sleep; it polls `GET /health` until it receives a `200`
  response (up to a bounded timeout), making the smoke check reliable regardless
  of machine load.
- **`note_links` edges never written**: a format mismatch between the wikilink
  writer (`[[section:ULID]]`) and the resolver (expecting only the ULID component)
  caused all wikilink edges to be silently dropped. New notes also produced zero
  edges. An internal `/internal/v1/id-lookup` endpoint now enables the resolver
  to accept the full typed-wikilink format. Graph queries (`vault_graph`,
  `vault_trace`) return correct results on all notes written after this fix;
  historical notes can be backfilled with `gradatum-admin backfill-note-links`.
- **`Section` parse inconsistency**: `project-map` notes written with
  `section_hint="project-map"` were silently reclassified to other sections
  because the curator's section list and the persistence layer's section list were
  separate hardcoded arrays that had diverged. Both sites now delegate to
  `Section::ALL` via `Section::from_canonical_str`.

### Security

- **`list_tools` authentication gate**: the MCP `list_tools` handler was
  unauthenticated, leaking the full tool catalogue to any network client that
  could reach the server port. It now requires a valid Bearer api-key, consistent
  with `call_tool`.
- **Body limit on `/mcp`** (anti-DoS): `POST /mcp` is now wrapped with a
  512 KiB `RequestBodyLimitLayer`. Requests exceeding this limit receive `413
  Payload Too Large` before the body is read. The `DefaultBodyLimit` middleware
  that applies to other routes does not cover `rmcp`-handled routes due to a
  tower service composition constraint; this layer closes that gap.
- **Three unauthenticated write endpoints closed**: `vault_downgrade`,
  `patch_note`, and `move_note_locus` were missing authentication and ACL checks
  on the loopback interface. Each now routes through an `_impl` handler that
  enforces ACL and tenant isolation, consistent with all other write endpoints.

### Tests

2337 passed / 0 failed / 10 skipped (`cargo nextest --workspace --release`); `clippy --workspace --all-targets -- -D warnings` clean.

## [0.5.2] — 2026-06-15

First public release since v0.4.3. No breaking changes; drop-in upgrade. Adds a static code index, a timeline API, action tracing, a proof-of-absence search signal, native TLS termination, and a suite of correctness and security fixes.

### Added

#### Code index (`gradatum-admin code ingest` / `gradatum-admin code update`)

A derived index of source code symbols, separate from the memory vault. Zero LLM cost — all ingestion is static analysis via tree-sitter.

- **`gradatum-admin code ingest`**: initial full ingest from a Git repository root; idempotent (repeated runs produce no duplicates).
- **`gradatum-admin code update`**: O(diff) incremental update driven by `git diff`; only changed files are re-ingested.
- **Drift detection**: the index tracks a per-file content hash. A stale entry is flagged before results are served so consumers always see fresh data or an explicit stale signal.
- **Ingest visibility** (`--visibility pub|all`): index public symbols only (default, unchanged) or all symbols including private.

#### `POST /api/v1/code_scope` — code search endpoint

Query the code index by vault identifier plus an optional symbol filter. Returns `DerivedSymbol` records (functions, structs, enums, traits, impls) with span and SHA-256 content hash.

- **`include_body`** / **`body_budget_tokens`** fields: retrieve the exact source span of a matching symbol on demand; path anti-traversal guard enforced unconditionally.
- **MCP tool `code_scope`**: thin proxy over the endpoint; schema auto-derived via schemars.

#### `POST /api/v1/vault_timeline` — chronological note listing

Paginated timeline of notes ordered by temporal anchor, with cursor-based pagination.

- **`as_of_ms`** / **`include_expired`** fields: query the vault as of a past point in time, or include notes whose `valid_until` has elapsed.
- **`valid_until` extraction**: the server extracts this field from note frontmatter and populates an internal temporal index used for as-of filtering.
- Protected sections excluded from all timeline results (0/49 leaks confirmed).
- **MCP tool `vault_timeline`**: thin proxy.

#### `POST /api/v1/session-log/trace` — agent action tracing

Fire-and-forget endpoint for recording agent actions. Append-only; no update or delete surface. `agent_id` is the JWT `sub` (server-assigned stable identifier, not free-form). Fields: `session_id`, `tenant_id`, `ts_ms`, `action_type`, `target`, `intent`, `outcome`, `marker`, `ref`. Retention: 90 days by default (configurable via `[session_trace] retention_days` in `gradatum.toml`).

#### `include_corpus_count` — proof-of-absence signal in `vault_search`

New optional request field (default `false`). When enabled, the response includes `corpus_match_count: Option<u64>` (full-corpus BM25/FTS5 match count, unbounded by the result limit K) and `corpus_count_capped: bool`. Distinguishes a genuine absence from a retrieval miss — useful in RAG pipelines where "nothing returned" is ambiguous.

- BM25/FTS5-only, unconditional: ANN semantic-only hits are not counted (invariant: `corpus_match_count >= count(results where !is_semantic_only)`).
- Opt-in count query (~2–5 ms); response is byte-for-byte identical when `include_corpus_count` is `false`.

#### Native TLS termination (`[server.tls]`)

The server can now terminate TLS directly, without a reverse proxy, via a new optional config block:

```toml
[server.tls]
cert_path = "/path/to/cert.pem"
key_path  = "/path/to/key.pem"
```

Backed by rustls 0.23 (TLS 1.2+/1.3 only) via `axum-server` `bind_rustls`. Boot is fail-closed: the certificate and key are loaded before the server binds; any load failure aborts startup rather than falling back to cleartext.

**Enforcement**: a non-loopback `bind` address without `[server.tls]` is refused at startup (fail-closed). The default deployment (`127.0.0.1:19090`, no `[server.tls]` block) is unchanged — loopback behind a reverse proxy requires no configuration change.

#### `vault_write` in-place update

`vault_write` now honors the `note_id` + `expected_sha256` fields for in-place updates:

- `note_id` present → update in-place; absent → fresh note; invalid → `400`.
- `expected_sha256` absent on an existing note → `409 Conflict` (prevents silent overwrite).
- `400`/`409` rejections are recorded in the audit trail.

### Changed

- **Studio session persistence**: the Studio now persists the session JWT in `localStorage` (key `gradatum_studio_jwt_persist`, 24h TTL) with a client-side expiry check at mount. No more api-key re-entry after reload. The `ak_` api-key itself is never persisted.
- **Job endpoints hardened** (`/api/v1/jobs`): all job routes now require a bearer JWT with ACL; the legacy `GET /api/v1/jobs/{id}` route is secured. `POST /api/v1/jobs` deserializes the real `JobKind`.
- **Gateway metrics cardinality**: `route` and `provider` Prometheus labels are bounded by an allowlist (unknown values map to `"other"`), preventing unbounded label growth from malformed or unexpected requests.
- **`vault_read` now returns `title`**: the `title` field is populated in `VaultReadResponse`, making read-modify-write (RMW) workflows reliable without a separate lookup.

### Fixed

- **Optimistic-lock `Conflict` not surfaced**: a `vault_write` update with a stale `expected_sha256` was correctly rejected (note never modified) but the job reported `Done` instead of `Conflict`, making the conflict silently invisible to the caller. Fixed by an anti-clobber guard in the job completion path; the `Conflict` status is now preserved through the ack cycle.
- **Code-ingest crash on multibyte source**: byte-slice truncation in the Rust parser is now char-safe (`char_indices`); no panic on source files with multibyte characters or emoji near the truncation boundary.
- **Code-ingest Unicode/space paths**: `git ls-files` and `git diff --name-status` now use `-z` + `core.quotepath=off` + `--no-renames` (NUL-split); paths with spaces or accented characters are ingested and purged correctly.
- **Code-ingest interrupted-ingest drift**: an atomicity marker is placed before any destructive mutation; an interrupted ingest forces a full rebuild on the next run instead of leaving silent drift in the index.
- **`vault_read` title always null**: `vault_read` previously returned `title: null` for all notes; the field is now populated from the note's stored title or extracted from the first Markdown H1.

### Security

- **Non-loopback without TLS refused**: a `bind` address outside loopback now requires `[server.tls]` to be configured; the server refuses to start if neither condition holds (fail-closed). See §Native TLS above.
- **Defense-in-depth against cross-tenant data access** (6-layer fix): two separate `tenant_id` fields (JWT claims vs. request body) were never reconciled, creating a latent path to cross-tenant reads. Fixed across 6 layers: `/auth/exchange` gate, central middleware, handler-level JWT derivation, cross-vault read clamp, worker job rejection, and api_key issuance guard. All six layers covered by tests; smoke-tested LIVE (403 on all cross-tenant paths, 200 on legitimate paths).
- **`code_scope` path anti-traversal**: the path guard is unconditional (not gated on request parameters); symlink traversal is also blocked (IB-5 asserted in tests, IB-7 symlink covered).
- **`vault_write` fail-open closed**: malformed `expected_sha256` returns `400` before reaching the `409` conflict check, closing a guard ordering issue that could have allowed a malformed hash to bypass the conflict check.
- **`vault_timeline` protected sections**: `PROTECTED_FORGET` sections are excluded from all timeline results; 0 leaks in 49 confirmed cases.

### Privacy

Two new at-rest data surfaces in `index.db`:

- **`event_log`**: LLM gateway call metadata (route, model alias, provider, latency, status code). No prompt or response content. Retention: 30 days by default (`[event_log] retention_days`).
- **`session_trace`**: agent action tracing entries. Fields: see `POST /api/v1/session-log/trace` above. Retention: 90 days by default (`[session_trace] retention_days`).
- **HTTP audit log** — planned v0.6.x. In v0.5.2 the server runs with `NoopAuditSink`: no audit files are written and there is **no `[audit]` configuration block**. The `HttpAuditEvent` data shape is defined in `gradatum_core::audit::http` but not wired to any sink.

### Tests

1925 passed / 0 failed (`cargo nextest --workspace --release`); `clippy --workspace --all-targets -- -D warnings` clean.

## [0.4.6] — 2026-06-11 · internal milestone, published in 0.5.2

Introduces a read-mostly operator UI over the vault, along with the backend API surfaces it
consumes. No breaking changes; drop-in upgrade from v0.4.5.

### Added

- **gradatum Studio**: 5 surfaces (React + TypeScript + Vite bundle) served by `ServeDir` under `/ui/*` without auth (LAN — the JS is public). Auth flow: the operator pastes an api-key → `POST /auth/exchange` → JWT stored in `sessionStorage` (never `localStorage`) → `Authorization: Bearer` on every `/api/v1/*` call. Hardened static serving: strict CSP, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Permissions-Policy: geolocation=(), microphone=(), camera=()`. SPA fallback (`ServeDir.fallback(ServeFile index.html)`): deep-links / refresh on client-side routes serve `index.html`; a missing bundle still returns a clean 404.
- **Opt-in score breakdown in `vault_search`**: new request field `include_scores: bool` (default `false`, fully backward-compatible under `deny_unknown_fields`) enriches each `SearchHit` with a `ScoreBreakdown` object (`rrf_score`, `recency_factor`, `pagerank_factor`, `in_degree`, `trust_raw`, `trust_decayed`, `composite`, optional `bm25_rank` / `sem_rank`). Signals were already computed by the scoring pipeline and discarded — they are now exposed only when requested. No `rerank` column (NoopReranker by default). The legacy hardcoded `trust: 0.5` field is documented as deprecated. The MCP tool schema auto-derives the new field via schemars.
- **Review queue endpoint**: new `GET /api/v1/review` (auth, paginated by ULID cursor) listing notes with `status IN ('pending-review', 'staging')`, with `provenance` (distinguishing `distilled` from curator) and a distinct legacy `staging` badge. `confidence` is not exposed (not persisted — honest copy).
- **Dashboard endpoint**: new `GET /api/v1/dashboard` (behind auth; `/health` stays unauthenticated) aggregating, with no new table: `notes_by_status` (tolerant of out-of-enum legacy statuses), `forgotten_count`, `jobs_by_status` (`GROUP BY`, DLQ included), `queue_depth`, `wal_size_bytes` (`null` = "n/a", never a lying 0), and the last job summary. New trait methods `count_notes_by_status` (`DocumentStore`) and `count_jobs_by_status` (`QueueStore`, default empty + native `GROUP BY` override in `SqliteQueueStore`).
- **Move-to-locus endpoint**: new `POST /api/v1/notes/{id}/move {locus}` performing an index-level `UPDATE notes.locus` (consistent with `vault_downgrade` / `patch_note_status`); the ULID is preserved (no redirect table). Strict `LocusId::parse` validation: non-empty, charset `[a-z0-9-/]`, ≤128 bytes, anti-traversal. Clean `400` / `404` / `422`. Physical `.md` relocation is intentionally deferred and documented in the handler contract.

### Changed

- **Curator routes low-confidence notes to `PendingReview`**: `CurateOutcome::Pending` now writes `NoteStatus::PendingReview` instead of `Staging` at the four worker sites (dispatch + apalis, create + reclassify), factored through a single source of truth `gradatum_curator::outcome_to_status` (`Admitted→Live`, `Pending→PendingReview`, `Rejected→None`) to close the parity-bug class. Semantically correct: `PendingReview` = awaiting judgement (feeds `/review`); `Staging` = optional human review. Validated by the curator golden-set F1 gate (orthogonal to the status flip — it measures section routing) plus a mapping parity test.
- **`/health` live metric wiring**: the previously stubbed `sqlite_wal_size_bytes` and `queue_depth` are now real — WAL size read from `AppState.wal_path` (`<index.db>-wal`, set by `with_search_path`) and queue depth derived from `count_jobs_by_status` (`Pending`). `queue_oldest_age_secs` stays 0 (deferred — no dedicated `QueueStore` method).

### Fixed

- **Locus preserved on re-upsert**: `upsert_note` now guards `locus` with `CASE WHEN notes.content_hash IS NOT excluded.content_hash THEN excluded.locus ELSE notes.locus END`. A re-upsert from a stale `.md` (unchanged content hash, as after an index-level `update_note_locus`) no longer clobbers a moved locus; a genuine content change still applies the frontmatter locus. Discriminant = `content_hash`.
- **Review queue tolerates a malformed id**: `list_review_queue` skips a non-ULID id (data anomaly) with a `warn` instead of failing the whole page with a 500; valid rows keep being served.

### Tests

- Workspace tests pass, zero failures; `clippy --workspace --all-targets` clean.
- New coverage: opt-in score breakdown (rrf ranks, omitted/present, MCP schema), curator status-flip parity + worker observable flip, review queue E2E + non-ULID resilience, dashboard aggregate + health re-check, move-locus E2E (success/400/404/422) + `LocusId::parse` unit, locus preservation on re-upsert, studio router (security headers, SPA fallback, missing-bundle 404).

## [0.4.5] — 2026-06-11 · internal milestone, published in 0.5.2

Multi-backend-readiness for the index (testability + decoupling), without shipping an
alternative backend yet. No breaking changes; drop-in upgrade from v0.4.4.

### Changed

- **Worker type-erased on `Arc<dyn Index>`**: the worker now depends on the type-erased `Arc<dyn Index>` facade instead of the concrete `Arc<SqliteIndex>`, unifying the composition root with the server. Eight inherent `SqliteIndex` methods used by the worker outside the three storage traits were promoted into `IndexStore` with neutral default implementations: `set_note_trust`, `write_temporal_entry`, `delete_redirect_by_ulid`, `delete_note_from_index`, `list_garbage_older_than`, `get_note_status`, `get_note_section`, `is_note_forgotten`. An alternative backend does not have to implement them to compile; `SqliteIndex` overrides each by delegation. No rusqlite type is exposed in any promoted signature.

### Added

- **Backend-agnostic index parity suite**: new test-only crate `index-parity-tests` locking the observable contract of the `Index` trait (`DocumentStore` + `IndexStore` + `VectorStore` facade). A `make_index() -> Arc<dyn Index>` factory selects the backend via the `GRADATUM_INDEX_BACKEND` env var (default `sqlite` in-memory) — adding a backend is one match arm + one CI matrix entry, zero duplicated tests. 24 tests across 7 invariant families: write→read round-trip + content hash, FTS + semantic cosine (descending order, downgraded exclusion), status state machine / decay, temporal_index idempotence, dynamic-trust preservation on re-upsert, lesson recall, forget lifecycle. New split CI job `index-backends` (matrix `[sqlite]`) on Forgejo + GitHub.

### Fixed

- **Purge tolerates an unreadable status**: `handle_purge` no longer aborts the whole batch when `get_note_status` fails to parse a candidate's status (e.g. an out-of-enum `'downgraded'` value appearing mid-loop). The offending note is counted ignored + logged (`warn`) and the batch continues purging the other Garbage notes, consistent with the per-note TOCTOU re-check intent.

### Tests

- Workspace tests pass, zero failures; `clippy --workspace --all-targets` clean.
- `index-parity-tests` runs against the sqlite backend via the factory; the `index-backends` CI matrix is extensible to alternative backends.

## [0.4.4] — 2026-06-11 · internal milestone, published in 0.5.2

Adds semantic distillation jobs, trust-decay scoring, a consumable event-log, and lesson
recall. No breaking changes; drop-in upgrade from v0.4.3.

### Added

- **Semantic distillation**: new `Distill` job (`DistillSource`, mode `Semantic` only) that clusters non-processed notes of a scope by embedding cosine similarity (threshold 0.75, batch capped at 500) and writes one synthesis note per cluster in `pending-review` with `provenance: "distilled"` and `derived-from` links; source notes are marked `processed` / `derived-into` via copy-on-write-safe extra fields (no parasite versions). Dry-run is the default; the cron schedule is documented but never enabled by default; vault-wide scope is refused outside dry-run. New `TRUST_SCORES["distilled"] = 0.60`.
- **Trust-decay scoring**: `composite_score` gains an optional trust-decay multiplier applied at the RRF layer only (never BM25), with per-provenance half-lives configurable (default `distilled` 90 days; `human-decision` no decay). Global flag `trust_decay_enabled` (default **on**; can be disabled) makes scores bit-identical to v0.4.3 when disabled. Modifier order documented: forgotten (short-circuit) > downgraded > [RRF × recency × pagerank × trust_decay]. Gated behind a search golden-set non-regression check.
- **Consumable event-log**: engines emit a semantic `agent_id` and a `feature_id` derived from request type (`embed` vs `chat`); the event-log store gains transactional reader methods (`fetch_pending`, `mark_processed`) for the distillation pipeline.
- **Lesson recall**: new `GET /api/v1/lessons/recall?class=<x>&limit=<n>` endpoint — BM25-only (no LLM) over the `lessons-learned` section, filtered to a controlled vocabulary of 12 classes (`400` otherwise), excluding lessons tagged `codified` and forgotten notes; returns `{items:[{ulid, title, snippet, tags, anchor_ms}]}` with a sub-50ms target. Also exposed as the `vault_lessons_recall` MCP tool.
- **Migration 0014**: adds a nullable `outcome` column to `event_log` (additive, safe default).

### Fixed

- **Section filter on the semantic search path** in `vault_search`: when a `section` is requested, semantic-only hits from other sections no longer leak through the RRF fusion (they were previously filtered on the BM25 path only). The semantic hits are filtered by section before fusion; on a batch lookup failure the search degrades to BM25-only rather than risk a section leak.
- **`section` parameter forwarding** confirmed end-to-end through the MCP stub for `vault_search` (the leak was a server-side fusion issue, not a stub forwarding gap).

### Tests

- Workspace tests pass, zero failures; `clippy --all-targets` clean.
- Migration 0014 is covered by automated application and idempotence tests.

## [0.4.3] — 2026-06-10

Semantic forget, note lifecycle state machine, configurable history retention, search scoping, and multimodal content support. No breaking changes; drop-in upgrade from v0.4.1.

### Added

- **Semantic forget** (`vault_forget`): mark notes as forgotten so their search relevance decays progressively (half-life of one day) — notes are **not deleted**; physical removal remains a separate, explicit purge concern. Two-step protocol: `POST /api/v1/vault_forget` with `dry_run: true` (default) returns a preview listing the exact note ULIDs; execution requires `dry_run: false` plus `confirm_ulids` matching that preview exactly (any mismatch → `400` with an explicit error body). Scopes: `topic` (full-text query), `locus` (path prefix), `agent` (author). Protected sections (`agent-issues`, `council`) are always excluded and reported in the preview. Companion endpoints: `GET /api/v1/vault/forgotten` (paginated listing with a global total) and `POST /api/v1/vault/unforgot/{ulid}` (restore). Also available as the `vault_forget` MCP tool and the `gradatum-admin vault forget` CLI subcommand.
- **Note lifecycle state machine**: notes transition between six states (`draft`, `staging`, `pending-review`, `live`, `deprecated`, `garbage`) with validated transitions. `PATCH /api/v1/notes/{id}` with a `status` field returns `409 Conflict` on an invalid transition and `204 No Content` on success (idempotent when the target equals the current state). Each transition is recorded in the note's copy-on-write history.
- **Configurable history retention**: the per-note history cap is no longer hardcoded. New `[history]` config section with `max_versions` (default 50, minimum 1 enforced) and `ttl_days` (optional; unset means no age-based expiry). TTL pruning runs before the count cap, on the write path.
- **Purge job for garbage notes**: `Purge` job deletes notes in the `garbage` state older than a grace period (default 30 days, based on the last status change), including their history and redirect entries. Dry-run is the default and lists affected ULIDs without mutating anything. No purge schedule is enabled by default; activation is an explicit operator decision.
- **Search scoping**: `vault_search` accepts optional `locus` (path-prefix filter, LIKE metacharacters escaped) and `vault_id` (read-only cross-vault scoping). Both filters apply to the full-text and the semantic search paths. Omitting them preserves the previous behaviour exactly.
- **Multimodal content support in the gateway**: `POST /v1/chat/completions` accepts the OpenAI content-array format (text and `image_url` parts, base64 data URIs). Requests containing images are only routed to aliases declared `vision_capable = true` (otherwise `400`); when the vision provider is down, the request fails with `503` instead of silently falling back to a text-only model.
- **Classifier prompt v2**: the curator classification prompt now covers all 11 canonical sections (adding `council`) with refined disambiguation criteria, and an explicit caller-provided section hint is honored when valid.
- **Migration 0012**: adds `forgotten`, `forgotten_at`, `forgotten_by`, and `orphaned` columns to the notes index (additive, safe defaults).
- **Migration 0013**: creates the derived `temporal_index` table (per-note temporal anchor and document kind) with an automatic backfill. Foundation only — no query surface in this release.

### Changed

- **Explicit section hints are authoritative**: when a `vault_write` request provides a `section_hint` matching one of the 11 canonical sections, the note is classified to that section directly (the heuristic and LLM classifier are bypassed). Invalid hints are ignored with a warning and classification proceeds as before.
- **`UnforgotResponse.status`** is documented and returned as `"restored"`.

### Fixed

- **FTS5 query sanitization** in the forget `topic` scope: queries containing hyphens or dates (e.g. `2026-06-10`) no longer fail with an FTS5 syntax error; user-supplied terms are quoted as literals.
- **Double LIKE-escaping** of the `locus` filter: the prefix filter is now escaped exactly once, so prefixes containing `%`, `_`, or `\` match literally and correctly.
- **Forgotten-note decay applied to every search path** (scored, filtered, snippet, and semantic), with results re-sorted after the decay is applied. A note that is both forgotten and downgraded receives the forgotten decay only (no penalty stacking).
- **docs.rs build fix for `gradatum-curator`**: the classifier prompt was referenced via `include_str!` with a path that escaped the crate root, causing docs.rs builds of `gradatum-curator` to fail since v0.4.1. The prompt is now packaged inside the crate (`crates/gradatum-curator/prompts/`). Same fix applied to `gradatum-admin` presets and `gradatum-acl-policy` test fixture.

### Security

- `SECURITY.md` now declares the `forgotten_by` field (free-form actor identifier, stored in the index, the note frontmatter, and API responses — treat as potentially containing PII), the configurable history retention, and the fact that `vision_capable = true` routes base64 image content to the configured backend. See `SECURITY.md` for details.

### Tests

- Workspace: 1407 tests pass, zero failures; `clippy --all-targets` clean.
- Both migrations are covered by automated application and idempotence tests; operators are advised to keep the standard pre-migration backup enabled.

## [0.4.1] — 2026-06-06

Quality and reliability improvements across security, documentation, and correctness. No new features; drop-in upgrade from v0.4.0.

### Fixed

- **Unimplemented endpoints**: MCP endpoints not yet implemented now return `501 Not Implemented` with a descriptive message instead of silently enqueuing jobs that never complete.
- **Trait default panics**: default implementations of storage-trait methods now return a typed error instead of panicking, making partial backend implementations safe at runtime.
- **Token revocation**: API token revocation is now checked on every request, not only at issuance. Revoked tokens are rejected immediately.
- **Embedding endpoint defaults**: the default embedding endpoint URL was incorrect and caused connection failures on fresh deployments; corrected to match the documented value.
- **Queue transition atomicity**: job state transitions are now performed atomically, preventing duplicate processing under concurrent workers.
- **SQLite lock contention**: the SQLite write lock is released before vector computation begins, eliminating a potential stall under concurrent search and write workloads.

### Security

- **API key entropy**: API keys are now 256-bit (32 bytes), replacing the previous 128-bit keys.
- **Secret file permissions**: secret files are written atomically with `0600` permissions, removing a window where the file was briefly readable by other processes before chmod.
- **History retention bound**: note history is capped to a fixed maximum (50 versions per note) to prevent unbounded disk usage over time; older versions are pruned automatically.
- **Privacy posture documented**: `SECURITY.md` now describes what data is stored locally, what may leave the host, and the absence of at-rest encryption.
- **Incorrect encryption claim corrected**: documentation previously stated that gateway body logging was encrypted at rest; this claim was inaccurate and has been removed.

### Documentation

- **docs.rs coverage**: all public items across the workspace now carry accurate doc-comments. Internal implementation details, broken links, and references that were not meaningful to library users have been removed or corrected.

## [0.4.0] — 2026-06-06

Vault durable writes — note history, optimistic locking, stable wikilinks, write provenance.

### Added

- **Provenance Trust** : `provenance` field (String) in note frontmatter ; `trust` column in index.db (confidence score 0.0–1.0). Presets: human-decision 0.95, qa-event 0.75, agent-log 0.50, web-scraped 0.35. Stored for use in scoring; trust decay scoring planned for v0.4.1.
- **Stable Wikilinks** : `redirect_table` maps old titles to new ULIDs. `vault_read` resolves title-based lookups via `IndexStore::resolve_redirect()`. CLI support: `gradatum-admin vault rename --old-title <T> --new-ulid <U>`.
- **Optimistic Locking** : `vault_write` accepts optional `expected_sha256: Option<String>` parameter. Conflict detection on worker (non-blocking). Job result includes `JobStatus::Conflict`; client polls via `/jobs/{id}`. Backward-compatible: omitting `expected_sha256` means unconditional write.
- **Note History** : Copy-on-Write `.history/<ulid>/<timestamp>.md` on delta detection (excludes transient fields). New MCP endpoints: `vault_history(note_id)`, `vault_history_get(note_id, timestamp)`, `vault_restore(note_id, timestamp)`, `vault_diff(note_id, t1, t2)`. History retention policy planned for v0.4.2.
- **Migration 0010** : adds `provenance TEXT` and `trust REAL DEFAULT 0.5` columns to notes table; creates `redirect_table(source_title TEXT UNIQUE, target_ulid TEXT REFERENCES notes(id))`.
- **Migration safety** : pre-migration backup script included in systemd units (ExecStartPre hook). Archives vault, queue, and audit DBs to `.tar.zst` with 7-day retention. See docs/DEPLOYMENT.md for configuration.

### Changed

- **Storage traits** : `DocumentStore`, `IndexStore`, `VectorStore` traits finalized for multi-backend support. No breaking API changes; dispatch overhead negligible.

### Fixed

- **Provenance backfill** : migration 0010 sets `provenance='agent-log'` for all existing notes lacking provenance (idempotent).

### Tests

- **Workspace** : 1178 tests PASS, 0 clippy warnings, 0 regressions.
- **Known limitations**: history pruning policy and trust-decay scoring are deferred to later releases.

## [0.3.7] — 2026-06-05 · internal milestone, published in 0.4.0

Reliability fixes: search/read/write consistency and wikilink stability.

### Fixed
- **vault_write/worker** : fixed ULID mismatch between enqueued note ID and persisted note ID. `write_note_with_id()` ensures write-time ID == stored ID.
- **vault_read** : now accepts `<section>/<ulid>` format returned by `vault_search` (round-trip consistency). ULID and title lookups remain supported.

### Changed
- **vault_search** : score documentation clarified. Score is a composite RRF rank, not a [0–1] similarity value.

## [0.3.6] — 2026-06-05 · internal milestone, published in 0.4.0

Add per-crate README.md for crates.io documentation pages; metadata only, no code changes.

### Added

- `README.md` for all 26 publishable crates in the workspace (one file per crate,
  co-located with `Cargo.toml`). Each README reflects the actual v0.3.x implementation:
  role, API surface, feature flags, and usage example where applicable.

## [0.3.5] — 2026-06-03

Enriches `title` and `section` fields for semantic-only hits in `vault_search`.

### Fixed

- **`vault_search`: `title = null`, `section = ""` for semantic-only hits**: after RRF
  fusion, notes present only in the semantic signal (absent from `bm25_map`) retained
  `title = null` and `section = ""` in the final response. A batch enrichment pass
  (`get_titles_sections` — single `SELECT … WHERE id IN (…)`) now fetches `title` and
  `section` from the `notes` table for all missing hits, just before `SearchHit`
  construction. Existing BM25 enrichments are not overwritten. `snippet` remains `None`
  for semantic-only hits: no FTS5 match is available to generate a localized excerpt.

### Added

- **`IndexStore::get_titles_sections`**: new batch helper on the `gradatum-core::IndexStore`
  trait — `SELECT id, title, section FROM notes WHERE vault_id = ? AND id IN (…)` — used
  by the enrichment pass above. Implemented in
  `gradatum-index::SqliteIndex::get_titles_sections`.

## [0.3.4] — 2026-06-03

Fix `vault_search` returning `title: null` for all notes written before this
version.  The `notes.title` column (added in 0.3.0 / migration 0005) was never
populated at write time; migration 0009 backfills the existing corpus by
extracting the first Markdown H1.

### Fixed

- **`notes.title` always null at write-path**: `handle_curate` now calls
  `upsert_note_title` after every successful `vault.write_note`, resolving the
  title from the explicit `spec.title` field (API payload) or, as a fallback,
  from the first `# H1` line of the note body via `extract_h1_title`. The call
  is non-fatal: a failure is logged as `WARN` and does not roll back the write.
- **Migration 0009 — backfill `notes.title` for existing corpus**: applies an
  `UPDATE notes SET title = TRIM(SUBSTR(body_text, 3, …))` for rows where
  `title IS NULL OR title = ''` and `body_text LIKE '# %'`, extracting the H1
  header. Idempotent (guard on `title IS NULL OR title = ''`). Does not overwrite
  already-set titles.

### Known limitations

- Notes written before v0.3.4 whose body does not start with a Markdown H1 will
  retain `title = NULL` after the backfill (~895/911 notes in the reference
  deployment). These titles are not recoverable from the current schema; future
  writes for those notes will populate the column correctly going forward.
- The `classify` / `reclassify` worker path does not yet call
  `upsert_note_title` (annotated with `NOTE v0.3.4` in `dispatch.rs`). Notes
  updated exclusively through reclassification will have their title populated on
  the next normal curate cycle.

## [0.3.3] — 2026-06-02

Reliability patch: fix the multi-worker queue deadlock that starved one job
kind (the actual cause behind the worker stall; 0.3.1/0.3.2 fixed adjacent
issues but not this).

### Fixed

- **Multi-worker dequeue deadlock**: `dequeue`/`dequeue_by_kind` ran a
  `SELECT … FOR lease` then `UPDATE` inside a `BEGIN DEFERRED` transaction, so
  the read lock had to upgrade to a write lock. Under concurrent workers two
  dequeues deadlocked on the upgrade (`SQLITE_BUSY`), starving one kind (e.g.
  embeddings stayed `Pending` indefinitely while curation drained). The two
  dequeue sites now use `BEGIN IMMEDIATE`, acquiring the write lock up front so
  dequeues serialize without deadlock. Covered by a multi-kind concurrency
  regression test (10 curate + 30 embed → all drained in parallel).

## [0.3.2] — 2026-06-02

Reliability patch: fix the worker stopping after draining a batch (the actual
root cause of the intermittent stall that 0.3.1 did not fix).

### Fixed

- **Worker stops after batch drain**: the custom Apalis backend fetcher had an
  internal `loop {}` that never yielded to the worker on an empty queue, so
  under the concurrency gate a wakeup was lost when the queue drained — the
  worker stopped and the Monitor shut down, leaving new jobs unprocessed until a
  restart. The fetcher now follows the canonical Apalis pattern (one poll = one
  dequeue, yields `Ok(None)` on empty), so the worker keeps polling. Covered by
  a regression test that drives a real Monitor (drain → re-enqueue → processed).

## [0.3.1] — 2026-06-02

Reliability patch: eliminate an intermittent worker hang on `vault_write`.

### Fixed

- **Worker hang under SQLite contention**: job acks (`fail`/`complete`) returned
  `SQLITE_BUSY` immediately and failed silently, leaving jobs stuck `Running`
  until lease expiry, then re-dequeued and re-wedged. Added `busy_timeout(5s)` +
  WAL on all sqlx pools (queue/server/worker) so SQLite retries internally.
- **DLQ guard infinite loop**: `promote_retries` read the retry counter from a
  stale serialized blob (always 0) instead of the SQL `attempt_count`; the guard
  now reads SQL so jobs terminate to DLQ at max retries.
- **DLQ replay no-op**: `jobs dlq --replay` now resets `attempt_count` so
  replayed jobs get fresh retries.

### Changed

- `gradatum-worker.service` reads an optional `EnvironmentFile` for `RUST_LOG`
  (worker observability).

## [0.3.0] — 2026-06-02

Storage trait decomposition, event-log sink, gateway cost-attribution, cognitive kind capture, and secrets dependency injection.

> **Breaking change (deploy)**: JWT signing key is now persisted. First deploy of v0.3.0 invalidates all existing JWTs. Consumers must re-exchange API keys for new JWTs after deploy.

### Added

- **Storage trait decomposition**: monolithic `trait Index` decomposed into three granular traits in `gradatum-core` — `DocumentStore` (note CRUD), `IndexStore` (FTS5, scoring, wikilinks), `VectorStore` (embedding + ANN). `trait Index` facade with blanket impl preserves call site compatibility. `AppState.search` uses vtable dispatch (`Arc<dyn Index>`). Types `SearchHitRaw`, `AuthorRow`, `Lineage` made public.
- **Event-log sink**: dedicated SQLite table `event_log` (migrations 0006/0007) — append-only, outside notes/notes_fts. Endpoint `POST /api/v1/event-log` with timestamp/payload bounds, log-injection sanitization. `EventLogStore` with `insert_batch` / `purge` / `count`. Retention policy: 30-day TTL, 6-hour purge interval, 5M-row cap. Prometheus metric included.
- **gradatum-gateway crate**: autonomous LLM proxy service (`:8436`). Routes: `/v1/chat/completions` (+SSE), `/v1/embeddings`, `/v1/rerank` (ONNX cross-encoder), `/v1/models`, `/health`, `/metrics`. Replaces standalone LLM services.
- **Cost attribution**: `QaEvent` enriched with feature_id, model_used (fallback-aware), tokens_input/output, cost_usd. Streaming paths omit token counts.
- **Cognitive kind capture** (migration 0008): columns `c_kind` (CoALA categories: episodic / semantic / procedural / reflective) and `doc_kind` (Event / Static) added to `notes`. Derived deterministically from `section` via const functions in `gradatum-core`. Zero LLM runtime cost. `section` remains authoritative; `c_kind`/`doc_kind` are derived metadata. Scoring unchanged (doc_kind usage deferred).
- **Secrets dependency injection**: `SecretsProvider` trait + `SecretBytes` (crate `secrecy`, Drop-zeroize, Debug masked) + `EnvSecretsProvider` + `FileSecretsProvider` in `gradatum-core/src/secrets.rs`. File secrets provider refuses overly-permissive permissions at load time.

### Changed

- **Workspace**: 26 → 28 crates (added `gradatum-gateway` + `gradatum-db-sqlite` promoted).
- **AppState.search** : switched to vtable dispatch for Index trait (`Arc<dyn Index>`), enabling future multi-backend support without recompilation.
- **Job dequeue filter** : fixed `dequeue_by_kind` to enforce strict `kind` matching. Previously, a Curate job could be processed by the wrong worker type, causing note loss.

### Fixed

- **JWT signing key persistence** : key was ephemeral (regenerated per boot). Now persisted to disk via `FileSecretsProvider` (mode 0600). See breaking change note above.
- **Job dequeue routing** : fixed `WHERE kind = ?` filter to prevent job type mismatches.

### Security

- **Secrets hardening**: `FileSecretsProvider` enforces file mode 0600 and directory mode 0700 at `O_CREAT` (zero world-readable window). Seed zeroize on drop via `secrecy`. Path-traversal guard on secret file paths. Warning logged on permissive permissions.
- **Event-log endpoint hardening**: timestamp bounds (400 on out-of-range), field bounds (422 on oversized payloads), `DefaultBodyLimit`, log-injection sanitize on string fields.
- **Secrets DI**: eliminates inline secret literals; secret material flows exclusively through `SecretsProvider` trait implementations with memory-zeroing guarantees.

### Tests

- Workspace: **1088 PASS** (up from 886 v0.2.0 baseline, +202 new across 5 tranches).
- **0 FAILED** across 28-crate workspace.
- Golden search regression: **3/3 diff-zero** maintained across all tranches.
- `clippy --all-targets`: 0 warnings maintained.
- Security review findings: all HIGH severity findings resolved.

## [0.2.0] — 2026-05-29

Apalis job infrastructure, Dead-Letter Queue, jobs introspection API with SSE, and Prometheus observability.

### Added

- **Apalis job infrastructure**: 22 Job variants (`JobKind` enum) covering curator and maintenance flows. `JobRecord` 5-block structure with forward-compatible fields. Custom `GradatumQueue` facade over Apalis `Backend`. `SqliteQueueStore` with atomic lease semantics. Framework-agnostic: future swap to Redis/RabbitMQ/Postgres needs only a new `QueueStore` impl.
- **Dead-Letter Queue + Monitor**: automatic DLQ routing for jobs exceeding max retries. Apalis Monitor for multi-worker coordination with timeout, retry, panic isolation, and load shedding layers. Graceful shutdown with 30s drain.
- **Jobs introspection API**: five HTTP endpoints for job lifecycle (enqueue, status, stream, cancel) + Prometheus metrics. Server-Sent Events for streaming. Idempotency-Key header support. `gradatum-admin jobs` CLI commands for inspection and control.
- **Prometheus exporter**: `:19091` pull endpoint, disabled by default (`metrics_enabled = true` in config to enable). Per-job-kind metrics.
- **`gradatum-db-sqlite` crate (new)**: isolates SQLite queue implementation — 15 methods, WAL mode, index on `(vault_id, job_kind, status)`.

### Fixed

- **`SqliteQueueStore::get()` stale payload**: record lifecycle fields (`started_at`, `completed_at`, `duration_ms`) were desynchronised from authoritative SQL columns. Fixed by syncing from SQL in `get()`.
- **`duration_ms` stub**: `JobResult.duration_ms` was hardcoded 0. Now measured via `std::time::Instant` injected in `record_to_task` and recovered in `GradatumAcknowledger::ack()`.
- **Apalis ack/complete wiring**: `apalis::Backend::ack`/`complete` now properly wired via `GradatumAcknowledger`.
- **`enable_tracing` panic**: `enable_tracing` re-enabled; `TaskId` injection in `record_to_task` resolves a panic in `make_span`.

### Tests

- 886 PASS / 0 failed.
- E2E integration: write note → curator job enqueued → Monitor processes → metric exported → SSE subscribers notified.

## [0.1.0-alpha.15] — 2026-05-28

### Security

- **LIKE wildcard escaping in `title_lookup`**: SQL wildcards (`%` and `_`) in note titles
  are now escaped via `escape_like_pattern` + SQLite `ESCAPE '\\'`, eliminating false-positive
  LIKE matches in `vault_read`, `vault_trace`, and classify.

### Performance

- **`vault_trace` parallel seed resolution**: seed entries are now resolved concurrently via
  `tokio::JoinSet`, eliminating the sequential N×seed round-trip.
- **Wikilink `title_lookup` parallel resolution**: wikilink resolution in the worker now uses
  `tokio::JoinSet` instead of a sequential `.await` loop.
- **Reranker single-pass tokenization**: `encode_batch` pre-tokenizes in one pass.

### Changed

- **`vault_classify` LLM cascade**: `vault_classify` now invokes the LLM curator in cascade
  with category normalization, fallback on curator error, and status propagation.

### Added

- **`gradatum-admin backfill-titles`**: new CLI subcommand that populates the `title` column
  for notes where it is null, extracting the value from the note body.

### Removed

- **`X-Gradatum-Wait` header**: the stub `X-Gradatum-Wait` header and `sync_wait` logic have
  been removed from `gradatum-server` — the server is async-only.

### Tests

- 826 PASS / 0 regressions; `cargo deny` GREEN; 0 clippy warnings.
- LIKE injection prevention and rate limiting confirmed by integration tests.

## [0.1.0-alpha.14] — 2026-05-28

Security hardening and CI release infrastructure.

### Security

- **JWT not-before validation** : explicitly enabled `validation.validate_nbf = true` in `crates/gradatum-auth/src/jwt.rs`. Default behavior in jsonwebtoken v9 skips this check, silently accepting future-dated tokens.

### Infrastructure

- **CI actions pinning** : pinned artifact actions to v3 for Forgejo Actions compatibility. Docker build job disabled pending docker-capable runner provisioning.

### Tests

- 13 tests pass; `cargo build --release` passes; no regressions.

## [0.1.0-alpha.13] — 2026-05-10

Endpoints completeness: wikilinks, title lookup, vault trace, and context budget support.

### Added

- **Wikilinks post-curate** : `process_wikilinks_b5` parses `[[wikilinks]]` and inserts edges into `note_links`.
- **Title lookup in vault_read** : `vault_read` now accepts both ULID and title lookups via `find_note_by_title()`.
- **vault_trace multi-mode** : supports ULID lookup (lineage), title lookup, and full-text query (FTS5 multi-match + aggregated lineage).
- **vault_context token budget** : `vault_context` enforces token budget via heuristic `chars/3.0` (UTF-8 safe). Returns top-10 notes under budget.

### Tests

- 779 → 796 PASS workspace (+17). 0 clippy, 0 fmt, `cargo deny` GREEN.
- Smoke E2E: auth exchange, health check, write→curate→read+trace+context integration.

### Changed

- Install script renamed to `install-gradatum-services.sh`; `install-gradatum-stub-mcp.sh`
  added.

## [0.1.0-alpha.12-bumps.1] — 2026-05-10

Dependency upgrades: supply chain hardening (5 sequential PRs).

### Changed

- **serde_yml** : upgraded to maintained fork (0.0.12) post-deprecation of upstream `serde_yaml`.
- **MCP protocol** : upgraded `rmcp` 0.x → 1.x and `schemars` 1.x (stabilisation).
- **HTTP stack** : upgraded `axum`, `tower-http`, `reqwest` with adapter updates for breaking changes.
- **Cryptography** : upgraded `sha2` 0.10 → 0.11, `governor`, `nix`, and 12 minor dependencies.
- **TOML** : upgraded `toml` 1.x suite with MSRV bump to 1.85 and clippy fixes.

### Deferred

- **rusqlite upgrade** : deferred until `sqlx 0.9` stable (linking conflict with `sqlx 0.8.6`).

### Tests

- 779 PASS / 0 clippy / 0 fmt / `cargo deny` GREEN on each merge.

## [0.1.0-alpha.12] — 2026-05-10

Multi-factor scoring and cross-encoder reranker integration.

### Added

- **Multi-factor scoring** : recency and PageRank factors combined via composite scoring (`composite_score = rrf × (1 + α·recency) × (1 + β·pagerank)` with α=0.2, β=0.1).
- **Backlinks queries** : `get_indegree()` and `get_note_created_and_indegree()` for lineage scoring.
- **Reranker trait** : pluggable trait with `NoopReranker` (default) and `OnnxCrossEncoderReranker` (feature-gated `onnx-reranker`).

### Fixed

- **ONNX tensor API** : adapted to `ort 2.0.0-rc.9` API (tuple-based shape + extraction).

### Tests

- 754 → 779 PASS workspace (+25). 0 clippy, 0 fmt.

### Known limitations

- The reranker model path is not yet configurable via environment variable;
  `NoopReranker` is the default.

## [0.1.0-alpha.11-patch.1] — 2026-05-10

Design foundations: SearchHit enrichment and error propagation.

### Added

- **SearchHit title enrichment** : `SearchHit.title` field populated from RrfHit, eliminating need for round-trip `vault_read` calls.
- **Inference error handling** : `GradatumError::Inference` variant for clean error propagation from embed/rerank layers.

### Coverage

- **RRF handler integration** : 4 new E2E tests for RRF fusion path, graceful degradation, and error handling.

### Changed

- Resolved pre-existing clippy warnings in `search_semantic.rs`.

### Tests

- 740 → 754 PASS workspace (+14). 0 clippy, 0 fmt.

## [0.1.0-alpha.10] — 2026-05-10

Vault API completeness: status reporting, pagination, title tracking, and section filtering.

### Fixed
- **vault_status** : `note_count` now returns accurate `COUNT(*) WHERE status='live'`. `total_size_bytes` returns accurate byte sum.
- **vault_search** : section filtering now applied via conditional `WHERE n.section = ?`.

### Added
- **vault_list pagination** : cursor-based pagination via `list_notes()` with lexicographic ULID ordering.
- **Note titles** : migration `0005_add_title_column` adds `title` column. `extract_h1_title()` extracts from body. `upsert_note_title()` keeps in sync.
- **FTS5 snippets** : native FTS5 snippet extraction localizes relevant passages instead of truncating.

### Tests
- 698 → 720 PASS workspace (+22). Unit tests for status, title, section filtering, snippets, and pagination.

## [0.1.0-alpha.9] — 2026-05-09

### Added

- **`vault_downgrade` endpoint**: parity with the legacy vault MCP.
  - Migration `0004_vault_downgrade.sql`: adds `replaced_by TEXT REFERENCES notes(id)` column
    and a partial index `idx_notes_status_downgrade WHERE status='downgraded'`.
  - DTO: `VaultDowngradeRequest/Response`, `NoteStatusPatch`, and
    `VaultSearchRequest.include_downgraded` extension (default `false`).
  - Endpoints: `POST /api/v1/vault_downgrade` (synchronous 200) and `PATCH /api/v1/notes/:id`
    (status patch, 204).
  - SQL helpers: `SqliteIndex::downgrade_note(id, reason, replaced_by)` and
    `patch_note_status(id, status?, reason?, replaced_by?)` (idempotent UPDATE; 404 if not
    found).
- **Downgraded-note search filter**:
  - `vault_search` excludes `status='downgraded'` by default.
  - `include_downgraded=true` penalizes the BM25 score (approximately 10% relative relevance).
  - `Index::search_fts_scored` trait gains `include_downgraded: bool` parameter; return type
    extended to `Vec<(NoteId, f64, String_status)>`.
- **MCP tool `vault_downgrade`**: thin proxy for drop-in compatibility with the legacy vault.
- **`gradatum-admin downgrade-from-legacy-vault-trash`**: imports `.vault-trash/<date>/*.md`
  files from the legacy vault into gradatum (idempotent, `--dry-run`, `--limit`).

### Changed

- `vault_downgrade` endpoint changed from asynchronous (202) to synchronous (200).
- Field name `replaced_by` aligned across DTO, SQL, and handlers.

### Tests

- 668 PASS / 0 failed (+29).

## [0.1.0-alpha.8-patch.1] — 2026-05-09

### Fixed

- **Missing `[embed]` section in generated `server.toml`**: the `[embed]` configuration
  section was absent from the template, causing all embedding jobs to silently skip
  (`enabled=None` → embedder resolved to `None` → `process_embed_note` early-returned without
  an HTTP call). Added `[embed]` section with explicit defaults (`enabled=true`,
  `timeout_ms=5000`).
- **Embed model/dimension defaults corrected**: `EmbedConfig::default()` and the `[embed]`
  template updated from `bge-small-en-v1.5` / 384 dimensions to `bge-m3-Q8_0` / 1024
  dimensions.

### Tests

- Regression tests added: `merge_adds_embed_section_when_backup_lacks_it`,
  `embed_defaults_match_documented_values`.

## [0.1.0-alpha.8] — 2026-05-09

### Added

- **`gradatum-warden` crate**: perimeter defense layer — CIDR IP filter, per-IP token-bucket
  rate limiting, loopback bypass. Public API: `WardenLayer`, `WardenConfig`, `WardenError`,
  `WardenDecision`. Advanced features (audit, GeoIP, hot-reload) deferred to a future release.
- **Rate limiting** on `/api/v1/*` and `/auth/exchange` (exempt: `/health`, `/metrics`).
  Default: 60 req/min, burst 10, `exempt_localhost` configurable. Returns `429` +
  `Retry-After`. Config: `[ratelimit]` block in `server.toml`.
- **Optional auth on `GET /api/v1/jobs/:id`**: new `[auth].require_jwt_jobs_endpoint` flag
  (default `false`). When `true`, a Bearer JWT is required.
- **Asynchronous embedding pipeline**: after note curation, an `embed_note` job is
  automatically chained. The worker fetches embeddings from the configured HTTP endpoint and
  stores them in `note_embeddings` (UPSERT, f32 LE). Config: `[embed]` block in `server.toml`
  (default: `localhost:8431`, model `bge-small-en-v1.5`, 384 dimensions, 5 s timeout).
- **`gradatum-admin backfill-embeddings`**: CLI subcommand that scans notes without embeddings
  and enqueues `embed_note` jobs idempotently. Args: `--root`, `--tenant`, `--limit`.
- `SqliteIndex::insert_note_embedding` and `get_note_embedding` helpers (UPSERT, f32 LE,
  validates `vector.len() == dim`).
- `EmbedConfig` and `RateLimitConfig` added to `ServerConfig`.

### Changed

- **Loopback bypass fix**: loopback clients (`:19090`) now receive the real handler response
  instead of `Body::empty`. Fixed via `WardenService::call` early-return `inner.call(req)`.
- **`gradatum-embed`**: `fastembed-cpu` feature is not enabled by default; the HTTP backend is
  the default (no ORT dependency required).
- **Ingestion pipeline**: `embed_note` is non-blocking — the note is persisted to the vault
  and FTS5 index before the embedding job is chained (best-effort).

### Removed

- `tower_governor 0.5` dependency: its `error_handler` terminated the middleware chain with an
  empty body, incompatible with the loopback bypass.

### Tests

- 636 PASS / 0 failed (+45).

## [0.1.0-alpha.7-patch.6] — 2026-05-08

### Fixed

- **Worker leadership lease not released on clean shutdown**: `gradatum-worker` received
  SIGTERM and logged a clean shutdown but did not delete its row in the `worker_leadership`
  table. Consequence: a rapid stop+start left the next worker retrying ~60–75 s (4 × 15 s)
  before taking over after TTL expiry.

### Changed

- `LeaderElection::release()` added in `leader.rs`: issues a `DELETE WHERE holder = ?`
  (race-safe — does not touch a lease held by another worker). Called from `main.rs` after
  `renewal.abort()` on clean shutdown (best-effort; errors are logged, not fatal).
- Stop+start takeover latency: **~60–75 s → < 1 s**.

### Added

- 4 integration tests in `leadership_cleanup.rs`: `release_removes_own_row`,
  `release_is_idempotent`, `release_only_self_not_other_holder`,
  `release_without_acquire_is_noop`.

## [0.1.0-alpha.7-patch.5] — 2026-05-08

### Fixed

- **`gradatum-worker` stays `inactive` after rapid stop+start**: `Restart=on-failure` in
  `gradatum-worker.service` did not cover a legitimate exit 0 ("not leader") when another
  worker still held the lease. Without automatic restart, the service stayed `inactive (dead)`
  until the lease expired naturally (~60 s) and required manual intervention.

### Changed

- `Restart=on-failure` → `Restart=always`, `RestartSec=5s` → `RestartSec=15s` in the systemd
  unit file. Systemd now always restarts; the leadership lease expires naturally (~60 s) and
  takeover is automatic on the next cycle.
- Motivation comments added inline in the unit file.

## [0.1.0-alpha.7-patch.4] — 2026-05-08

### Fixed

- **Structural merge bug in `walk_and_merge`**: `gradatum-admin/src/init.rs` iterated over
  keys from the new template and discarded sections present only in the user backup (e.g.
  `[curator]`, `[curator.llm]`). On re-install, this wiped the live `[curator]` configuration,
  causing `gradatum-worker` to go `inactive (dead)`.

### Changed

- **Merge semantics inverted**: the backup is now authoritative for all user content (custom
  sections, extension sections, customized keys). The new template only augments with:
  - New keys/sections absent from the backup (added with their default values)
  - Default values for keys the backup does not define
- `KEY_MIGRATIONS` renames (`db_path` → `vault_index_path`) applied pre-walk on a copy of the
  backup to maintain consistency.
- Helpers `lookup_item_mut` and `set_item` replaced by `set_item_or_insert` and `remove_path`.
- Merge log now emits a `user_added` counter for sections/keys preserved from the backup.

### Added

- 2 regression tests: `merge_preserves_backup_only_sections_curator` (exact reproducer) and
  `merge_adds_user_only_top_level_section`.
- `set_item_or_insert` — helper that creates intermediate nodes when absent.
- `remove_path` — helper that deletes a key by dotted path.

## [0.1.0-alpha.7-patch.3] — 2026-05-08

### Added

- **Atomic `bearer.toml` backup**: `gradatum-admin init --force` (and
  `install-gradatum-services.sh`) now backs up `bearer.toml` to `.bak.<ISO-TS>` before
  overwriting. Consistent with the `server.toml` backup behaviour from patch.2.
- 2 regression tests: `materialize_preset_backups_existing_bearer_toml` and
  `materialize_preset_no_backup_on_fresh_install`.

### Known limitations

- Manual customisations to `bearer.toml` are overwritten in the active file on `--force`
  re-init but remain recoverable from the backup. Automatic merge support is deferred to a
  future release.

## [0.1.0-alpha.7-patch.2] — 2026-05-08

### Added

- **Schema-directed `server.toml` merge**: `gradatum-admin init --force` no longer blindly
  overwrites. Pattern: atomic backup `.bak.<ISO-TS>` + schema-directed merge. Preserves user
  customisations (`[curator.llm].base_url`, `api_key_env`, `timeout_ms`, `jwt_ttl_*`, etc.);
  adds new keys with defaults; drops legacy keys absent from the new schema.
- **Explicit `KEY_MIGRATIONS` table**: handles cross-version key renames
  (`storage.db_path` → `storage.vault_index_path`).
- 3 regression tests: `merge_preserves_user_curator_customizations`,
  `merge_drops_legacy_db_path_via_rename_migration`, `merge_keeps_new_keys_with_defaults`.
- `toml_edit = "=0.22.27"` workspace dependency (preserves TOML format and comments via
  `DocumentMut`).
- `gradatum-admin` gains a `[lib]` target for integration-test access without the binary.
- `generate_server_toml_template` and `merge_user_config` exposed as `pub`.

## [0.1.0-alpha.7-patch.1] — 2026-05-08

### Fixed

- **`gradatum-admin init` template still used legacy `db_path`**: the generated `server.toml`
  template referenced `db_path` instead of the canonical `vault_index_path`, triggering a
  deprecation WARN on every fresh or forced init. Fixed in `init.rs` with a regression test
  in `init_clean.rs`.

## [0.1.0-alpha.7] — 2026-05-08

### Changed

- **`[storage].db_path` renamed to `[storage].vault_index_path`**: backward-compatible via
  `serde(alias)` — the old name is still accepted but emits a WARN at boot. The alias will be
  removed in a future release.

### Added

- `StorageConfig::legacy_alias_used()` — detects use of the deprecated field name.
- `build_snippet` exposed as `pub(crate)` (deduplication between test and production paths).
- 3 regression tests for UTF-8 ZWJ emoji boundary handling.
- `EXPECTED_TOOL_NAMES` constant in MCP stub tests (dynamic tool count).

## [0.1.0-alpha.6] — 2026-05-08

### Fixed

- **`GET /api/v1/jobs/<id>` now returns real status**: previously always returned `"pending"`;
  now reflects actual transitions `pending` → `leased` → `done`. **Behavioural breaking
  change**: a non-existent id returns `404 Not Found` instead of `200 + pending`.
- **BM25 ranking**: `POST /api/v1/vault_search` now uses native FTS5 `bm25(notes_fts)` instead
  of a positional proxy score. Score normalised to `[0..1]` via `1.0 / (1.0 + bm25.abs())`.
- **Information disclosure**: `last_error` is now mapped to opaque codes
  (`invalid_input` / `vault_error` / `storage_error` / `processing_error`) before being
  returned in the API response, preventing leakage of filesystem paths, internal ULIDs, and
  anyhow error chains.

### Added

- `Queue::get(id) -> Option<JobInfo>` (async trait).
- `SqliteQueue::get` impl with `SELECT ... FROM jobs_v2 WHERE id = ?`.
- `Index::get_note(tenant_id, note_id) -> Option<NoteRecord>` (async trait).
- `Index::search_fts_scored(...) -> Vec<(NoteId, f64)>` (real BM25).
- `SqliteIndex::search_fts_scored` impl with `bm25(notes_fts)`.
- `JobInfo` struct (read job metadata without claiming).
- `JobStatus::as_str` and `from_str` helpers.
- `sanitize_job_error` mapping to opaque codes.
- `NoteRecord` moved to `gradatum-core::index` (portable type).
- 11 regression tests (Queue::get unit, status helpers, sanitize, E2E poll, BM25 ordering).

### Tests

- 566 PASS / 0 failed (+21).

## [0.1.0-alpha.5] — 2026-05-07

### Added

- **Auth via API key and `/auth/exchange`**:
  - `gradatum-acl-auth::ApiKeyStore` trait and `SqliteApiKeyStore` impl; argon2id hashing
    `m=19456 KiB / t=2 / p=1`
  - Migration SQL: `api_keys` table, index, and integrated init
  - CLI commands: `gradatum-admin api-key {create,list,revoke,rotate}` and
    `gradatum-admin token issue`
  - Endpoint `POST /auth/exchange {api_key}` with uniform 401 outside the JWT middleware
  - `SqliteRevocationStore` wired at runtime; checked on every exchange call
- **Mandatory `Claims.tenant_id`** with `TrustContext` propagation through the middleware
  layer
- **11 integration tests** (E2E auth flow + tenant propagation):
  - `auth_e2e_full_flow.rs`: 5 tests (create key → exchange → TTL check)
  - `auth_tenant_propagation.rs`: 6 tests (TrustContext leak + middleware accept/reject)
- `scripts/smoke-alpha-5.sh`: 9-step acceptance smoke + RAM check
- `ExchangeResponse` V2: 5 fields — `token`, `ttl_secs`, `scopes`, `tenant_id`, `kid`
- `AppState::with_acl_preset_path()` wired from `cfg.acl.preset_path`

### Changed

- `ExchangeResponse.expires_in` → `ttl_secs`
- `AuthConfig::default()` `revocation_db_path` and `api_keys_db_path`: absolute paths via
  config instead of auto-derived `None`
- Migration `api_keys.sql`: removed `PRAGMA journal_mode = WAL` (sqlx::migrate runs inside
  an implicit transaction — SQLite rejects the pragma there). WAL now configured via
  `SqliteConnectOptions::journal_mode(Wal)` at connection time, before migrations.
- `queue_path` convention: `<root>/queue.db` → `<root>/db/queue.sqlite` (aligns with the
  `db/` folder layout)
- `gradatum-admin init --preset`: embeds presets via `include_str!` (idempotent on
  re-install); install script `scripts/install-gradatum-services.sh` added

### Fixed

- **NFS build-artifact corruption**: 24 `target/debug/deps/` files corrupted by
  `zstd: stdout: I/O error` after a filesystem availability incident. Cleaned and rebuilt;
  two latent code bugs surfaced and fixed (WAL pragma in migration, absolute
  `AuthConfig` defaults).
- `AclEngine` not loading from `cfg.acl.preset_path` — previously hardcoded to an empty
  preset, causing all vault operations to return 403.

### Tests

- 492 PASS / 0 FAIL / 9 ignored.

### Known limitations

- `JsonlFileSink` audit events are wired with writeable stubs only — full end-to-end audit
  deferred.
- Rate limiting on `/auth/exchange` deferred.
- Granular scopes deferred: flat scopes only (`read`, `write`, `admin`).
- API key auto-rotation deferred.
- ACL filter by `tenant_id` at runtime deferred.
- Worker dispatch and `Vault.read_note` stubs deferred.

### Security

- argon2id: `m=19456 KiB`, `t=2`, `p=1` (OWASP 2023 compliant)
- `OsRng` for secret generation (128 bits effective entropy per key)
- Uniform 401 on `/auth/exchange` (no key enumeration)
- Constant-time argon2id verify (via `argon2` crate)
- API key displayed only once at creation, no re-display
- Revocation store wired at runtime; checked on every exchange call

---

## [0.1.0-alpha.4] — skipped

This version number was reserved but skipped; development proceeded directly to
v0.1.0-alpha.5.

---

## [0.1.0-alpha.3] — 2026-05-05

### Added

- **`gradatum-queue`**: `SqliteQueue` + `Queue` trait; `UPDATE...RETURNING` atomic lease claim.
- **`gradatum-worker`**: leader election via SQLite CAS, dispatcher loop, SIGTERM drain, GC of
  stale leases.
- **3 MCP write handlers** (`vault_write` / `vault_classify` / `vault_downgrade`) with async
  202 response + a job-status poll endpoint.
- **`gradatum-curator`** cascade pipeline — 5 functions:
  - Novelty detection (SHA-256 + MinHash 128)
  - Section routing (regex + Bayesian, 10 sections)
  - Tags (TF-IDF, top 5)
  - Wikilink extraction (Jaro-Winkler 0.88 threshold)
  - Deduplication (cosine 0.95 threshold)
- **5 LLM backends** (protocol-generic):
  `HeuristicBackend` / `OpenAiCompatBackend` / `OllamaCompatBackend` /
  `AnthropicCompatBackend` (ephemeral prompt caching) / `GeminiCompatBackend`
- **`CircuitBreaker<B>`** wrapper: exponential backoff 30→60→120→300 s, `HalfOpen`
  `success_threshold=2`; 7 tests.
- **JSONL audit log**: `HttpAuditEvent` + `JsonlFileSink` with daily rotation, mode 0640,
  content hash (JCS RFC 8785).
- **`gradatum-bench` binary `curator_f1`**: benchmarks curator F1 against a dataset; supports
  `LLM_ENDPOINT` / `LLM_MODEL` env vars.
- **OpenDAL feature gates**: `fs` default + `s3` / `gcs` / `azure` / `all-cloud` opt-in.
- **Systemd packaging**: `gradatum-server.service` (`MemoryMax=512M`) +
  `gradatum-worker.service` (`MemoryMax=1G`, `MemorySwapMax=0`) +
  `sysusers.d/gradatum.conf` (UID 990).
- **TOML curator config**: `[curator] backend = "heuristic"` default + `[curator.llm]`
  opt-in; classifier prompt embedded via `include_str!`.

### Fixed

- `gradatum-curator::routing` regex `\b SECTION \b` broken on `[SECTION]` prefixes — fixed
  with two-layer `PREFIX_PATTERNS` + `KEYWORD_PATTERNS`; 6 tests added.
- `gradatum-bench::curator_f1` raw Markdown body degraded lightweight LLM accuracy — fixed
  with `clean_body_for_llm()` that strips headings, wikilinks, code blocks, and frontmatter.

### Bench results — ALL PASS

Dataset: `gradatum-balanced-v1-final.jsonl` (147 notes / 10 sections).

| Backend | F1 weighted | Threshold | Verdict |
|---|---|---|---|
| **heuristic** (offline default) | **0.7871** | ≥ 0.65 | PASS |
| **Qwen3-4B-Instruct-2507 Q4_K_M** (recommended LLM tier) | **0.7938** | ≥ 0.75 | PASS |
| Qwen3-0.6B-Extract (indicative, unoptimised prompt) | 0.4443 | — | note |

Strong sections (heuristic): decisions 0.983 / lessons-learned 1.000 / experiments 1.000 /
feedback 1.000.

The LLM tier is an operator TOML option — default is `[curator] backend = "heuristic"`
(zero LLM, offline-first). Minimum recommended LLM tier: `Qwen3-4B-Instruct-2507 Q4_K_M`
(~2.5 GB binary, ~4 GB VRAM, F1 0.7938 measured).

### Drop-in compatibility (legacy vault v1.6.2)

- Wire/protocol: MCP tool names + REST endpoints `/api/v1/vault_*` (10 read + 3 write) — compatible
- DTO/shape: identical JSON fields + optional additive `tenant_id` — compatible
- Auth/ACL: same Ed25519 bearer JWT format, audience-scoped, deny-wins ACL — compatible
- Data content: empty stubs — full parity deferred to a future release
- Search/curator semantics: may diverge intentionally (gradatum is a new release, not a port)

---

## [0.1.0-alpha.2] — 2026-05-05

### Added

- **`gradatum-server`**: HTTP/MCP facade (Axum + figment + JSON tracing + 30 s SIGTERM drain)
- **`ServerConfig::validate_bind_tls()`**: fail-closed TLS configuration validation (5 cases)
- **`gradatum-core::TrustContext`**: mandatory enum propagated through all API handlers
- **`gradatum-auth::RevocationStore`** trait + `InMemory` + SQLite implementations + boot guard
- **JWT Ed25519** with scope-based TTL (1 h human / 24 h service)
- **`gradatum-acl-policy::AclEngine`**: deny-wins ACL (12 gold cases)
- **`gradatum-admin init`** CLI: auto-generates Ed25519 keys, bearer token, `bearer.toml`,
  and `server.toml` with defaults
- **10 MCP read endpoints**: drop-in API parity with legacy vault v1.6.2
- **`gradatum-mcp-stub`**: stdio → HTTP bridge + real JWT middleware
- **`/health`** endpoint (10 fields)
- **`/metrics:19091`** sidechannel with cardinality cap
- Shape parity tests (10 methods + smoke)
- Cross-platform support: Linux primary, Windows secondary tier (RFC-0002 — design note
  retired; the tiered model itself was superseded 2026-06-05 by a Linux-only stance, see
  `CONTRIBUTING.md` § Linux-only platform note)

### Drop-in compatibility (legacy vault v1.6.2)

- Wire/protocol: MCP tool names + REST endpoints `/api/v1/vault_*` — compatible
- DTO/shape: identical JSON fields + optional additive `tenant_id` (default `main`) — compatible
- Auth/ACL: same Ed25519 bearer JWT format, audience-scoped — compatible
- Data content: empty stubs — full parity deferred to a future release
- Search/curator semantics: may diverge intentionally (gradatum is a new release, not a port)

---

## [0.1.0-alpha] — 2026-05-04

Initial alpha release. Establishes the workspace foundation and all Layer 0/1/2 crates.

### Added

- **`gradatum-core`**: canonical types (`Note`, `Frontmatter`, `NoteId` ULID,
  `ContentHash` JCS RFC 8785, `NoteVersion`, `IntegritySignature`, `AuthorRef`, `Tag`,
  `Section`, `NoteStatus` 6-state machine, lazy `ExtraFields`); traits (`Index`, `AclPolicy`,
  `ACLFilter`, `Overridable`, `OverridePayload`); `AuditEvent` typed enum; embedded schema
  registry (4 TOML schemas via `include_dir!`); `GradatumError` taxonomy; `VaultConfig`
  runtime TOML (6 sub-sections: embed / curator / index / drift / audit / vault).
- **`gradatum-markdown`**: parser/writer for `Note` ↔ `String` round-trip + wikilink
  extractor regex.
- **`gradatum-cache`**: `EffectiveNoteCache` (moka LRU) with checksum validation on hit —
  zero stale-read risk under concurrency.
- **`gradatum-queue`**: SQLite job queue with `UPDATE...RETURNING` atomic claim, 5-minute
  lease, and 4 SQLite PRAGMAs (WAL, `synchronous=NORMAL`, `busy_timeout=5000`,
  `foreign_keys=ON`).
- **`gradatum-chat`**: `Chat` trait + 3 impls (`HeuristicBackend` offline, `HttpChat`
  OpenAI-compat, `Noop`) + `CircuitBreakerChat<C>` decorator (3 consecutive failures →
  5-minute cooldown).
- **`gradatum-embed`**: `Embedder` trait + 3 impls (`FastEmbedCpu` feature-gated,
  `HttpEmbedder`, `Noop`) + `FallbackEmbedder<P, F>` decorator.
- **`gradatum-index`**: `SqliteIndex` implementing the `Index` trait; FTS5 unicode61;
  complete schema (notes, audit_trail, note_index, generic note_overrides, file_checksums,
  history scaffold); three-level drift detection (size → prefix 4 KB → full SHA-256).
- **`gradatum-storage`**: `Storage` trait (OpenDAL) + `FileStorage` backend + NFS reject
  via `statfs`.
- **`gradatum-vault`**: registry + lifecycle (`write_note`: compose → persist → upsert
  index); `NoteMetadataOverride`; drift orchestration; `effective_note` cache.
- **`gradatum-curator`**: heuristic gating workflow + LLM review for low-confidence notes
  via `Chat` trait; 3 fallback strategies (`PendingReviewFallback` default / `Reject` /
  `AdmitPendingReview`).
- **`v1-parity-tests`**: 22 integration baseline tests (vault_crud, curator_workflow,
  drift_e2e, cache_concurrency, index_search, audit_trail, markdown_roundtrip,
  persistence_reopen).
- **`gradatum-bench`**: 9 active Criterion benches + 1 feature-gated + 2 standalone
  scripts. JCS hash baseline: 5.23 µs @ 10 KB.
- Workspace: pinned `=X.Y.Z` deps, `CHANGELOG.md`, `CONTRIBUTING.md`, `deny.toml` graph
  rule.

### Known limitations

- Unknown YAML keys in `ExtraFields` are silently dropped (`serde_yaml` without
  `#[serde(flatten)]`); deferred to a future release.
- `FastEmbedCpu` feature-gated (`fastembed-cpu`, off by default) due to an upstream
  `ort-sys` build script issue. Activatable via `cargo --features fastembed-cpu`.
- `Vault::read_note` and `update_status` return `NoteNotFound` (stubs); deferred to a
  future release.

---

## Past versions

- `0.1.0-scaffold` (2026-05-01) — initial workspace scaffolding.
- `0.1.0-phase0bis` (2026-05-03) — Phase 0bis re-structuring 17 -> 22 focused crates + RFC-0001 + CI enriched.
