# Gradatum — Dépendances

> Arbre de dépendances Cargo. Établi par lecture des `Cargo.toml` du workspace (source de
> vérité) ; à recouper avec `cargo tree --workspace --depth 1`.
> Version workspace `2.1.1`, edition 2024, MSRV 1.91, `resolver = "3"`.
> MAJ 2026-09-02 (`6a7515a5..3a981130`) : aucune dépendance externe ajoutée, retirée ni bumpée —
> le delta `Cargo.lock` (32/32) est la propagation des 34 bornes intra-famille à `2.1.1`.
> MAJ 2026-08-23 (delta `c942cb15..687b7ac5`, 9 commits sur les manifestes — paliers 2.0.2→2.0.9) :
> **aucune dépendance ajoutée, retirée ni repinnée**. Seul changement réel de l'arbre : la crate
> `nix` gagne la feature `user` (`fs, signal, process` → `fs, signal, process, user`). Le reste des
> diffs de manifeste n'est que la version du workspace `2.0.1` → `2.0.9`. Vérifié par diff des
> `Cargo.toml`, pas supposé.
>
> MAJ 2026-08-18 : version workspace `2.0.1` → **`2.0.4`**. Deux dépendances **transitives**
> ont bougé le même jour et ne figurent pas dans l'arbre ci-dessous, qui n'énumère que les
> dépendances directes du workspace : `h2` `0.4.14` → `0.4.16` (levée de RUSTSEC-2026-0258,
> chemin de requête du serveur) et `spin` `0.9.8` retiré → `0.9.9`. **Aucune dépendance directe
> ajoutée, retirée ni repinnée** ; le verrou `links=sqlite3` est intact. Arbre non revérifié
> dans cette passe.
> MAJ 2026-08-12 : colonne « Version » de la table des pins régénérée depuis le manifeste
> (30 cellules divergaient — `ulid` documenté `1.2.1` pour un réel `3.0.0`, idem `criterion`,
> `prometheus`, `insta`, `tower-http`, `schemars`, `prometheus-client`, `fastembed`…). La
> justification `prometheus` (« 0.14 breaking connu ») était devenue auto-contradictoire (pin réel
> `0.14.0`) → neutralisée. Commande de régénération documentée en § *Mise à jour*. Les blocs
> datés ci-dessous portent des chiffres exacts à leur date : **non modifiés**.
> MAJ 2026-08-10 : version workspace corrigée `1.0.2` → `2.0.0` (alignement documentaire seul —
> le graphe de dépendances n'a pas été revérifié dans cette passe, voir l'entrée Updated
> 2026-08-04 ci-dessous pour le dernier état vérifié).
> MAJ 2026-08-10 (2) : `gradatum-mcp-stub` bascule `publish = false` (retrait de la distribution
> `2.0.0`, source conservée — voir `ARCHITECTURE.md` § API surface topology). Décompte publiables
> **27 → 26** ; groupe *Clients* **3 → 2** ; groupe *Non publiables* **4 → 5**. Le crate gagne
> `clap` (feature `derive`) pour un flag `--version`, vérifié dans son `Cargo.toml`.
> MAJ 2026-08-06 : bump 1.0.2 (author=header au write MCP, fix `04589e0e`) — aucune dépendance ajoutée/retirée.
> Arbres établis au commit `fb0742e5` pour la structure des crates ; le graphe des dépendances
> externes a évolué depuis — voir l'entrée Updated 2026-07-31 ci-dessous pour le changement tracé.
>
> Updated: 2026-08-04 — gradatum-studio passée **publiable** (F-131, commit `2e274bea` : retrait
> `publish = false`, licence composite `Apache-2.0 AND OFL-1.1 AND MIT AND ISC` [bundle `dist/`
> redistribue code + fontes + paquets npm tiers, notices THIRD-PARTY-LICENSES.md],
> categories web-programming/gui, include allow-list sur le bundle Vite). 4 non-publiables
> désormais : `gradatum-cli` (déjà 0.7.6 sur crates.io, republication reportée sans version cible), `gradatum-bench`,
> `index-parity-tests`, `v1-parity-tests`. Structure 31 membres inchangée ; graphe externe inchangé
> depuis l'entrée 2026-07-31 (re-vérifié au commit `761f9625`).
>
> Updated: 2026-07-31 — bump `opendal` `=0.51.0` → `=0.58.1` au commit `6dfdb8f0` (ferme
> RUSTSEC-2026-0194 / -0195 au lieu de les exempter ; relève la MSRV workspace 1.88 → 1.91,
> exigence d'opendal 0.58). Graphe externe changé : `Cargo.lock` +17 paquets net (688 → 705,
> mesuré `git show <ref>:Cargo.lock | grep -c '^name = '` avant/après). `opendal` est éclaté
> depuis sa 0.56 en façade + `opendal-core` + 5 `opendal-service-*` (azblob/azure-common/fs/gcs/s3),
> confirmé `cargo tree -i opendal`. Entrent : `reqsign-*` (5 crates : aws-v4/azure-storage/core/
> file-read-tokio/google, remplace `reqsign` monolithique) + `jiff*` (5 crates : jiff/core/static/
> tzdb/tzdb-platform). Sortent : `backon`, `reqsign` (monolithique), `time`/`time-core`/`time-macros`,
> `quick-xml` 0.36.2 et 0.37.5 (deux versions coexistantes avant, remplacées par une unique 0.41.0).
> Updated: 2026-07-24 — réalignement complet sur le code (`d6c0135a..fb0742e5` : 279 fichiers,
> 26 157 insertions sur `crates/`). Corrections : décompte des crates (22 annoncés → **31 membres
> de workspace, dont 27 publiables**), arbres par crate refaits depuis les `[dependencies]` réels,
> versions externes remplacées par les pins exacts du `[workspace.dependencies]`, table des feature
> flags refaite (l'ancienne listait 6 features dont **aucune** n'existe dans le code). Une seule
> dépendance a réellement bougé sur la période : `gradatum-dto` gagne `gradatum-core` (typage
> `TenantId`/`VaultId` des DTOs) et `bincode` en dev-dep. Graphe externe inchangé.
> Updated: 2026-07-16 — vérifié sans changement de dépendances (train v0.9.0 F-110 P2/F-111/F-112).
> Updated: 2026-07-12 — F-70 qualified method-call resolution (code-only `gradatum-ingest`). Graphe inchangé.
> Updated: 2026-07-11 — bumps 0.7.8 / 0.7.9 (champ version seul) + Cargo.lock only (`fix(deps)`) :
> crossbeam-epoch 0.9.18→0.9.20 (RUSTSEC-2026-0204), quinn-proto 0.11.14→0.11.16 (RUSTSEC-2026-0185).
> Updated: 2026-07-10 — 0.7.7 (champ version seul). Cutover E1 : code seul, zéro impact Cargo.toml.
> Updated: 2026-06-11 — 0.4.6 : nouveaux membres `gradatum-studio` (publish=false), `index-parity-tests`.

