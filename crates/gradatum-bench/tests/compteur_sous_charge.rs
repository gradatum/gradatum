//! F-162 lot 0 Task 5 — vérification décisive du compteur.
//!
//! Ce fichier est la Tâche 5 du plan
//! `docs/superpowers/plans/2026-08-23-f162-lot0-recherche.md`. Il tranche
//! l'unique hypothèse encore testable du sous-comptage du 22/08 (31 ≠ 32 ≠ 33) :
//! une **désynchronisation entre la table `notes` et sa table d'index plein-texte
//! `notes_fts` sous écritures concurrentes**.
//!
//! # Deux vérifications
//!
//! 1. **Étape 1 — décompte juste sur cardinalités connues, au repos.** Le corpus
//!    synthétique plante trois termes à cardinalité connue (6, 8, 15) plus une
//!    sonde 200 et un absent 0. On vérifie que `corpus_match_count` (HTTP
//!    `vault_search`, `include_corpus_count=true`) les retrouve exactement, sans
//!    écriture concurrente.
//!
//! 2. **Étape 2 — parité `notes` / `notes_fts` SOUS écritures concurrentes.**
//!    Mécanisme mesuré : `upsert_note` (`crates/gradatum-index/src/sqlite.rs`)
//!    insère d'abord dans `notes`, puis dans `notes_fts`, en DEUX instructions
//!    autocommit **sans transaction englobante**. Entre les deux commits, un
//!    lecteur concurrent voit la note dans `notes` mais pas dans l'index FTS →
//!    le décompte FTS sous-compte pendant cette fenêtre.
//!
//!    Pendant qu'une charge écrit des notes contenant le terme planté
//!    (`cargo-semver-checks`), une **sonde SQLite atomique** (une seule
//!    instruction, donc un seul snapshot WAL) compare en continu :
//!    `COUNT(*) FROM notes_fts WHERE notes_fts MATCH <phrase>` contre
//!    `COUNT(*) FROM notes WHERE body_text LIKE <terme>`. Toute sonde où
//!    `fts_count != notes_count` est une divergence observée.
//!
//! # Conditions qui font échouer le test (déclarées explicitement)
//!
//! - **CONDITION A — transitoire** : une sonde quelconque, pendant la charge,
//!   observe `fts_count != notes_count`. Si la fenêtre à deux commits est
//!   réellement observable, elle se manifeste ainsi.
//! - **CONDITION B — persistante** : une fois la charge stabilisée (toutes les
//!   écritures abouties), `fts_count != notes_count` sur la population totale,
//!   ou un `note_id` accepté est présent dans `notes` mais absent de `notes_fts`.
//!   Un échec du second INSERT laisserait une divergence durable.
//!
//! Une divergence transitoire OBSERVÉE est le résultat qui confirme l'hypothèse :
//! le test ROUGE **est** le verdict. Un test vert (aucune divergence observée)
//! est le verdict « pas reproduit » — les deux sont consignés dans le rapport
//! imprimé par le test, puis dans le message de commit.
//!
//! # Exécution (harnais éphémère)
//!
//! Le test est piloté par le harnais éphémère (`scripts/internal/eph-server.sh`
//! puis `eph-ingest.sh`), qui pose `EPH_URL`, `EPH_JWT`, `EPH_INDEX_DB`. Sans ces
//! trois variables le test se SKIPE (retour immédiat) pour ne pas casser
//! `cargo test -p gradatum-bench` hors harnais.
//!
//! ```bash
//! EPH_MODE=bm25 NEED_WORKER=1 EPH_BIN_DIR=target/release \
//!   EPH_WORKDIR=~/tmp/scratch/f162-eph/t5 scripts/internal/eph-server.sh up
//! scripts/internal/eph-ingest.sh --workdir ~/tmp/scratch/f162-eph/t5
//! source ~/tmp/scratch/f162-eph/t5/eph.env
//! EPH_URL="$EPH_URL" EPH_JWT="$EPH_JWT" EPH_INDEX_DB="$EPH_INDEX_DB" \
//!   cargo test -p gradatum-bench --test compteur_sous_charge --release \
//!     -- --nocapture --test-threads=1
//! scripts/internal/eph-server.sh down
//! ```
//!
//! Paramètres de charge surchargeables par env :
//! `COMPT_NB_ECRITS` (120), `COMPT_CORPS_PARAGRAPHES` (260 ≈ 60 Ko/note),
//! `COMPT_PROBE_INTERVAL_US` (500), `COMPT_TACHES` (3).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::json;

