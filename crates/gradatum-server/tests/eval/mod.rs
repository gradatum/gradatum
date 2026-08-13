//! Harness d'évaluation — preuve d'apport RRF+composite vs baseline FTS-only.
//!
//! ## Philosophie TDD honnête
//!
//! Ce module garantit que l'assertion `assembled.recall@k > baseline.recall@k` repose
//! sur de l'apport RÉEL et non sur une tautologie.
//!
//! ### Garanties anti-tautologie
//!
//! 1. [`EvalEmbedder`] mappe par TOPIC SÉMANTIQUE partagé, jamais par identité de note
//!    (pas d'ULID encodé dans l'embedding).
//! 2. Les keywords **note-side** (corps des notes) et **query-side** (requêtes paraphrase)
//!    sont strictement disjoints → zéro overlap lexical entre requête paraphrase et corps cible.
//! 3. [`seed_eval_corpus`] avec `store_embeddings=false` → `search_semantic` retourne vide
//!    → `assembled.recall == baseline.recall` → l'assertion échoue (red test démontrable).
//! 4. Avec `store_embeddings=true` → embeddings stockés en DB → `search_semantic` retrouve
//!    les notes du même topic → Δrecall > 0 → assertion passe (green).
//!
//! ## Design EvalEmbedder
//!
//! 5 topics avec centroïdes one-hot orthonormés (dim=5) :
//!
//! | Topic | Idx | Keywords note-side (corps notes) | Keywords query-side (requêtes paraphrase) |
//! |---|---|---|---|
//! | deploy | 0 | systemd, daemon, reload, init.d, rollback, bascule | lancement, démarrage, boot |
//! | backup | 1 | rsync, sauvegarde, archive, tar, snapshot, restauration | préservation, copie |
//! | auth | 2 | jwt, bearer, token, authentification, secret, oauth, claim | identité, credential, connexion |
//! | monitoring | 3 | prometheus, grafana, métriques, scrape, alerte, alertmanager | surveillance, observabilité, santé |
//! | network | 4 | nftables, pare-feu, routage, interface, vlan, segment | topologie, filtrage, paquet, adressage |

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use ulid::Ulid;

// ── EvalEmbedder ─────────────────────────────────────────────────────────────

/// Dimension du vecteur sémantique évaluation.
///
/// 5 dimensions = 5 topics orthogonaux → centroïdes one-hot [1,0,0,0,0]…[0,0,0,0,1].
pub const EVAL_DIM: u16 = 5;

/// ID de l'embedder — doit correspondre au champ `embedder_id` stocké en DB par
/// `seed_eval_corpus`, et utilisé par `search_semantic` pour filtrer les embeddings.
pub const EVAL_EMBEDDER_ID: &str = "eval-embedder-v1";

/// EvalEmbedder — embedder déterministe à base de topics sémantiques.
///
/// Mappe TEXT→TOPIC via un dictionnaire de mots-clés contrôlé. Retourne le centroïde
/// one-hot normalisé du topic détecté (`backend_kind=Http` → active le chemin sémantique).
///
/// ## Anti-tautologie
///
/// - Mappe par TOPIC, jamais par identité de note.
/// - Plusieurs notes partagent le même topic → precision@k non triviale.
/// - Séparation note-side / query-side → zéro overlap lexical requête↔corpus.
pub struct EvalEmbedder;