---

## Périmètre du workspace

**31 membres** déclarés dans `[workspace] members` (`ls crates/` = 31 répertoires).
**26 sont publiables** ; 5 portent `publish = false` :

| Crate | Raison du `publish = false` |
|---|---|
| `gradatum-cli` | republication reportée, aucune version cible fixée — ⚠️ déjà publiée en 0.7.6 sur crates.io |
| `gradatum-mcp-stub` | **retiré de la distribution `2.0.0`** — plus construit ni publié ; dernière version publiée `1.0.0` sur crates.io ; source conservée, retrait réversible (voir `ARCHITECTURE.md` § API surface topology) |
| `gradatum-bench` | benchmarks internes |
| `index-parity-tests` | suite de parité backend-agnostique |
| `v1-parity-tests` | suite de parité v1 |

> ⚠️ Piège de décompte : **26 = crates publiés sur crates.io**, **31 = membres du workspace**.
> Les deux chiffres sont corrects et ne se substituent pas.

Répartition fonctionnelle des 31 (3 + 1 + 19 + 1 + 2 + 5) :

| Groupe | N | Membres |
|---|---|---|
| Binaires control plane | 3 | `gradatum-server`, `gradatum-worker`, `gradatum-admin` |
| Binaire gateway | 1 | `gradatum-gateway` |
| Bibliothèques data plane | 19 | `core`, `dto`, `markdown`, `vault`, `storage`, `index`, `search`, `queue`, `db-sqlite`, `cache`, `chat`, `curator`, `embed`, `engine`, `ingest`, `warden`, `acl-policy`, `acl-auth`, `auth` (préfixe `gradatum-`) |
| Binaire web UI | 1 | `gradatum-studio` (bundle React+TS servi à `/ui/*`, publiable depuis F-131) |
| Clients | 2 | `gradatum-sdk-rs`, `gradatum` (umbrella) |
| Non publiables | 5 | `gradatum-cli`, `gradatum-mcp-stub`, `gradatum-bench`, `index-parity-tests`, `v1-parity-tests` |

