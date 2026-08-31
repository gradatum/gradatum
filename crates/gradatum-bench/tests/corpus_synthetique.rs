//! F-162 lot 0 Task 2 — le corpus synthétique et ses décomptes dérivés.
//!
//! Ces tests prouvent les propriétés des fixtures commitées, **sans moteur** :
//!
//! 1. chaque `expected_count` se recalcule à l'identique depuis le corpus ;
//! 2. les trois termes ponctués plantés ont les cardinalités normatives (6, 8, 15) ;
//! 3. le graphe de liens a la distribution de degré normative (82 % à 0, {0,1,3,10}),
//!    mesurée **depuis les corps** via la fonction de production `extract_wikilinks`
//!    — degré entrant (backlinks) et sortant (liens émis) ;
//! 4. le champ JSON `links` et les wikilinks du corps disent la même chose.
//!
//! Un décompte dérivé par construction (ici) ≠ un classement capturé-puis-gelé
//! (Task 3) : ces deux artefacts sont distincts par nature.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gradatum_core::section::Section;
use gradatum_curator::wikilinks::extract_wikilinks;
use serde::Deserialize;

/// Une note du corpus synthétique (sous-ensemble des champs suffisant aux tests).
#[derive(Deserialize)]
struct CorpusNote {
    id: String,
    section: String,
    body: String,
    links: Vec<String>,
}

/// Une requête et son décompte lexical attendu.
#[derive(Deserialize)]
struct ExpectedQuery {
    id: String,
    literal: String,
    expected_count: usize,
}

/// Racine du fichier `expected-counts-v1.json`.
#[derive(Deserialize)]
struct ExpectedCounts {
    queries: Vec<ExpectedQuery>,
}

/// Chemin d'une fixture, ancré sur `CARGO_MANIFEST_DIR` (indépendant du cwd).
fn fixture(nom: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(nom)
}

/// Charge le corpus JSONL (une note par ligne).
fn charger_corpus() -> Vec<CorpusNote> {
    let path = fixture("corpus-synthetique-v1.jsonl");
    let contenu = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("lecture corpus {} : {e}", path.display()));
    contenu
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("désérialisation d'une note du corpus"))
        .collect()
}

/// Charge les décomptes attendus.
fn charger_attendus() -> ExpectedCounts {
    let path = fixture("expected-counts-v1.json");
    let contenu = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("lecture décomptes {} : {e}", path.display()));
    serde_json::from_str(&contenu).expect("désérialisation des décomptes")
}

/// Décompte de notes dont le corps contient le littéral (même logique que le générateur).
fn compter(corpus: &[CorpusNote], literal: &str) -> usize {
    corpus.iter().filter(|n| n.body.contains(literal)).count()
}

#[test]
fn chaque_attendu_se_recalcule_depuis_le_corpus() {
    let corpus = charger_corpus();
    let attendus = charger_attendus();
    assert!(!attendus.queries.is_empty(), "jeu d'attendus vide");
    for q in &attendus.queries {
        let recalcule = compter(&corpus, &q.literal);
        assert_eq!(
            recalcule, q.expected_count,
            "requête {} (`{}`) : attendu {}, le corpus en contient {recalcule}",
            q.id, q.literal, q.expected_count
        );
    }
}

#[test]
fn cardinalites_plantees_sont_6_8_15() {
    let corpus = charger_corpus();
    assert_eq!(corpus.len(), 200, "le corpus doit contenir 200 notes");
    assert_eq!(
        compter(&corpus, "cargo-semver-checks"),
        6,
        "cargo-semver-checks"
    );
    assert_eq!(compter(&corpus, "2.0.7"), 8, "2.0.7");
    assert_eq!(
        compter(&corpus, "version:gradatum/2.1.0"),
        15,
        "version:gradatum/2.1.0"
    );
}

/// Tokenise un corps comme `unicode61` (minuscules, suite d'alphanumériques/`_`)
/// — suffisant pour affirmer qu'un jeton apparaît indépendamment de la casse ou
/// de la ponctuation adjacente (« Or, » → jeton `or`).
fn a_le_jeton(corps: &str, jeton: &str) -> bool {
    corps
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|t| t.eq_ignore_ascii_case(jeton))
}

/// F-162 critère 4 — la disjonction `2.0.7 OR 2.0.6` a un décompte attendu NON NUL.
///
/// Post-fix, `build_fts_query` rend `"2.0.7" "OR" "2.0.6"` — un ET implicite des
/// TROIS jetons : le mot `or` doit littéralement figurer dans le corps. Cette
/// co-occurrence est plantée dans le corpus (notes [46,54)) : 2.0.7 ET `or` ET
/// 2.0.6 dans exactement 8 notes. C'est le jeton `or` qui rendait 0 partout sur
/// le vault réel (« absence prouvée » alors que 2.0.7 seul rendait 5).
#[test]
fn disjonction_or_a_un_attendu_non_nul() {
    let corpus = charger_corpus();
    // Le jeton `or` (mot littéral, pas un opérateur) : présent uniquement dans
    // les 8 notes qui portent aussi 2.0.7 ET 2.0.6.
    let cooc = corpus
        .iter()
        .filter(|n| {
            n.body.contains("2.0.7") && a_le_jeton(&n.body, "or") && n.body.contains("2.0.6")
        })
        .count();
    assert_eq!(
        cooc, 8,
        "la co-occurrence 2.0.7 + `or` + 2.0.6 doit valoir 8 (notes [46,54))"
    );
    // Aucune note ne porte `or` hors de cet ensemble — le décompte est exact,
    // pas un minorant.
    let or_total = corpus.iter().filter(|n| a_le_jeton(&n.body, "or")).count();
    assert_eq!(
        or_total, 8,
        "le jeton `or` ne doit exister que dans les 8 notes [46,54)"
    );
    // L'attendu doit être non nul : un attendu nul serait identique avant et
    // après le correctif et ne démontrerait rien.
    assert!(
        cooc > 0,
        "l'attendu doit être non nul — un attendu nul ne prouverait rien"
    );
}

