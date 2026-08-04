# v1-parity-tests

Harness d'intégration pour valider la parité fonctionnelle entre
`gradatum-server` (Phase 2.0a) et the legacy vault v1.6.2 (référence D5).

## Objectif

Deux suites de tests coexistent dans ce crate :

1. **Phase 1 — Tests internes** (`vault_crud.rs`, `curator_workflow.rs`, etc.) :
   tests d'intégration contre les libs gradatum (gradatum-vault, gradatum-index…).
   Lancés sans flag `--ignored` dans la CI normale.

2. **Phase 2 — Harness parité MCP** (`api_v1_parity.rs`) :
   10 tests `shape_vault_*` + 1 test `smoke_all_10_methods_reachable` qui valident la
   **forme** des réponses HTTP de `gradatum-server`. Ils tournent dans la CI normale, ne
   sont pas `#[ignore]`, et sont **hermétiques** : chacun démarre un `gradatum-server`
   éphémère en process. La comparaison de *contenu* avec le prédécesseur est différée
   (voir « Phase 2.1 » plus bas).

## Prérequis

Aucun prérequis externe : ni binaire du prédécesseur, ni snapshot DB, ni serveur à démarrer
à la main. `cargo test -p v1-parity-tests` suffit.

Les constantes `LEGACY_VAULT_TEST_PORT` (18462), `GRADATUM_TEST_PORT` (19190) et
`SNAPSHOT_DB_PATH` existent dans `api_v1_parity.rs` mais portent toutes `#[allow(dead_code)]` :
elles sont réservées à la Phase 2.1 et **ne sont lues par aucun test actuel**. Les tests de
forme s'appuient sur `spawn_test_server()`, qui écoute sur un port éphémère attribué par
l'OS.

Quatre tests du crate portent réellement `#[ignore]`, et aucun n'est dans `api_v1_parity.rs` :
deux dans `drift_e2e.rs`, un dans `write_synthetic.rs`, un dans `markdown_roundtrip.rs`.

## Exécution

### Suite complète (CI normale)

```bash
cargo test -p v1-parity-tests
```

### Tests différés uniquement

```bash
# N'exécute QUE les 4 tests #[ignore] listés ci-dessus.
# Ne lance aucun test de parité : ceux-ci tournent déjà dans la commande précédente.
cargo test -p v1-parity-tests -- --ignored
```

### Test individuel

```bash
cargo test -p v1-parity-tests shape_vault_search
```

## Snapshot DB regeneration

Le snapshot `tests/fixtures/legacy-vault-snapshot.db` n'est **pas committé** dans le repo.

**Raison** : il contient les notes réelles du vault personnel du mainteneur
(legacy vault v1.6.2, ~337 notes). Le repo `gradatum` est public — commiter ces données
constituerait une fuite de données personnelles.

**Pour régénérer :**
```bash
bash crates/v1-parity-tests/scripts/regenerate-snapshot.sh
```

Le script utilise `sqlite3 .backup` (API Online Backup — safe si le vault source tourne).
Prérequis : accès à `~/.memory-vault/.vault-index/vault.db` sur la machine de développement.

**Override du chemin source :**
```bash
VAULT_DB=/chemin/vers/vault.db bash crates/v1-parity-tests/scripts/regenerate-snapshot.sh
```

**En CI :** le snapshot n'est requis par aucun test actuel, donc rien à régénérer pour
faire passer la CI. La régénération est une étape manuelle et conditionnelle, effectuée par
un mainteneur le jour où les tests de parité de contenu (Phase 2.1) seront activés.

> Note : le script lui-même n'est pas testé en CI (il est utilitaire, pas du code
> applicatif). Son exécution est vérifiée manuellement lors de chaque régénération
> de snapshot.

**État de l'historique Git :** le snapshot n'a **jamais** été commité. `git log --all --
crates/v1-parity-tests/tests/fixtures/legacy-vault-snapshot.db` ne rend aucun commit, et le
`.gitignore` du répertoire de fixtures est en place depuis l'origine. Aucune réécriture
d'historique (`git filter-repo`) n'est nécessaire avant la publication.

## Structure des fixtures

```
tests/fixtures/
├── .gitignore                   # Empêche *.db d'être commité
└── legacy-vault-snapshot.db     # NON COMMITÉ — régénérer via ./scripts/regenerate-snapshot.sh
                                 # Capturé via sqlite3 .backup (online-safe)
                                 # Tables : notes, note_links, note_tags, note_embeddings,
                                 #          note_pagerank, authors, notes_fts*
```

## Méthodes MCP testées (10 read)

| Test                    | Méthode MCP       | Description |
|-------------------------|-------------------|-------------|
| `shape_vault_search`    | `vault_search`    | Recherche FTS + sémantique |
| `shape_vault_read`      | `vault_read`      | Lecture note par slug/ID |
| `shape_vault_list`      | `vault_list`      | Liste paginée par section |
| `shape_vault_status`    | `vault_status`    | Métriques globales |
| `shape_vault_graph`     | `vault_graph`     | Graphe de liens + PageRank |
| `shape_vault_authors`   | `vault_authors`   | Auteurs + statistiques |
| `shape_vault_tags`      | `vault_tags`      | Tags + fréquences |
| `shape_vault_trace`     | `vault_trace`     | Wikilinks entrants/sortants |
| `shape_vault_context`   | `vault_context`   | Notes similaires (cosinus) |
| `shape_vault_links`     | `vault_links`     | Liens inter-sections |

## Phase 2.0a : shape parity (T12 — Option α)

Les 10 tests `shape_vault_*` + 1 test `smoke_all_10_methods_reachable` vérifient :

- **Authentification de bout en bout** : `JwtService::new_ephemeral()` génère une clé Ed25519
  locale au test ; le bearer est signé et vérifié par le même service (`auth_middleware` réel).
- **ACL fonctionnelle** : consumer `"test-bearer"` avec `read_patterns = ["**"]` configuré
  via `AppState::with_jwt_and_acl` — le handler reçoit `AclDecision::Allow`.
- **Shape DTO conforme** : chaque réponse est parsée en JSON et les champs clés du DTO
  Rust sont vérifiés (présence + type).
- **Comportement stub documenté** : `vault_read` → 404 (stub T8, aucun vault réel câblé).

Ces tests remplacent les 10 stubs `#[ignore]` du scaffold PR-3 (qui visaient le diff
content nul, non réalisable avec les stubs T8 + schémas DB incompatibles).

**Arbitrage du 2026-05-05** : Option α retenue. Parité contenu stricte (diff JSON nul) = Phase 2.1.

## Phase 2.1 : full content parity (planifié)

Avec `migrate-from-v0` (import legacy vault v1.6.2 → gradatum-storage) :

- Les 10 tests de contenu compareront les réponses HTTP de `gradatum-server` vs
  legacy vault v1.6.2 sur les mêmes données (snapshot DB).
- Le helper `diff_json_strip_tenant(a, b)` (conservé avec `#[allow(dead_code)]` dans
  `api_v1_parity.rs`) sera activé.
- Les champs ignorés lors de la comparaison : `tenant_id`, `created_at_ms`,
  `updated_at_ms`, `_gradatum_*`.

Reference : design spec P2.0 — 2026-05-04.