// ─── Terme planté de la mesure de parité ─────────────────────────────────────
// Présent dans 6 notes du corpus synthétique (fixture expected-counts-v1.json,
// q01). La charge en ajoute d'autres — le compte de départ est donc connu (6).
const TERME: &str = "cargo-semver-checks";
const FTS_PHRASE: &str = "\"cargo-semver-checks\""; // phrase FTS5 (le `-` serait sinon lu comme NOT)
const LIKE_MOTIF: &str = "%cargo-semver-checks%";

// ─── Fixture des décomptes attendus (compilée — indépendante du cwd) ─────────
const ATTENDUS_JSON: &str = include_str!("../fixtures/expected-counts-v1.json");

// ─── Paramètres de charge ────────────────────────────────────────────────────
fn env_u64(nom: &str, defaut: u64) -> u64 {
    std::env::var(nom)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(defaut)
}

// ─── Harness ─────────────────────────────────────────────────────────────────
#[derive(Clone)]
struct Harness {
    url: String,
    jwt: String,
    index_db: String,
}

fn harness() -> Option<Harness> {
    let url = std::env::var("EPH_URL").ok()?;
    let jwt = std::env::var("EPH_JWT").ok()?;
    let index_db = std::env::var("EPH_INDEX_DB").ok()?;
    if url.is_empty() || jwt.is_empty() || index_db.is_empty() {
        return None;
    }
    Some(Harness { url, jwt, index_db })
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("construction client HTTP")
}

// ─── API HTTP ────────────────────────────────────────────────────────────────

/// `corpus_match_count` de `vault_search` (décompte lexical FTS, opt-in).
async fn vault_search_cmc(client: &reqwest::Client, h: &Harness, query: &str) -> u64 {
    let resp = client
        .post(format!("{}/api/v1/vault_search", h.url))
        .bearer_auth(&h.jwt)
        .json(&json!({ "query": query, "limit": 1, "include_corpus_count": true }))
        .send()
        .await
        .expect("requête vault_search");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "vault_search HTTP {status} : {body}");
    let v: serde_json::Value =
        serde_json::from_str(&body).expect("réponse vault_search non parsable");
    v.get("corpus_match_count")
        .and_then(|c| c.as_u64())
        .unwrap_or_else(|| panic!("corpus_match_count absent : {body}"))
}

/// Soumet une note via `vault_write` (202 = mise en file). Retourne `(note_id, job_id)`.
async fn vault_write(
    client: &reqwest::Client,
    h: &Harness,
    titre: &str,
    corps: &str,
) -> (String, String) {
    let resp = client
        .post(format!("{}/api/v1/vault_write", h.url))
        .bearer_auth(&h.jwt)
        .json(&json!({ "title": titre, "body": corps, "section_hint": "reference" }))
        .send()
        .await
        .expect("requête vault_write");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(
        status,
        reqwest::StatusCode::ACCEPTED,
        "vault_write {status} : {body}"
    );
    let v: serde_json::Value =
        serde_json::from_str(&body).expect("réponse vault_write non parsable");
    let note_id = v
        .get("note_id")
        .and_then(|x| x.as_str())
        .unwrap_or_else(|| panic!("note_id absent : {body}"))
        .to_string();
    let job_id = v
        .get("job_id")
        .and_then(|x| x.as_str())
        .unwrap_or_else(|| panic!("job_id absent : {body}"))
        .to_string();
    (note_id, job_id)
}

// ─── Sonde SQLite ATOMIQUE (un seul snapshot WAL) ────────────────────────────