/// Détecte le topic sémantique d'un texte via un dictionnaire de mots-clés.
///
/// Retourne `None` si aucun keyword connu n'est trouvé (texte neutre → vecteur uniforme).
/// Priorité : topic 0 → 4, premier match gagne.
///
/// ## Garantie de séparation lexicale
///
/// Les keywords note-side de chaque topic sont **disjoints** des keywords query-side.
/// Un texte de requête paraphrase ne contient QUE des keywords query-side → son topic
/// est détecté mais il ne partage aucun token avec le corps de la note cible.
fn detect_topic(text: &str) -> Option<usize> {
    let t = text.to_lowercase();

    // Topic 0: deploy
    // note-side: systemd, daemon, reload, init.d, rollback, bascule
    // query-side: lancement, démarrage, boot
    if t.contains("systemd")
        || t.contains("daemon")
        || t.contains("reload")
        || t.contains("init.d")
        || t.contains("rollback")
        || t.contains("bascule")
        || t.contains("lancement")
        || t.contains("démarrage")
        || t.contains("boot")
    {
        return Some(0);
    }

    // Topic 1: backup
    // note-side: rsync, sauvegarde, archive, tar (borné par espaces), snapshot, restauration
    // query-side: préservation, copie
    if t.contains("rsync")
        || t.contains("sauvegarde")
        || t.contains("archive")
        || t.contains(" tar ")
        || t.contains("snapshot")
        || t.contains("restauration")
        || t.contains("préservation")
        || t.contains("copie")
    {
        return Some(1);
    }

    // Topic 2: auth
    // note-side: jwt, bearer, token, authentification, secret, oauth, claim, révocation, blacklist
    // query-side: identité, credential, connexion
    if t.contains("jwt")
        || t.contains("bearer")
        || t.contains("token")
        || t.contains("authentification")
        || t.contains("secret")
        || t.contains("oauth")
        || t.contains("claim")
        || t.contains("révocation")
        || t.contains("blacklist")
        || t.contains("identité")
        || t.contains("credential")
        || t.contains("connexion")
    {
        return Some(2);
    }

    // Topic 3: monitoring
    // note-side: prometheus, grafana, métriques, scrape, alerte, alertmanager, visualisation
    // query-side: surveillance, observabilité, santé
    if t.contains("prometheus")
        || t.contains("grafana")
        || t.contains("métriques")
        || t.contains("scrape")
        || t.contains("alerte")
        || t.contains("alertmanager")
        || t.contains("visualisation")
        || t.contains("surveillance")
        || t.contains("observabilité")
        || t.contains("santé")
    {
        return Some(3);
    }

    // Topic 4: network
    // note-side: nftables, pare-feu, routage, interface, vlan, segment, statique
    // query-side: topologie, filtrage, paquet, adressage
    if t.contains("nftables")
        || t.contains("pare-feu")
        || t.contains("routage")
        || t.contains("interface")
        || t.contains("vlan")
        || t.contains("segment")
        || t.contains("topologie")
        || t.contains("filtrage")
        || t.contains("paquet")
        || t.contains("adressage")
    {
        return Some(4);
    }

    None
}

/// Centroïde one-hot normalisé pour un topic (L2 = 1.0, cosine bien défini).
fn topic_centroid(topic: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; EVAL_DIM as usize];
    if topic < EVAL_DIM as usize {
        v[topic] = 1.0;
    }
    v
}

/// Produit le vecteur d'embedding pour `text`.
///
/// Topic détecté → centroïde one-hot. Texte hors-table → vecteur neutre uniforme
/// (cosine faible avec tous les centroïdes → note non récupérée par aucun topic).
pub fn eval_embed(text: &str) -> Vec<f32> {
    match detect_topic(text) {
        Some(t) => topic_centroid(t),
        None => {
            // Vecteur neutre : toutes dimensions égales, normalisé L2.
            // cosine avec tout centroïde = 1/sqrt(5) ≈ 0.447 — intentionnellement bas.
            let n = EVAL_DIM as usize;
            let val = 1.0_f32 / (n as f32).sqrt();
            vec![val; n]
        }
    }
}

#[async_trait]
impl Embedder for EvalEmbedder {
    fn embedder_id(&self) -> &str {
        EVAL_EMBEDDER_ID
    }

    fn dim(&self) -> u16 {
        EVAL_DIM
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(eval_embed(text))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| eval_embed(t)).collect())
    }

    fn backend_kind(&self) -> EmbedBackend {
        // Http = non-Noop → active le chemin sémantique dans `retrieve_candidates`.
        EmbedBackend::Http
    }
}

// ── Corpus ───────────────────────────────────────────────────────────────────

/// Note du corpus d'évaluation.
pub struct CorpusNote {
    /// Clé stable (non-ULID) — référencée dans `DatasetEntry::expected_keys`.
    pub key: &'static str,
    /// Index du topic (0-4). `99` = distracteur neutre.
    pub topic: usize,
    pub title: &'static str,
    /// Corps : UNIQUEMENT keywords note-side du topic.
    /// ZÉRO overlap avec les keywords query-side des requêtes paraphrase.
    pub body: &'static str,
}