---

## Workspace structure

**3 binaires (control plane)** :

```
gradatum-server         (façade HTTP/MCP stateless)
├── gradatum-core          gradatum-dto           gradatum-vault
├── gradatum-storage       gradatum-index         gradatum-search
├── gradatum-queue         gradatum-db-sqlite     gradatum-cache
├── gradatum-embed         gradatum-chat          gradatum-curator
├── gradatum-acl-policy    gradatum-acl-auth      gradatum-auth
├── gradatum-warden
├── axum + axum-server + rustls + tower + tower-http + http
├── rmcp + schemars                    (serveur MCP natif /mcp)
├── rusqlite + sqlx + sqlite-vec       (index + queue + ANN)
├── figment + toml + clap              (config + CLI)
├── prometheus-client                  (metrics)
└── tokio (+ tokio-stream, tokio-util) + tracing + tracing-subscriber

gradatum-worker         (consommateur de queue asynchrone)
├── gradatum-core          gradatum-dto           gradatum-db-sqlite
├── gradatum-queue         gradatum-curator       gradatum-chat
├── gradatum-embed
├── apalis + apalis-cron + cron        (Monitor + schedules)
├── prometheus                         (exporter :19091)
├── axum + tower                       (surface health/metrics)
├── sqlx                               (QueueStore + payloads)
├── figment + toml + clap + reqwest + secrecy
└── tokio + tracing + tracing-subscriber

gradatum-admin          (CLI ops)
├── gradatum-core          gradatum-dto           gradatum-vault
├── gradatum-storage       gradatum-index         gradatum-queue
├── gradatum-db-sqlite     gradatum-curator
├── gradatum-ingest  (features code-rust, code-python, code-bash, code-typescript)
├── gradatum-acl-policy    gradatum-acl-auth      gradatum-auth
├── clap + toml + toml_edit + walkdir + hex
├── argon2 + rand + ed25519-dalek + pkcs8        (bootstrap auth/token)
├── rusqlite + sqlx + reqwest
└── tokio + tracing + tracing-subscriber + anyhow
```

**1 binaire (gateway)** :

```
gradatum-gateway        (proxy LLM autonome :8436)
├── gradatum-core          gradatum-embed         gradatum-search
├── axum + http + tower + tower-http + reqwest
├── figment + toml + rusqlite
├── secrecy + subtle + bytes + futures + once_cell + ulid + chrono
└── tokio + tracing + tracing-subscriber + anyhow + thiserror
```

**Bibliothèques data plane** :