/// Compte en une seule instruction (donc un seul snapshot de lecture WAL) le
/// nombre de lignes indexées dans `notes_fts` matchant la phrase, et le nombre
/// de lignes de `notes` dont le corps contient le terme. Si un écrivain
/// concurrent est entre ses deux commits, les deux comptes divergent.
fn probe_sql(conn: &Connection) -> (i64, i64) {
    conn.query_row(
        "SELECT \
           (SELECT COUNT(*) FROM notes_fts WHERE notes_fts MATCH ?1), \
           (SELECT COUNT(*) FROM notes WHERE body_text LIKE ?2)",
        rusqlite::params![FTS_PHRASE, LIKE_MOTIF],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .expect("sonde SQL notes/fts")
}

#[derive(Debug, Default, Clone)]
struct ProbeStats {
    total: u64,
    divergences: Vec<(u64, i64, i64)>, // (index de sonde, fts, notes)
    max_ecart: i64,
}

/// Boucle de sonde : lit `fts` et `notes` en continu tant que `stop` est faux,
/// enregistre chaque divergence observée. `interval` est la part de sommeil
/// entre deux sondes (la sonde elle-même compte dans le rythme réel).
fn thread_sonde(
    index_db: &str,
    stop: Arc<AtomicBool>,
    stats: Arc<Mutex<ProbeStats>>,
    interval: Duration,
) {
    let conn = Connection::open(index_db).expect("ouverture index.db pour la sonde");
    let mut idx: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        let (fts, notes) = probe_sql(&conn);
        idx += 1;
        let ecart = notes - fts;
        let mut s = stats.lock().expect("verrou stats sonde");
        s.total += 1;
        if ecart != 0 {
            s.divergences.push((idx, fts, notes));
            s.max_ecart = s.max_ecart.max(ecart);
        }
        drop(s);
        let elapsed = t0.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}

// ─── Corps des notes de charge (déterministe, sans donnée personnelle) ───────

/// Corps volumineux contenant le terme planté. La taille élargit la fenêtre
/// FTS (insertion d'un grand corps dans l'index = plus lente), comme les notes
/// réelles du vault qui sont volumineuses.
fn corps_charge(i: usize, paragraphes: usize) -> String {
    let mut corps = String::with_capacity(paragraphes * 260 + 128);
    let _ = writeln!(
        corps,
        "# Note de charge F-162 #{i} — parité notes/notes_fts"
    );
    let _ = writeln!(
        corps,
        "Terme planté pour la mesure de parité : cargo-semver-checks."
    );
    for p in 0..paragraphes {
        let _ = writeln!(
            corps,
            "Paragraphe synthétique {p} de la note {i} : le banc F-162 mesure la parité \
             entre la table notes et la table notes_fts sous écritures concurrentes, pour \
             trancher l'hypothèse du sous-comptage du 22 août. Remplissage déterministe, \
             aucune donnée personnelle."
        );
        let _ = writeln!(corps);
    }
    corps
}

// ─── Étape 1 : décompte juste sur cardinalités connues, au repos ─────────────

#[ignore = "exige le harnais éphémère (EPH_URL/EPH_JWT/EPH_INDEX_DB posés par eph-server.sh/eph-ingest.sh) ; reproduit le défaut F-262 (divergence notes/notes_fts) — instrument de reproduction, pas gate"]
#[tokio::test(flavor = "multi_thread")]
async fn decompte_juste_sur_cardinalites_connues() {
    let Some(h) = harness() else {
        eprintln!(
            "compteur_sous_charge : harnais éphémère absent (EPH_URL/EPH_JWT/EPH_INDEX_DB non posés) — test sauté"
        );
        return;
    };
    let client = client();
    let attendus: serde_json::Value =
        serde_json::from_str(ATTENDUS_JSON).expect("fixture expected-counts-v1.json");
    let queries = attendus["queries"].as_array().expect("requêtes attendues");

    let mut echecs = Vec::new();
    for q in queries {
        let id = q["id"].as_str().unwrap_or("?");
        let literal = q["literal"].as_str().expect("literal");
        let attendu = q["expected_count"].as_u64().expect("expected_count");
        let mesure = vault_search_cmc(&client, &h, literal).await;
        eprintln!("  au repos  {id} `{literal}` : attendu {attendu}, mesuré {mesure}");
        if mesure != attendu {
            echecs.push(format!(
                "{id} (`{literal}`) : attendu {attendu}, mesuré {mesure}"
            ));
        }
    }
    assert!(
        echecs.is_empty(),
        "décompte FAUX au repos sur cardinalités connues :\n{}",
        echecs.join("\n")
    );
    eprintln!("compteur_sous_charge : étape 1 VERTE — les 5 décomptes au repos sont exacts.");
}

// ─── Étape 2 : parité notes/notes_fts sous écritures concurrentes ────────────

#[ignore = "exige le harnais éphémère (EPH_URL/EPH_JWT/EPH_INDEX_DB posés par eph-server.sh/eph-ingest.sh) ; reproduit le défaut F-262 (divergence notes/notes_fts sous écritures concurrentes) — instrument de reproduction, pas gate"]
#[tokio::test(flavor = "multi_thread")]
async fn parite_notes_fts_sous_ecritures_concurrentes() {
    let Some(h) = harness() else {
        eprintln!(
            "compteur_sous_charge : harnais éphémère absent (EPH_URL/EPH_JWT/EPH_INDEX_DB non posés) — test sauté"
        );
        return;
    };

    let nb_ecrits = env_u64("COMPT_NB_ECRITS", 120) as usize;
    let paragraphes = env_u64("COMPT_CORPS_PARAGRAPHES", 260) as usize;
    let interval_us = env_u64("COMPT_PROBE_INTERVAL_US", 500);
    let nb_taches = env_u64("COMPT_TACHES", 3).max(1) as usize;

    let client = client();

    // ── Pré-charge : la sonde SQL voit déjà fts == notes (sans écriture) ────
    let conn = Connection::open(&h.index_db).expect("ouverture index.db (pré-charge)");
    let (fts0, notes0) = probe_sql(&conn);
    drop(conn);
    assert_eq!(
        fts0, notes0,
        "pré-charge : FTS ≠ notes avant même la charge (fts={fts0}, notes={notes0})"
    );

    // ── Étape 1 (au repos, HTTP) : cardinalités plantées exactes ────────────
    // q01 (le terme de la charge) est comparé à la vérité `notes` du moment
    // (robuste à une exécution antérieure du test sur le même serveur) ; les
    // quatre autres sont comparés à leur valeur plantée, jamais polluée.
    let attendus: serde_json::Value =
        serde_json::from_str(ATTENDUS_JSON).expect("fixture expected-counts-v1.json");
    let mut echecs = Vec::new();
    for q in attendus["queries"].as_array().expect("requêtes attendues") {
        let id = q["id"].as_str().unwrap_or("?");
        let literal = q["literal"].as_str().expect("literal");
        let mesure = vault_search_cmc(&client, &h, literal).await;
        let attendu = if literal == TERME {
            notes0 as u64
        } else {
            q["expected_count"].as_u64().expect("expected_count")
        };
        if mesure != attendu {
            echecs.push(format!(
                "{id} (`{literal}`) : attendu {attendu}, mesuré {mesure}"
            ));
        }
    }
    assert!(
        echecs.is_empty(),
        "décompte faux au repos :\n{}",
        echecs.join("\n")
    );

    // ── Charge concurrente ───────────────────────────────────────────────────
    let attendu_fin = notes0 + nb_ecrits as i64;
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Mutex::new(ProbeStats::default()));
    // (note_id, job_id, marqueur unique présent dans le corps → check FTS par phrase)
    let acceptes = Arc::new(Mutex::new(Vec::<(String, String, String)>::new()));

    let sonde = {
        let (stop_c, stats_c, db_c) = (stop.clone(), stats.clone(), h.index_db.clone());
        std::thread::spawn(move || {
            thread_sonde(&db_c, stop_c, stats_c, Duration::from_micros(interval_us));
        })
    };

    let mut taches = Vec::new();
    for t in 0..nb_taches {
        let (client_c, h_c, acceptes_c) = (client.clone(), h.clone(), acceptes.clone());
        let debut = t * (nb_ecrits / nb_taches);
        let fin = if t == nb_taches - 1 {
            nb_ecrits
        } else {
            (t + 1) * (nb_ecrits / nb_taches)
        };
        taches.push(tokio::spawn(async move {
            for i in debut..fin {
                let corps = corps_charge(i, paragraphes);
                let titre = format!("Note de charge F-162 #{i}");
                let (note_id, job_id) = vault_write(&client_c, &h_c, &titre, &corps).await;
                acceptes_c
                    .lock()
                    .expect("verrou acceptés")
                    .push((note_id, job_id, titre));
            }
        }));
    }
    for t in taches {
        t.await.expect("tâche écrivaine");
    }

    // ── Stabilisation : attendre que toutes les écritures aboutissent dans notes ──
    // La sonde reste ACTIVE pendant toute la consommation de la file (les écritures
    // sont asynchrones : le 202 met en file, le worker écrit ensuite, une à une).
    let conn = Connection::open(&h.index_db).expect("ouverture index.db (stabilisation)");
    let deadline = Instant::now() + Duration::from_secs(180);
    let (_, mut notes_fin) = probe_sql(&conn);
    while notes_fin < attendu_fin {
        if Instant::now() > deadline {
            eprintln!(
                "AVERTISSEMENT : notes={notes_fin} < attendu {attendu_fin} après 180 s — écritures non abouties"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        (_, notes_fin) = probe_sql(&conn);
    }
    // Dernier commit possible de l'écrivain : laisser la fenêtre se refermer.
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.store(true, Ordering::Relaxed);
    let _ = sonde.join();

    let probe_stats = stats.lock().expect("verrou stats").clone();
    let acceptes_liste = acceptes.lock().expect("verrou acceptés").clone();

    // ── Mesures finales (fenêtre refermée) ───────────────────────────────────
    let (fts_fin, notes_fin) = probe_sql(&conn);

    // ── Vérification persistante par note écrite ─────────────────────────────
    // NOTE : `notes_fts` est un FTS5 external-content (`content=notes`) — un
    // `SELECT rowid FROM notes_fts` lit la table `notes`, PAS l'index. L'index est
    // interrogé via `MATCH` : on vérifie chaque note écrite par sa PHRASE unique
    // (le titre, présent dans le corps). Présente dans notes mais introuvable au
    // MATCH = entrée FTS manquante = divergence persistante.
    let mut dans_notes = 0usize;
    let mut dans_notes_sans_fts = 0usize;
    let mut exemples = Vec::new();
    for (note_id, _, marqueur) in &acceptes_liste {
        let in_notes: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM notes WHERE id=?1 AND vault_id='main')",
                [note_id],
                |r| r.get(0),
            )
            .expect("existence notes");
        if in_notes == 1 {
            dans_notes += 1;
            let phrase = format!("\"{marqueur}\"");
            let in_fts: i64 = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM notes_fts WHERE notes_fts MATCH ?1)",
                    [&phrase],
                    |r| r.get(0),
                )
                .expect("existence fts (phrase)");
            if in_fts == 0 {
                dans_notes_sans_fts += 1;
                if exemples.len() < 5 {
                    exemples.push(note_id.clone());
                }
            }
        }
    }
    drop(conn);

    // ── Rapport ──────────────────────────────────────────────────────────────
    let d = probe_stats.divergences.len();
    let prem = probe_stats
        .divergences
        .first()
        .map(|(i, f, n)| format!("sonde #{i} fts={f} notes={n}"))
        .unwrap_or_else(|| "—".to_string());
    println!("════════ RAPPORT PARITÉ notes/notes_fts SOUS CHARGE (Task 5) ════════");
    println!("harness  : url={} index_db={}", h.url, h.index_db);
    println!(
        "charge   : {nb_ecrits} écritures × ~{paragraphes} paragraphes, {nb_taches} tâche(s), sonde ≈ {interval_us} µs"
    );
    println!("pré-charge: fts={fts0} notes={notes0}");
    println!("écrits acceptés      : {} (HTTP 202)", acceptes_liste.len());
    println!("notes attendues fin  : {attendu_fin}");
    println!("sondes totales       : {}", probe_stats.total);
    println!(
        "divergences TRANSITOIRES observées : {d} (max écart {})",
        probe_stats.max_ecart
    );
    if d > 0 {
        println!("  première : {prem}");
    }
    println!("après stabilisation : fts={fts_fin} notes={notes_fin}");
    println!("notes écrites présentes dans notes : {dans_notes}");
    println!("présentes dans notes SANS entrée FTS : {dans_notes_sans_fts} {exemples:?}");
    println!(
        "VERDICT : {}",
        if d == 0 && fts_fin == notes_fin && dans_notes_sans_fts == 0 {
            "pas de divergence observée (VERT)"
        } else {
            "divergence observée (ROUGE — hypothèse reproduite)"
        }
    );
    println!("══════════════════════════════════════════════════════════════════════");

    // ── Assertions : les conditions qui font échouer le test ─────────────────
    assert_eq!(
        d, 0,
        "CONDITION A : {d} sonde(s) ont observé FTS ≠ notes pendant la charge \
         ({prem}) — la désynchronisation notes/notes_fts SE REPRODUIT sous écritures concurrentes"
    );
    assert_eq!(
        notes_fin, fts_fin,
        "CONDITION B : après stabilisation notes={notes_fin} ≠ fts={fts_fin} — divergence PERSISTANTE"
    );
    assert_eq!(
        dans_notes_sans_fts, 0,
        "CONDITION B : {dans_notes_sans_fts} note(s) écrite(s) dans notes mais absentes de notes_fts {exemples:?} — divergence PERSISTANTE"
    );
}