/// 15 notes principales (3 par topic) + 5 distracteurs.
///
/// ## Garantie d'isolation lexicale
///
/// Pour chaque topic, les corps de notes utilisent exclusivement des keywords NOTE-SIDE.
/// Les requêtes paraphrase du dataset utilisent exclusivement des keywords QUERY-SIDE.
/// Il est trivial de vérifier l'absence d'overlap : grep(keyword_query_side, body) = ∅.
pub static CORPUS: &[CorpusNote] = &[
    // Topic 0: deploy — keywords note-side: systemd, daemon, reload, init.d, rollback, bascule
    CorpusNote {
        key: "deploy-checklist",
        topic: 0,
        title: "Checklist déploiement",
        body: "systemd daemon reload unité init.d activer",
    },
    CorpusNote {
        key: "deploy-rollback",
        topic: 0,
        title: "Rollback service",
        body: "rollback daemon restart unité systemd version précédente",
    },
    CorpusNote {
        key: "deploy-bascule",
        topic: 0,
        title: "Bascule blue-green",
        body: "bascule daemon systemd unité reload double-instance",
    },
    // Topic 1: backup — keywords note-side: rsync, sauvegarde, archive, tar, snapshot, restauration
    CorpusNote {
        key: "backup-rsync",
        topic: 1,
        title: "Sauvegarde rsync",
        body: "rsync archive tar snapshot sauvegarde disque",
    },
    CorpusNote {
        key: "backup-restore",
        topic: 1,
        title: "Restauration archive",
        body: "restauration tar rsync snapshot sauvegarde récupération",
    },
    CorpusNote {
        key: "backup-rotation",
        topic: 1,
        title: "Rotation archive",
        body: "rotation rsync archive snapshot tar purge",
    },
    // Topic 2: auth — keywords note-side: jwt, bearer, token, authentification, secret, oauth, claim
    CorpusNote {
        key: "auth-jwt",
        topic: 2,
        title: "JWT bearer",
        body: "jwt bearer token secret authentification claim",
    },
    CorpusNote {
        key: "auth-oauth",
        topic: 2,
        title: "OAuth token",
        body: "oauth token bearer authentification jwt refresh",
    },
    CorpusNote {
        key: "auth-revocation",
        topic: 2,
        title: "Révocation token",
        body: "révocation jwt bearer token authentification blacklist",
    },
    // Topic 3: monitoring — keywords note-side: prometheus, grafana, métriques, scrape, alerte, alertmanager
    CorpusNote {
        key: "monitor-prometheus",
        topic: 3,
        title: "Prometheus métriques",
        body: "prometheus grafana métriques scrape alerte règle",
    },
    CorpusNote {
        key: "monitor-alerting",
        topic: 3,
        title: "Alerting règles",
        body: "alertmanager alerte métriques prometheus scrape grafana",
    },
    CorpusNote {
        key: "monitor-dashboard",
        topic: 3,
        title: "Tableau de bord métriques",
        body: "grafana dashboard métriques scrape prometheus visualisation",
    },
    // Topic 4: network — keywords note-side: nftables, pare-feu, routage, interface, vlan, segment
    CorpusNote {
        key: "network-firewall",
        topic: 4,
        title: "Pare-feu nftables",
        body: "nftables pare-feu règle routage interface réseau",
    },
    CorpusNote {
        key: "network-vlan",
        topic: 4,
        title: "VLAN réseau",
        body: "vlan routage interface pare-feu nftables segment",
    },
    CorpusNote {
        key: "network-routing",
        topic: 4,
        title: "Routage statique",
        body: "routage statique interface nftables pare-feu table",
    },
    // Distracteurs — topic 99 → vecteur neutre (cosine faible avec tous les centroïdes)
    CorpusNote {
        key: "distractor-1",
        topic: 99,
        title: "Documentation code",
        body: "documentation code projet développeur manuel guide",
    },
    CorpusNote {
        key: "distractor-2",
        topic: 99,
        title: "Planning sprint",
        body: "réunion équipe planning sprint itération backlog",
    },
    CorpusNote {
        key: "distractor-3",
        topic: 99,
        title: "Schema base données",
        body: "base données schema migration requête index",
    },
    CorpusNote {
        key: "distractor-4",
        topic: 99,
        title: "Tests unitaires",
        body: "test unitaire assertion mock fixture vérifié",
    },
    CorpusNote {
        key: "distractor-5",
        topic: 99,
        title: "Rapport statistique",
        body: "rapport analyse statistique résultat mesure graphe",
    },
];

// ── Dataset ───────────────────────────────────────────────────────────────────

