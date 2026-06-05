# v1-parity-tests

Harness d'intégration pour valider la parité fonctionnelle entre
`gradatum-server` (Phase 2.0a) et the legacy vault v1.6.2 (référence D5).

## Objectif

Deux suites de tests coexistent dans ce crate :

1. **Phase 1 — Tests internes** (`vault_crud.rs`, `curator_workflow.rs`, etc.) :
   tests d'intégration contre les libs gradatum (gradatum-vault, gradatum-index…).
   Lancés sans flag `--ignored` dans la CI normale.

2. **Phase 2 — Harness parité MCP** (`api_v1_parity.rs`) :
   10 tests comparant les réponses HTTP de `gradatum-server` vs the legacy vault v1.6.2
   sur les mêmes données (snapshot DB). Marqués `#[ignore]` — lancés manuellement.

## Prérequis (harness parité)

- Legacy vault v1.6.2 binary accessible dans `PATH`
  ```bash
  which legacy-vault && legacy-vault --version
  # attendu : legacy-vault 1.6.2
  ```

- `gradatum-server` compilé
  ```bash
  cargo build -p gradatum-server
  # binaire : target/debug/gradatum-server
  ```

- Snapshot DB présente dans `tests/fixtures/legacy-vault-snapshot.db`
  Le snapshot n'est **pas committé** (`.gitignored`, confidentialité des notes personnelles).
  Voir la section [Snapshot DB regeneration](#snapshot-db-regeneration) ci-dessous.

## Exécution

### Tests Phase 1 (CI normale)

```bash
cargo test -p v1-parity-tests
```

### Tests harness parité (manuel — Phase 2.0a)

```bash
# Lance les 10 tests de parité MCP (ignorés par défaut)
cargo test -p v1-parity-tests -- --ignored
```

Les tests lanceront automatiquement les deux serveurs sur leurs ports de test :
- Legacy vault v1.6.2 : `http://127.0.0.1:18462`
- `gradatum-server` : `http://127.0.0.1:19190`

### Test individuel

```bash
cargo test -p v1-parity-tests parity_vault_search -- --ignored
```

## Snapshot DB regeneration

Le snapshot `tests/fixtures/legacy-vault-snapshot.db` n'est **pas committé** dans le repo.

**Raison** : il contient les notes réelles du vault personnel du mainteneur
(legacy vault v1.6.2, ~337 notes). Le repo `gradatum` deviendra public au tag `v1.0` —
commiter ces données constituerait une fuite de données personnelles.

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

**En CI (Forgejo Actions self-hosted) :**
Le runner self-hosted a accès au vault local. Un step de régénération est intégré dans
`.forgejo/workflows/ci.yml` pour le job `parity` (runner `gradatum-ci`). Cependant,
les tests parité sont marqués `#[ignore]` et ne sont pas exécutés dans la CI normale —
le step de régénération est donc également conditionnel.

> Note : Le script lui-même n'est pas testé en CI (il est utilitaire, pas du code
> applicatif). Son exécution est vérifiée manuellement lors de chaque régénération
> de snapshot.

**Purge de l'historique Git avant v1.0 (OBLIGATOIRE) :**
Le snapshot a été committé par erreur dans le commit `eb933db` (PR-3, 2026-05-05).
Ce fichier reste accessible via `git log -- crates/v1-parity-tests/tests/fixtures/legacy-vault-snapshot.db`.
**Avant le tag `v1.0` public**, il faut purger l'historique avec `git filter-repo` :
```bash
git filter-repo --path crates/v1-parity-tests/tests/fixtures/legacy-vault-snapshot.db --invert-paths
# Suivi d'un force-push coordonné sur Forgejo + GitHub mirror
```
Ce TODO est suivi dans `memory/TODO.md` sous `[gradatum] Purge snapshot DB historique`.

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

| Test                     | Méthode MCP       | Description |
|--------------------------|-------------------|-------------|
| `parity_vault_search`    | `vault_search`    | Recherche FTS + sémantique |
| `parity_vault_read`      | `vault_read`      | Lecture note par slug/ID |
| `parity_vault_list`      | `vault_list`      | Liste paginée par section |
| `parity_vault_status`    | `vault_status`    | Métriques globales |
| `parity_vault_graph`     | `vault_graph`     | Graphe de liens + PageRank |
| `parity_vault_authors`   | `vault_authors`   | Auteurs + statistiques |
| `parity_vault_tags`      | `vault_tags`      | Tags + fréquences |
| `parity_vault_trace`     | `vault_trace`     | Wikilinks entrants/sortants |
| `parity_vault_context`   | `vault_context`   | Notes similaires (cosinus) |
| `parity_vault_links`     | `vault_links`     | Liens inter-sections |

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

**Arbitrage the maintainer 2026-05-05** (Option α) : "le but est de faire mieux pas reprendre iso
the legacy vault poc, gradatum la release". Parité contenu stricte (diff JSON nul) = Phase 2.1.

## Phase 2.1 : full content parity (planifié)

Avec `migrate-from-v0` (import legacy vault v1.6.2 → gradatum-storage) :

- Les 10 tests de contenu compareront les réponses HTTP de `gradatum-server` vs
  legacy vault v1.6.2 sur les mêmes données (snapshot DB).
- Le helper `diff_json_strip_tenant(a, b)` (conservé avec `#[allow(dead_code)]` dans
  `api_v1_parity.rs`) sera activé.
- Les champs ignorés lors de la comparaison : `tenant_id`, `created_at_ms`,
  `updated_at_ms`, `_gradatum_*`.

Reference : design spec P2.0 — 2026-05-04.