```
gradatum-core           (primitives partagées : erreurs, ids, scope, sections, jobs, audit)
├── serde + serde_json + serde_jcs
├── chrono + ulid + sha2 + smallvec + http
├── secrecy                (SecretBytes, zeroize-on-drop)
├── include_dir            (schémas embarqués)
├── stability              (annotations #[stability::unstable])
└── async-trait + thiserror + toml + tokio

gradatum-dto            (DTOs contrats wire — source de vérité unique)
├── gradatum-core          ← NOUVEAU 2026-07 : typage TenantId (principal) / VaultId (namespace)
├── serde
└── (feature schemars) schemars + serde_json

gradatum-markdown       (parse/serialize MD + frontmatter + wikilinks)
├── gradatum-core
├── serde + serde_norway   (⚠ backend YAML ; a remplacé serde_yml, archivé + unsound)
└── regex + once_cell + thiserror

gradatum-vault          (registre multi-vault + cycle de vie + swap)
├── gradatum-core + gradatum-markdown + gradatum-cache
├── gradatum-index + gradatum-storage
├── serde + serde_json + toml + chrono + ulid + sha2
└── async-trait + thiserror + tokio + tracing

gradatum-storage        (abstraction FS/objet + garde NFS)
├── gradatum-core
├── opendal                (feature `fs` par défaut ; s3/gcs/azure opt-in)
├── opendal-http-transport-reqwest  (feature `cloud-http`, optional ; transport HTTP objet, `rustls-no-provider`)
└── async-trait + thiserror + tokio + tracing

gradatum-index          (SQLite + FTS5 + migrations idempotentes + drift + ANN)
├── gradatum-core + gradatum-storage
├── rusqlite (bundled — FTS5 natif, 4 PRAGMA C12)
├── (feature sqlite-vec-ann) sqlite-vec
├── serde + serde_json + chrono + ulid + sha2
└── async-trait + thiserror + tokio + tracing

gradatum-search         (lecteur multi-mode + fusion RRF + scoring composite)
├── gradatum-core + gradatum-index + gradatum-cache
├── (feature onnx-reranker) ort + tokenizers
└── tracing

gradatum-queue          (façade GradatumQueue + lease atomique)
├── gradatum-core + gradatum-db-sqlite
├── sqlx + rusqlite
├── serde + serde_json + chrono + ulid
└── async-trait + thiserror + tokio + tracing

gradatum-db-sqlite      (SqliteQueueStore — impl QueueStore)
├── gradatum-core
├── sqlx
├── serde + serde_json + chrono + ulid
└── async-trait + thiserror + tokio + tracing

gradatum-cache          (LRU moka in-process — clé composite (vault_id, note_id, scope_hash))
├── gradatum-core
├── moka
└── tokio

gradatum-chat           (trait Chat + OpenAICompat + Heuristic + Noop)
├── gradatum-core
├── reqwest (rustls-tls par défaut)
├── secrecy + regex + once_cell + chrono
├── (feature windows-native-tls) native-tls via reqwest — OFF par défaut (Windows corporate-proxy/custom-cert rationale, from a since-retired portability RFC)
└── async-trait + serde + serde_json + thiserror + tokio + tracing

gradatum-curator        (curation : filtrage, routage, tagging, audit/dedup)
├── gradatum-core + gradatum-chat + gradatum-index
├── regex + once_cell + strsim + unicode-segmentation
├── secrecy + sha2 + chrono + ulid
└── async-trait + serde + serde_json + thiserror + tokio + tracing

gradatum-embed          (trait Embedder + impl remote/local)
├── gradatum-core
├── reqwest (rustls-tls par défaut)
├── (feature fastembed-cpu) fastembed + ort-sys — OFF par défaut
├── (feature windows-native-tls) native-tls via reqwest — OFF par défaut
└── async-trait + serde + serde_json + thiserror + tokio

gradatum-engine         (superviseur des sous-processus llama-server)
├── gradatum-core
└── (feature serve — porte TOUTES les deps ci-dessous)
    ├── gradatum-dto       (QaEvent pour l'event-log)
    ├── axum + reqwest + prometheus-client
    ├── figment + toml + serde + serde_json + url
    ├── zeroize            (JWT/api-key du sink event-log)
    ├── nix                (signalisation de groupe de processus POSIX)
    └── tokio + tracing + tracing-subscriber + async-trait + chrono + anyhow

gradatum-ingest         (pipeline code-ingest tree-sitter, zéro LLM)
├── gradatum-core + gradatum-index
├── (feature code-rust, défaut)  tree-sitter + tree-sitter-rust
├── (feature code-python)        tree-sitter + tree-sitter-python
├── (feature code-bash)          tree-sitter + tree-sitter-bash
├── (feature code-typescript)    tree-sitter + tree-sitter-typescript
└── sha2 + serde_json + thiserror + tracing

gradatum-warden         (garde réseau L0 — filtre IP, rate-limit, bypass loopback)
├── axum + tower + tower-http + http
├── governor + ipnet + dashmap
└── async-trait + serde + thiserror + tokio + tracing

gradatum-acl-policy     (presets ACL + chargement du modèle de config + matching glob)
├── gradatum-core
├── globset                (⚠ le matching glob vit ICI, pas dans gradatum-acl-auth)
├── serde + toml
└── thiserror + tracing

gradatum-acl-auth       (store de clés d'API + vérification de bearer token)
├── gradatum-core
├── argon2 + rand          (hachage argon2id des clés)
├── sqlx
├── serde + serde_json + ulid
└── async-trait + thiserror + tokio + tracing

gradatum-auth           (auth JWT/OIDC/API-key + validation de token)
├── gradatum-core
├── jsonwebtoken + ed25519-dalek + pkcs8 + zeroize
├── dashmap + sqlx
├── serde + serde_json + chrono + ulid + rand
└── async-trait + thiserror + tokio + tracing
```