/// Famille de requête dans le dataset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// BM25 ET sémantique retrouvent les cibles (tokens partagés query↔corps).
    Lexical,
    /// ZÉRO token commun query↔corps. Seul le sémantique réussit.
    /// Ce sont ces requêtes qui **prouvent l'apport** du pipeline RRF.
    Paraphrase,
}

/// Entrée du dataset d'évaluation.
pub struct DatasetEntry {
    pub query: &'static str,
    /// Clés des notes attendues (résolues en ULIDs au runtime via `key_to_ulid`).
    pub expected_keys: &'static [&'static str],
    pub kind: QueryKind,
}

/// Dataset d'évaluation : 10 requêtes lexicales + 5 requêtes paraphrase.
///
/// ## Propriétés garanties
///
/// **Lexicales** : chaque token de la requête apparaît dans les corps des notes attendues.
/// BM25 retrouve les cibles → recall > 0 pour les deux modes (baseline non nulle, test honnête).
///
/// **Paraphrase** : AUCUN token de la requête n'apparaît dans les corps des notes attendues
/// (vérifiable par grep). BM25 → ∅. EvalEmbedder → même centroïde topic → cosine ≈ 1.0.
///
/// ## Proof de non-overlap pour les 5 requêtes paraphrase
///
/// | Query | Topic | Tokens query | Tokens corps note | Overlap |
/// |---|---|---|---|---|
/// | "lancement application démarrage boot procédure" | 0 | lancement,démarrage,boot | systemd,daemon,reload,init.d,rollback,bascule | ∅ |
/// | "préservation données copie distante" | 1 | préservation,copie | rsync,sauvegarde,archive,tar,snapshot,restauration | ∅ |
/// | "vérification identité credential connexion utilisateur" | 2 | identité,credential,connexion | jwt,bearer,token,authentification,secret,oauth,claim | ∅ |
/// | "surveillance observabilité santé état" | 3 | surveillance,observabilité,santé | prometheus,grafana,métriques,scrape,alerte,alertmanager | ∅ |
/// | "topologie filtrage paquet adressage" | 4 | topologie,filtrage,paquet,adressage | nftables,pare-feu,routage,interface,vlan,segment | ∅ |
pub static DATASET: &[DatasetEntry] = &[
    // ── Lexicales (tokens partagés → BM25 et sémantique retrouvent les notes) ──
    DatasetEntry {
        query: "systemd daemon reload",
        expected_keys: &["deploy-checklist", "deploy-rollback", "deploy-bascule"],
        kind: QueryKind::Lexical,
    },
    DatasetEntry {
        query: "rsync archive snapshot sauvegarde",
        expected_keys: &["backup-rsync", "backup-restore", "backup-rotation"],
        kind: QueryKind::Lexical,
    },
    DatasetEntry {
        query: "jwt bearer token authentification",
        expected_keys: &["auth-jwt", "auth-oauth", "auth-revocation"],
        kind: QueryKind::Lexical,
    },
    DatasetEntry {
        query: "prometheus grafana métriques scrape",
        expected_keys: &[
            "monitor-prometheus",
            "monitor-alerting",
            "monitor-dashboard",
        ],
        kind: QueryKind::Lexical,
    },
    DatasetEntry {
        query: "nftables pare-feu routage interface",
        expected_keys: &["network-firewall", "network-vlan", "network-routing"],
        kind: QueryKind::Lexical,
    },
    DatasetEntry {
        query: "systemd unité restart",
        expected_keys: &["deploy-checklist", "deploy-rollback", "deploy-bascule"],
        kind: QueryKind::Lexical,
    },
    DatasetEntry {
        query: "sauvegarde snapshot restauration",
        expected_keys: &["backup-rsync", "backup-restore", "backup-rotation"],
        kind: QueryKind::Lexical,
    },
    DatasetEntry {
        query: "authentification oauth jwt",
        expected_keys: &["auth-jwt", "auth-oauth", "auth-revocation"],
        kind: QueryKind::Lexical,
    },
    DatasetEntry {
        query: "alerte scrape alertmanager",
        expected_keys: &[
            "monitor-prometheus",
            "monitor-alerting",
            "monitor-dashboard",
        ],
        kind: QueryKind::Lexical,
    },
    DatasetEntry {
        query: "vlan routage interface",
        expected_keys: &["network-firewall", "network-vlan", "network-routing"],
        kind: QueryKind::Lexical,
    },
    // ── Paraphrase (BM25 = ∅, seul le sémantique retrouve via topic centroid) ──
    DatasetEntry {
        query: "lancement application démarrage boot procédure",
        expected_keys: &["deploy-checklist", "deploy-rollback", "deploy-bascule"],
        kind: QueryKind::Paraphrase,
    },
    DatasetEntry {
        query: "préservation données copie distante",
        expected_keys: &["backup-rsync", "backup-restore", "backup-rotation"],
        kind: QueryKind::Paraphrase,
    },
    DatasetEntry {
        query: "vérification identité credential connexion utilisateur",
        expected_keys: &["auth-jwt", "auth-oauth", "auth-revocation"],
        kind: QueryKind::Paraphrase,
    },
    DatasetEntry {
        query: "surveillance observabilité santé état",
        expected_keys: &[
            "monitor-prometheus",
            "monitor-alerting",
            "monitor-dashboard",
        ],
        kind: QueryKind::Paraphrase,
    },
    DatasetEntry {
        query: "topologie filtrage paquet adressage",
        expected_keys: &["network-firewall", "network-vlan", "network-routing"],
        kind: QueryKind::Paraphrase,
    },
];