#[test]
fn distribution_de_degre_entrant_des_corps_est_conforme() {
    let corpus = charger_corpus();
    // Degré ENTRANT (backlinks) : pour chaque note, nombre d'autres notes dont le
    // corps la cite en wikilink — exactement ce que consomme `pagerank_factor`.
    // Extraction depuis les corps avec la fonction de production.
    let mut entrant: BTreeMap<String, usize> = BTreeMap::new();
    for n in &corpus {
        entrant.entry(n.id.clone()).or_insert(0);
    }
    for n in &corpus {
        for cible in extract_wikilinks(&n.body) {
            *entrant.entry(cible).or_insert(0) += 1;
        }
    }

    let mut hist: BTreeMap<usize, usize> = BTreeMap::new();
    for deg in entrant.values() {
        *hist.entry(*deg).or_insert(0) += 1;
    }

    let attendu: BTreeMap<usize, usize> = [(0usize, 164usize), (1, 20), (3, 15), (10, 1)]
        .into_iter()
        .collect();
    assert_eq!(
        hist, attendu,
        "distribution de degré entrant = {hist:?}, attendu {attendu:?}"
    );

    // 82 % exactement à degré entrant 0, maximum 10.
    assert_eq!(
        hist[&0] * 100 / corpus.len(),
        82,
        "82 % des notes doivent être à degré entrant 0"
    );
    assert_eq!(
        *hist.keys().max().expect("histogramme non vide"),
        10,
        "degré entrant maximum = 10"
    );
}

#[test]
fn distribution_de_degre_sortant_des_corps_est_conforme() {
    let corpus = charger_corpus();
    // Degré sortant : nombre de wikilinks émis dans le corps de chaque note.
    // Construit symétriquement dans le générateur — doit rester {0: 164, 1: 20, 3: 15, 10: 1}.
    let mut hist: BTreeMap<usize, usize> = BTreeMap::new();
    for n in &corpus {
        let deg = extract_wikilinks(&n.body).len();
        *hist.entry(deg).or_insert(0) += 1;
    }
    let attendu: BTreeMap<usize, usize> = [(0usize, 164usize), (1, 20), (3, 15), (10, 1)]
        .into_iter()
        .collect();
    assert_eq!(
        hist, attendu,
        "distribution de degré sortant = {hist:?}, attendu {attendu:?}"
    );
}

#[test]
fn champ_links_et_wikilinks_du_corps_concordent() {
    let corpus = charger_corpus();
    // Le champ JSON `links` est une déclaration redondante des wikilinks du corps :
    // les deux sources doivent dire exactement la même chose, pour chaque note.
    for n in &corpus {
        assert_eq!(
            extract_wikilinks(&n.body),
            n.links,
            "note {} : wikilinks du corps ≠ champ links",
            n.id
        );
    }

    // 36 notes portent au moins un lien — que ce soit dans le champ ou dans le corps.
    let avec_links = corpus.iter().filter(|n| !n.links.is_empty()).count();
    let avec_wikilinks = corpus
        .iter()
        .filter(|n| !extract_wikilinks(&n.body).is_empty())
        .count();
    assert_eq!(avec_links, 36, "36 notes avec champ links non vide");
    assert_eq!(
        avec_wikilinks, 36,
        "36 notes avec au moins un [[..]] dans le corps"
    );
}

/// GARDE DE CANONICITÉ — chaque note du corpus porte une section canonique.
///
/// Garde la régression de 759a610e : le correctif 8df872ff avait corrigé le
/// FICHIER généré (`notes` → `debug`) mais pas son GÉNÉRATEUR ; la
/// régénération suivante avait réécrit les 50 notes de `debug` vers `notes`
/// (section INCONNUE, absente de `Section::ALL`) en silence, car aucun test
/// n'assertait la canonicité des sections du corpus.
///
/// La source d'autorité est `gradatum_core::section` (`Section::ALL` via
/// `Section::from_canonical_str`) — jamais une liste en dur dans ce test, qui
/// recréerait exactement la classe de bug corrigée ici. Depuis F-261, une
/// section hors canon retombe sur le trust neutre 0.50 et un doc_kind `Static`
/// de repli : ce test verrouille aussi que le corpus reste mesurable par le
/// banc de pertinence.
#[test]
fn corpus_sections_toutes_canoniques() {
    let corpus = charger_corpus();
    assert!(
        !corpus.is_empty(),
        "corpus vide — aucune section à vérifier"
    );
    for n in &corpus {
        assert!(
            Section::from_canonical_str(&n.section).is_some(),
            "note {} : section {:?} hors canon (absente de Section::ALL) — \
             une régénération a-t-elle réécrit une section canonique en \
             section inconnue, comme 759a610e l'avait fait pour debug→notes ?",
            n.id,
            n.section
        );
    }
}