**Clients** :

```
gradatum-sdk-rs         (SDK Rust pour intégration directe)
├── gradatum-core
├── reqwest
└── serde + serde_json + tokio

gradatum                (façade SDK parapluie — re-exports feature-gated)
├── (feature client) gradatum-sdk-rs
└── (feature core)   gradatum-core
```

**Non publiables** :

```
gradatum-cli            (CLI utilisateur — publish=false, republication reportée sans version cible, ⚠️ déjà 0.7.6 sur crates.io)
├── gradatum-core
├── reqwest + clap
└── serde + serde_json + tokio

gradatum-mcp-stub       (adapter MCP stdio → HTTP — RETIRÉ de la distribution 2.0.0, publish=false, source conservée)
├── gradatum-core + gradatum-dto
├── rmcp + schemars
├── reqwest + clap
└── tokio + tracing + tracing-subscriber + anyhow + async-trait + serde + serde_json

index-parity-tests      (suite de parité backend-agnostique — dev-deps uniquement)
v1-parity-tests         (suite de parité v1 — dev-deps uniquement)
gradatum-bench          (criterion + core/cache/chat/curator/embed/index/storage)
```

---

## Dépendances externes principales

Les dépendances du workspace sont **épinglées à l'exact** (`=x.y.z`) dans le
`[workspace.dependencies]` racine. Les versions ci-dessous en sont la lecture directe.
Exceptions non épinglées, volontaires : `stability 0.2`, `subtle 2` (workspace) ainsi que
`ipnet 2`, `bytes 1` et `tokenizers 0.21` (déclarées au niveau crate).