// ── Corpus seeder ─────────────────────────────────────────────────────────────

/// Seed le corpus d'évaluation dans un `SqliteIndex`.
///
/// ## Flux
///
/// Pour chaque note du [`CORPUS`] :
/// 1. `seed_note_with_fts(ulid, section, body)` → FTS5 indexée + `notes` table.
/// 2. `upsert_note_title(NoteId, title)` → résolution titre.
/// 3. **Si `store_embeddings=true`** : `seed_note_embedding(ulid, embedder_id, dim, vector)`
///    → embedding stocké dans `note_embeddings`. C'est LE point critique : sans cet appel,
///    `search_semantic` retourne ∅ et `assembled.recall == baseline.recall` (red-test).
///
/// ## Preuve du red-test
///
/// Appeler `seed_eval_corpus(idx, false)` produit le même recall que la baseline Noop :
/// `EvalEmbedder` embed la requête mais `search_semantic` trouve 0 vecteurs en DB →
/// RRF dégradé → recall identique → l'assertion STRICTE `assembled > baseline` ÉCHOUE.
/// C'est le comportement TDD "rouge" exact (pas un faux rouge de compilation).
///
/// ## Retour
///
/// Map `key → ULID` — utilisée pour résoudre les `expected_keys` du dataset au runtime.
pub async fn seed_eval_corpus(
    idx: &Arc<SqliteIndex>,
    store_embeddings: bool,
) -> HashMap<String, String> {
    let mut key_to_ulid: HashMap<String, String> = HashMap::with_capacity(CORPUS.len());

    for note in CORPUS {
        let ulid_str = Ulid::generate().to_string();
        // Corps FTS : titre en H1 + body (même pattern que seed_note_sql_only dans helpers).
        let body_fts = format!("# {}\n{}", note.title, note.body);

        idx.seed_note_with_fts(&ulid_str, "reference", &body_fts)
            .await
            .expect("seed_eval_corpus: seed_note_with_fts");

        let note_id =
            NoteId(Ulid::from_string(&ulid_str).expect("seed_eval_corpus: ULID parse — invariant"));
        idx.upsert_note_title("main", &note_id, note.title)
            .await
            .expect("seed_eval_corpus: upsert_note_title");

        if store_embeddings {
            // Embedding du corps (pas du titre + corps) pour que detect_topic mappe
            // sur les keywords note-side uniquement — garantit la séparation lexicale.
            let vector = eval_embed(note.body);
            idx.seed_note_embedding(&ulid_str, EVAL_EMBEDDER_ID, EVAL_DIM, &vector)
                .await
                .expect("seed_eval_corpus: seed_note_embedding");
        }

        key_to_ulid.insert(note.key.to_string(), ulid_str);
    }

    key_to_ulid
}

// ── Métriques IR ─────────────────────────────────────────────────────────────

/// Precision@k : fraction des `k` premiers résultats pertinents.
///
/// # Arguments
///
/// - `retrieved` : ULIDs retournés dans l'ordre par le système (top-k).
/// - `expected` : ensemble des ULIDs pertinents pour la requête.
/// - `k` : taille de la fenêtre d'évaluation.
pub fn precision_at_k(retrieved: &[String], expected: &[String], k: usize) -> f64 {
    if k == 0 || retrieved.is_empty() {
        return 0.0;
    }
    let hits = retrieved
        .iter()
        .take(k)
        .filter(|id| expected.contains(id))
        .count();
    hits as f64 / k.min(retrieved.len()) as f64
}

/// Recall@k : fraction des notes pertinentes retrouvées dans les `k` premiers résultats.
///
/// Retourne `0.0` si `expected` est vide (pas de cible → aucune performance mesurable).
pub fn recall_at_k(retrieved: &[String], expected: &[String], k: usize) -> f64 {
    if expected.is_empty() {
        return 0.0;
    }
    let hits = retrieved
        .iter()
        .take(k)
        .filter(|id| expected.contains(id))
        .count();
    hits as f64 / expected.len() as f64
}

// ── Runner ────────────────────────────────────────────────────────────────────

/// Rapport d'évaluation agrégé (moyenne sur toutes les requêtes du dataset).
#[derive(Debug)]
pub struct EvalReport {
    pub precision_at_k: f64,
    pub recall_at_k: f64,
    pub n_queries: usize,
}

/// Exécute l'évaluation IR sur l'ensemble du [`DATASET`].
///
/// Appel direct à `retrieve_candidates` (pas de HTTP) pour une mesure précise de
/// precision/recall@k sans bruit de budget ou de troncature.
///
/// ## Flux par requête
///
/// 1. `retrieve_candidates(state, vault_id, query, top_n=k*3, timeout=5s)`
/// 2. Top-k candidats (ULIDs).
/// 3. Résolution `expected_keys → ULIDs` via `key_to_ulid`.
/// 4. `precision_at_k` + `recall_at_k`.
///
/// ## Mode implicite
///
/// Le mode (Assembled vs FtsOnly) est déterminé par l'embedder câblé dans `state` :
/// - `EvalEmbedder` → sémantique actif → pipeline RRF complet.
/// - `NoopBackend` → `embed_fallback=true` → BM25-only.
///
/// # Arguments
///
/// - `state` : état du serveur avec l'embedder injecté.
/// - `key_to_ulid` : map produite par [`seed_eval_corpus`] pour ce même env.
/// - `k` : fenêtre d'évaluation (recommandé : 5).
pub async fn run_eval(
    state: &gradatum_server::state::AppState,
    key_to_ulid: &HashMap<String, String>,
    k: usize,
) -> EvalReport {
    use gradatum_server::context::retrieval::retrieve_candidates;

    let vault_id = gradatum_core::scope::AclCheckedVaultId::for_system_task(VaultId::new("main"));
    let mut total_precision = 0.0_f64;
    let mut total_recall = 0.0_f64;
    let n = DATASET.len();

    for entry in DATASET {
        let outcome = retrieve_candidates(
            state,
            &vault_id,
            entry.query,
            None,  // pas de filtre section
            k * 3, // top_n élargi — RRF a besoin de candidats à fusionner
            5_000, // embed_timeout_ms — généreux pour les tests (pas de vrai HTTP)
        )
        .await
        .expect("run_eval: retrieve_candidates — invariant test");

        // Top-k ULIDs retournés par le système.
        let retrieved: Vec<String> = outcome
            .candidates
            .iter()
            .take(k)
            .map(|c| c.note_id.clone())
            .collect();

        // ULIDs attendus (résolus depuis les clés stables du dataset).
        let expected: Vec<String> = entry
            .expected_keys
            .iter()
            .filter_map(|key| key_to_ulid.get(*key).cloned())
            .collect();

        total_precision += precision_at_k(&retrieved, &expected, k);
        total_recall += recall_at_k(&retrieved, &expected, k);
    }

    EvalReport {
        precision_at_k: total_precision / n as f64,
        recall_at_k: total_recall / n as f64,
        n_queries: n,
    }
}

// ── Mode enum (documentation) ─────────────────────────────────────────────────

/// Mode d'évaluation — documentaire, le mode réel est l'embedder de l'env.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalMode {
    /// Pipeline complet RRF (BM25 + sémantique) — env avec `EvalEmbedder`.
    Assembled,
    /// Baseline BM25-only — env avec `NoopBackend` (`embed_fallback=true`).
    FtsOnly,
}