| Crate | Version | Usage | Justification |
|---|---|---|---|
| `tokio` | `=1.53.0` | Runtime async | Standard Rust async |
| `axum` | `=0.8.9` | Serveur HTTP | Léger, performant, ergonomique |
| `axum-server` | `=0.8.0` | Terminaison TLS native (`[server.tls]`, B-2) | `tls-rustls-no-provider` : provider crypto explicite |
| `rustls` | `=0.23.40` | Provider crypto process-default | `aws_lc_rs` installé au boot — évite un second provider `ring` ambigu |
| `tower` / `tower-http` | `=0.5.3` / `=0.7.0` | Middleware | Compose avec axum (`cors`, `trace`, `fs`, `set-header`, `limit`) |
| `http` | `=1.4.2` | Types HTTP partagés | Contrat commun serveur/warden/gateway |
| `rmcp` | `=1.6.0` | Serveur MCP natif | Lib Rust MCP officielle, pinnée (`transport-streamable-http-server`) |
| `schemars` | `=1.2.1` | Schémas JSON des outils MCP | Dérivation auto des schémas d'outils |
| `rusqlite` | `=0.32.1` (bundled) | SQLite + FTS5 | Embarqué, multi-feature |
| `sqlx` | `=0.8.6` | QueueStore + auth + acl-auth | WAL + `UPDATE … RETURNING` atomique |
| `sqlite-vec` | `=0.1.9` | Index ANN vec0 | `default-features = false` : évite un rusqlite ^0.31 conflictuel |
| `globset` | `=0.4.19` | Matching de patterns ACL | Lib pure, simple |
| `argon2` | `=0.5.3` | Hachage de clés d'API (OWASP) | Recommandation standard |
| `moka` | `=0.12.15` | Cache LRU | Performant, thread-safe |
| `reqwest` | `=0.13.4` (rustls) | Client HTTP | Standard, sans OpenSSL |
| `opendal` | `=0.58.1` | Abstraction de stockage (façade `opendal-core` + `opendal-service-*` depuis 0.56) | `fs` par défaut ; s3/gcs/azure derrière features |
| `opendal-http-transport-reqwest` | `=0.58.1` | Transport HTTP enfichable d'OpenDAL 0.58 (apache/opendal#7900) — requis par les backends objet (S3/GCS/Azure) : sans lui, toute opération objet échoue avant le premier paquet réseau (`ConfigInvalid`). Installé au démarrage par `gradatum-storage` (`install_default()`). | `default-features = false`, feature `rustls-no-provider` (aucun provider crypto embarqué ; lit le provider process-default `aws_lc_rs`). Direct sur `gradatum-storage`, `optional`, feature `cloud-http` |
| `serde` / `serde_json` / `serde_jcs` / `serde_norway` / `toml` | `=1.0.229` / `=1.0.150` / `=0.1.0` / `=0.9.42` / `=1.1.3` | Sérialisation | Frontmatter MD, JSON-RPC, JCS RFC 8785, configs |
| `toml_edit` | `=0.25.13` | Édition TOML préservant le format | Écriture de config par la CLI |
| `figment` | `=0.10.19` | Chargement de config | Fusion fichier + env |
| `tracing` / `tracing-subscriber` | `=0.1.44` / `=0.3.23` | Logs structurés | Sortie JSON prête pour SIEM |
| `clap` | `=4.6.2` | Parsing CLI | Derive standard |
| `ulid` | `=3.0.0` | IDs de notes stables | Triable lexicographiquement, ordonné dans le temps |
| `chrono` | `=0.4.45` | Dates ISO 8601 | Standard |
| `walkdir` | `=2.5.0` | Scan FS | Reindex / migrate (`gradatum-admin`) |
| `regex` / `once_cell` | `=1.13.1` / `=1.21.4` | Parsing wikilinks, statiques paresseux | Standard |
| `thiserror` / `anyhow` | `=2.0.19` / `=1.0.104` | Gestion d'erreurs | `anyhow` ≥ 1.0.103 = fix RUSTSEC-2026-0190 |
| `sha2` | `=0.11.0` | Hachage (drift, content_hash JCS) | — |
| `secrecy` / `zeroize` / `subtle` | `=0.10.3` / `=1.9.0` / `2` | Secrets zeroize-on-drop, comparaison constante | Credentials LLM + JWT |
| `ed25519-dalek` / `pkcs8` | `=2.1.1` / `=0.10.2` | Signature JWT EdDSA | Clé Ed25519 persistée chmod 600 |
| `jsonwebtoken` | `=9.3.1` | Encodage/décodage JWT | ⚠ **maintenu en 9.x** : 10.x exige la feature `rust_crypto` (`sha2 ^0.10.7`), incompatible avec `sha2 =0.11.0` |
| `governor` / `ipnet` / `dashmap` | `=0.10.4` / `2` / `=6.2.1` | Rate-limit, CIDR, map concurrente | `gradatum-warden`, `gradatum-auth` |
| `nix` | `=0.31.3` | Syscalls UNIX (`fs`, `signal`, `process`) | Signalisation de groupe de processus (`gradatum-engine`) |
| `apalis` | `=1.0.0-rc.9` | Framework de job queue (Monitor multi-worker + layers Timeout/Retry/CatchPanic/LoadShed) | Framework Rust type-safe, crate embarquée à la compilation (pas un service runtime — ARCH-D15 F-24). Pin exact D-09 + caveat C1 RC9→v1.0 |
| `apalis-cron` / `cron` | `=1.0.0-rc.8` / `=0.16.0` | Schedules périodiques | rc.9 non publié pour `apalis-cron` |
| `prometheus` | `=0.14.0` | Exporter worker (:19091) | Pin exact minor level |
| `prometheus-client` | `=0.25.0` | Metrics serveur + engine | — |
| `ort` / `tokenizers` | `=2.0.0-rc.9` / `0.21` | Reranker cross-encoder ONNX | Optionnels (`onnx-reranker`) |
| `fastembed` / `ort-sys` | `=4.9.1` / `=2.0.0-rc.9` | Embeddings CPU locaux | Optionnels (`fastembed-cpu`) ; `ort-sys` pinné rc.9 (rc.12 tire ureq v3) |
| `tree-sitter` (+ `-rust`, `-python`, `-bash`, `-typescript`) | `=0.26.9` (+ `=0.24.2`, `=0.25.0`, `=0.25.1`, `=0.23.2`) | Parsing code-ingest | Optionnels, une feature par langage |
| `stability` | `0.2` | Annotations `#[stability::unstable]` | Seule dépendance non épinglée à l'exact |
| `include_dir` / `smallvec` / `strsim` / `unicode-segmentation` / `hex` / `bytes` / `url` / `futures` | `=0.7.4` / `=1.15.2` / `=0.11.1` / `=1.13.3` / `=0.4.3` / `1` / `=2.5.8` / `=0.3.33` | Utilitaires | — |
| `tempfile` / `proptest` / `serde_test` / `wiremock` / `mockito` / `insta` / `criterion` | `=3.27.0` / `=1.11.0` / `=1.0.177` / `=0.6.5` / `=1.7.2` / `=1.48.0` / `=0.8.2` | Tests + benchs (dev) | Standard |

> ❗ `tantivy` a été retiré de cette table : il n'a jamais été une dépendance du workspace
> (aucune occurrence dans les `Cargo.toml`). La ligne « à fixer Phase 3 » était une intention,
> pas un état.
>
> ❗ `apalis-sql` / `apalis-sqlite` retirées de cette table (2026-08-11) : plus aucune occurrence
> dans les `Cargo.toml` du workspace ni dans `Cargo.lock` (`cargo metadata` résout uniquement
> `apalis`, `apalis-core` [transitif], `apalis-cron`). La description « Backend SQLite réel (via
> sqlx 0.8) » d'`apalis-sqlite` décrivait une architecture qui n'existe pas — ne pas la reconduire.
>
> ❗ `bincode` retiré de cette table (2026-08-11) : supprimé du dev-dep `gradatum-dto` et des deps
> `gradatum-worker`/`gradatum-queue` par le commit `48bc72c1` (2026-08-09, suppression de l'ancien
> moteur de travaux). Ni `Cargo.toml` ni `Cargo.lock` n'en portent plus trace — un `grep -rn
> bincode crates/*/Cargo.toml` est vide. La ligne 40 ci-dessus (entrée changelog 2026-07-24) est
> laissée intacte : elle décrivait un état exact à cette date, antérieur au retrait.

### Crate workspace `gradatum-db-sqlite`

Crate dédié : `SqliteQueueStore` implémente le trait `QueueStore` de `gradatum-core`
(15 méthodes : enqueue / dequeue / get / complete / fail / cancel / fail_dlq / find_awaiting /
set_pending / recover_stale_leases / cancel_expired_deadlines / promote_retries / schedule_retry /
list / subscribe). Schéma custom `gradatum_jobs` (id TEXT ULID + payload JSON + status + priority
+ class + timestamps + lease_until + attempt_count + deadline + last_error + await_jobs + `kind`
dénormalisé + `tenant_id`).

Migrations (`crates/gradatum-db-sqlite/migrations/`) : `006_apalis_bootstrap` ·
`007_jobs_kind_indexed` · `008_idempotency` · `009_jobs_v2_drain` · `010_backfill_kind` ·
**`011_jobs_tenant_scope`** (colonne `tenant_id NOT NULL DEFAULT 'main'` + index — isolation des
jobs par tenant, filtrage conditionnel : absence de clause à OFF, `AND tenant_id = ?` à ON).

Pattern F-24 agnostique : trait `QueueStore` dans `gradatum-core`, impl `SqliteQueueStore`,
futur Postgres/libsql/LanceDB sans casser la couche worker Apalis.

> À ne pas confondre avec les migrations de l'**index** (`crates/gradatum-index/migrations/`,
> numérotées `0001` → `0039`), documentées dans [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Dépendances optionnelles (feature flags)

Table établie par lecture des blocs `[features]` de chaque `Cargo.toml`.

| Crate | Feature | Défaut | Active |
|---|---|---|---|
| `gradatum-ingest` | `code-rust` | ✅ | `tree-sitter` + `tree-sitter-rust` |
| `gradatum-ingest` | `code-python` | — | `tree-sitter` + `tree-sitter-python` |
| `gradatum-ingest` | `code-bash` | — | `tree-sitter` + `tree-sitter-bash` |
| `gradatum-ingest` | `code-typescript` | — | `tree-sitter` + `tree-sitter-typescript` |
| `gradatum-storage` | `fs` | ✅ | `opendal/services-fs` |
| `gradatum-storage` | `s3` / `gcs` / `azure` / `all-cloud` | — | backends objet OpenDAL correspondants |
| `gradatum-index` | `sqlite-vec-ann` | — | `sqlite-vec` (index ANN vec0) |
| `gradatum-search` | `onnx-reranker` | — | `ort` + `tokenizers` (cross-encoder) |
| `gradatum-embed` | `fastembed-cpu` | — | `fastembed` + `ort-sys` (embeddings CPU locaux) |
| `gradatum-embed` / `gradatum-chat` | `windows-native-tls` | — | `reqwest/native-tls` (Windows-only feature, vestigial since the project went Linux-only) |
| `gradatum-bench` / `gradatum-gateway` | `fastembed-cpu` | — | propage `gradatum-embed/fastembed-cpu` |
| `gradatum-engine` | `serve` | — | toute la surface serveur du superviseur (axum, reqwest, figment, nix, …) |
| `gradatum-engine` | `test-utils` | — | implique `serve` |
| `gradatum-dto` | `schemars` | — | `schemars` + `serde_json` (schémas d'outils MCP) |
| `gradatum-core` | `test-utils` | — | helpers de test |
| `gradatum` (umbrella) | `core` | ✅ | `gradatum-core` (défaut : sinon la façade n'expose que `VERSION`) |
| `gradatum` (umbrella) | `client` | — | `gradatum-sdk-rs` (placeholder sans surface cliente) |

> ❗ L'ancienne table listait `local-encoder`, `reranker`, `tantivy-index`, `sqlite-vec`,
> `prometheus` et `tokio-console`. **Aucune de ces six features n'existe dans le code.**
> Les noms réels sont `fastembed-cpu`, `onnx-reranker` et `sqlite-vec-ann` ; l'export
> Prometheus n'est pas derrière un feature flag.

---

## Externes (services réseau, pas crates)

| Service | Usage | Dépendance core ? |
|---|---|---|
| Gateway compatible OpenAI | Curator LLM + embeddings | NON (R1 single-source-of-LLM-auth — pluggable, pas hardcodé) |
| Litestream | Backup continu de la DB | NON (défini par l'opérateur) |
| SIEM / sink de logs d'audit | Ingestion des logs d'audit | NON (défini par l'opérateur) |
| Système de notification | Alertes ops | NON (défini par l'opérateur) |

→ **Aucun service externe requis** dans le code core. Tout est pluggable ou optionnel.

---

## Mise à jour

Régénérer après `cargo build --workspace` :

```bash
cd /path/to/gradatum
cargo tree --workspace --depth 1 > /tmp/cargo-tree.txt
# Comparer avec ce fichier, mettre à jour si divergence.
```

**Régénérer la colonne « Version » de la table des pins** (source = manifeste, pas
saisie manuelle). La commande ci-dessous produit la liste autoritative
`nom = pin` du `[workspace.dependencies]` racine plus les pins déclarés au niveau
crate (`hex`/`bytes`/`url`/`tokenizers`/`cron`/`ipnet`/`tree-sitter*`). La table doit
en être la lecture directe ; toute cellule qui en diverge est un bug documentaire :

```bash
cd /path/to/gradatum
{ awk '/^\[workspace.dependencies\]/{s=1;next} /^\[/{s=0} s' Cargo.toml
  grep -hE '^[a-zA-Z0-9_-]+ *= *(\{[^}]*version *=|")' crates/*/Cargo.toml
} | sed -E 's/#.*//' \
  | grep -oE '^[a-zA-Z0-9_-]+ *= *(\{[^}]*version *= *"[^"]+"|"[^"]+")' \
  | sed -E 's/ *= *\{[^}]*version *= *"/ = "/; s/([a-zA-Z0-9_-]+) *= *"([^"]+)".*/\1 = \2/' \
  | sort -u
# Diffe cette sortie contre les cellules Version de la table ci-dessus.
```

Contrôles rapides utiles :

```bash
ls crates/ | wc -l                                   # nombre de membres du workspace
grep -L 'publish = false' crates/*/Cargo.toml | wc -l  # nombre de crates publiables
git diff <ref>..HEAD -- Cargo.toml 'crates/*/Cargo.toml'  # tout changement de dépendance
```

---

*Document maintenu par les mainteneurs Gradatum. Dernière mise à jour : 2026-08-04 — réalignement
arbres workspace / table de répartition (studio publiable F-131, `gradatum-cli` non-publiable), déjà
committé `1a7d01c8`. Précédent : 2026-07-31 — bump `opendal` `=0.51.0` → `=0.58.1` (MSRV workspace
1.88 → 1.91, `Cargo.lock` 688 → 705 paquets). Réalignement complet précédent : 2026-07-24, `fb0742e5`
(décompte des crates, arbres par crate, pins exacts, feature flags réels, migration queue 011).
Arbre initial : 2026-05-01.*
