//! `gen_synthetic_corpus` — génère le corpus synthétique F-162 lot 0 (Task 2).
//!
//! ## Rôle
//!
//! Produit deux artefacts commités sous `crates/gradatum-bench/fixtures/` :
//!
//! - `corpus-synthetique-v1.jsonl` — 200 notes synthétiques, une par ligne ;
//! - `expected-counts-v1.json` — décomptes lexicaux **dérivés par construction**
//!   du corpus (`{queries:[{id, literal, expected_count}]}`), sans exécuter aucun
//!   moteur de recherche.
//!
//! ## Confidentialité (C2 ABSOLUE)
//!
//! Aucune donnée du vault personnel n'entre dans les fixtures :
//! - **ULIDs** : dérivés d'un compteur via [`Ulid::from_parts`] avec un timestamp
//!   nul (`0`) — les vrais ULIDs du vault portent un timestamp récent (`01K…`,
//!   `01M…`), le préfixe `0000000000` garantit l'absence de collision ;
//! - **titres** : gabarits synthétiques, jamais un titre réel ;
//! - **corps** : texte neutre + termes ponctués plantés à cardinalité connue.
//!
//! ## Contraintes normatives reproduites (valeurs réelles mesurées le 2026-08-23)
//!
//! - `cargo-semver-checks` planté dans exactement 6 notes ;
//! - `2.0.7` dans 8 ; `version:gradatum/2.1.0` dans 15 ;
//! - graphe de liens à distribution de degré explicite : 82 % des notes à degré 0,
//!   degrés {0, 1, 3, 10} tous représentés, maximum 10 — la forme du corpus réel,
//!   sans laquelle le recalibrage de `NORM_CONST` (Task 6) n'est pas mesurable.
//!   Les liens sont émis DANS LE CORPS sous forme de wikilinks `[[ULID]]` (la forme
//!   que lit l'ingestion de production), avec une construction **symétrique** :
//!   distribution de degré entrant == sortant == {0: 164, 1: 20, 3: 15, 10: 1}.
//!
//! Le générateur **échoue** si l'une de ces cardinalités n'est pas atteinte
//! (garde anti-collision de sous-chaîne).
//!
//! ## Usage
//!
//! ```bash
//! cargo run -p gradatum-bench --bin gen_synthetic_corpus
//! ```
//!
//! Réécrit les deux fixtures de façon **déterministe** : deux exécutions produisent
//! des octets identiques (aucun aléa, aucun horodatage).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use ulid::Ulid;

/// Nombre total de notes du corpus.
const N_NOTES: usize = 200;

// Cardinalités des termes ponctués plantés (mesurées le 2026-08-23 sur le vault).
const CARD_SEMVER: usize = 6;
const CARD_207: usize = 8;
const CARD_VERSION_TAG: usize = 15;

// Distribution de degré visée — symétrique : entrant (backlinks, ce que consomme
// `pagerank_factor`) == sortant (liens émis) == {0: 164, 1: 20, 3: 15, 10: 1}.
// 82 % à 0, degrés {0, 1, 3, 10} représentés, maximum 10.
const DEG10_NOTES: usize = 1; // hub — degré entrant et sortant 10
const DEG3_NOTES: usize = 15; // notes à degré 3
const DEG1_NOTES: usize = 20; // notes à degré 1
// degré 0 = N_NOTES - (1 + 15 + 20) = 164 → 164 / 200 = 82 %.

// Termes ponctués plantés — littéraux exacts recherchés.
const LIT_SEMVER: &str = "cargo-semver-checks";
const LIT_207: &str = "2.0.7";
const LIT_VERSION_TAG: &str = "version:gradatum/2.1.0";
// Contrôles : un terme ubiquitaire (présent dans chaque note) et un terme absent.
const LIT_UBIQUITAIRE: &str = "synthétique";
const LIT_ABSENT: &str = "terme-absent-jamais-plante";
// Disjonction F-162 critère 4 : jeton `or` planté littéralement (la phrase
// « Or, … » est tokenisée `or` par unicode61) + variante 2.0.6, dans les MÊMES
// notes que 2.0.7 ([46,54)). Post-fix, la requête « 2.0.7 OR 2.0.6 » devient
// l'ET implicite « "2.0.7" "OR" "2.0.6" » → attendu non nul (8), mesuré sur le
// corpus réel comme NON nul par construction.
const LIT_206: &str = "2.0.6";
const CARD_206: usize = 8;
const LIT_OR_PHRASE: &str = " Or, la variante 2.0.6 reste citée pour la co-occurrence.";

/// Note synthétique sérialisée en une ligne JSONL.
#[derive(Serialize)]
struct CorpusNote {
    /// ULID synthétique (timestamp 0 + compteur) — jamais un ULID réel.
    id: String,
    /// Titre synthétique (gabarit) — jamais un titre réel.
    title: String,
    /// Section thématique synthétique.
    section: String,
    /// Corps Markdown : filler neutre + termes plantés éventuels.
    body: String,
    /// Liens sortants (ULIDs synthétiques d'autres notes) — déclaration redondante
    /// des wikilinks du corps, croisée par le test de cohérence.
    links: Vec<String>,
}

/// Une requête et son décompte attendu, dérivé par construction du corpus.
#[derive(Serialize)]
struct ExpectedQuery {
    id: String,
    literal: String,
    expected_count: usize,
}

/// Racine du fichier `expected-counts-v1.json`.
#[derive(Serialize)]
struct ExpectedCounts {
    queries: Vec<ExpectedQuery>,
}

/// ULID synthétique dérivé du compteur : timestamp 0, aléa = `i`.
///
/// Le timestamp nul rend le préfixe `0000000000`, distinct de tout ULID réel du
/// vault (timestamps récents) — collision impossible.
fn id_synthetique(i: usize) -> String {
    Ulid::from_parts(0, i as u128).to_string()
}

/// Cibles des liens sortants de la note d'indice `i` — construction symétrique.
///
/// Le graphe a la **même** distribution de degré qu'on le lise en sortant (liens
/// émis, croisés par le test de cohérence avec le champ `links`) ou en entrant
/// (backlinks, ce que consomme `pagerank_factor`) : {0: 164, 1: 20, 3: 15, 10: 1}.
///
/// | indices | degré | rôle |
/// |---|---|---|
/// | 0 | 10 | hub — émet vers les notes 16..25 **et** reçoit de ces mêmes notes |
/// | 1..=15 | 3 | cycle tri-connexe — chaque note émet vers les 3 suivantes du groupe, reçoit des 3 précédentes |
/// | 16..=25 | 1 | groupe A — émet vers le hub, reçoit du hub |
/// | 26..=35 | 1 | groupe B — cycle simple au sein du groupe |
/// | 36..=199 | 0 | isolées |
fn cibles_liens(i: usize) -> Vec<usize> {
    let debut_deg3 = DEG10_NOTES; // 1
    let debut_deg1 = debut_deg3 + DEG3_NOTES; // 16
    let debut_groupe_b = debut_deg1 + DEG1_NOTES / 2; // 26 — la moitié du groupe degré 1
    let fin_deg1 = debut_deg1 + DEG1_NOTES; // 36

    if i < DEG10_NOTES {
        // hub → groupe A : 10 cibles (degré sortant 10)
        (debut_deg1..debut_groupe_b).collect()
    } else if i < debut_deg1 {
        // notes 1..=15 : cycle tri-connexe — degré 3
        (1..=3)
            .map(|k| debut_deg3 + (i - debut_deg3 + k) % DEG3_NOTES)
            .collect()
    } else if i < debut_groupe_b {
        // notes 16..=25 : groupe A → hub — degré 1
        vec![0]
    } else if i < fin_deg1 {
        // notes 26..=35 : groupe B, cycle simple — degré 1
        vec![if i + 1 < fin_deg1 {
            i + 1
        } else {
            debut_groupe_b
        }]
    } else {
        Vec::new() // notes 36..=199 : isolées
    }
}

/// Sections synthétiques cyclées (vocabulaire neutre, pas de donnée personnelle).
///
/// `debug` et pas `notes` : la section doit être CANONIQUE (membre de
/// `gradatum_core::section::Section::ALL`) pour que le facteur de confiance F-261
/// (`trust_for_section_str`) porte une valeur réelle. Le correctif 8df872ff avait
/// corrigé le FICHIER généré sans le générateur ; la régénération 759a610e avait
/// réécrit `debug` → `notes` en silence (section inconnue → neutre 0.50). Le test
/// `corpus_sections_toutes_canoniques` garde cette régression.
const SECTIONS: [&str; 4] = ["debug", "reference", "architecture", "decisions"];
/// Thèmes de titre synthétiques (neutres, sans chaîne de version).
const THEMES: [&str; 5] = ["build", "recherche", "graphe", "banc", "corpus"];

fn main() -> Result<()> {
    let ids: Vec<String> = (0..N_NOTES).map(id_synthetique).collect();

    let mut notes: Vec<CorpusNote> = Vec::with_capacity(N_NOTES);
    for i in 0..N_NOTES {
        let theme = THEMES[i % THEMES.len()];
        let section = SECTIONS[i % SECTIONS.len()];
        let title = format!("Note synthétique {i:03} — thème {theme}");

        // Filler neutre — contient toujours `synthétique` (terme ubiquitaire de
        // contrôle) et jamais aucun des littéraux plantés.
        let mut body = format!(
            "# {title}\n\nNote synthétique numéro {i} pour le banc de pertinence F-162. \
             Contenu neutre sans donnée du vault personnel, section {section}.",
        );

        // Termes ponctués plantés — ensembles d'indices disjoints, cardinalités connues.
        // [40, 46) → cargo-semver-checks (6) ; [46, 54) → 2.0.7 (8) ;
        // [54, 69) → version:gradatum/2.1.0 (15).
        if (40..46).contains(&i) {
            body.push_str(
                " Compatibilité API vérifiée avec l'outil cargo-semver-checks avant publication.",
            );
        }
        if (46..54).contains(&i) {
            body.push_str(" Version de référence 2.0.7 citée pour le décompte lexical.");
            // Critère 4 — co-occurrence des TROIS jetons de la disjonction :
            // « 2.0.7 », « or » (mot littéral, pas un opérateur) et « 2.0.6 ».
            body.push_str(LIT_OR_PHRASE);
        }
        if (54..69).contains(&i) {
            body.push_str(" Étiquette version:gradatum/2.1.0 plantée pour la cardinalité connue.");
        }

        let links: Vec<String> = cibles_liens(i)
            .into_iter()
            .map(|j| ids[j].clone())
            .collect();

        // Wikilinks émis DANS LE CORPS — forme acceptée par l'ingestion de production
        // (`gradatum_curator::wikilinks::WIKILINK_RE` : `[[target]]` ou `[[target|alias]]`).
        // Le champ JSON `links` reste une déclaration redondante, croisée par le test
        // de cohérence ; il cesse d'être la seule source des liens.
        for target in &links {
            body.push_str(&format!(" [[{target}]]"));
        }

        notes.push(CorpusNote {
            id: ids[i].clone(),
            title,
            section: section.to_string(),
            body,
            links,
        });
    }

    // Décomptes dérivés par construction : on scanne le corpus, sans moteur.
    let compter = |lit: &str| notes.iter().filter(|n| n.body.contains(lit)).count();
    let c_semver = compter(LIT_SEMVER);
    let c_207 = compter(LIT_207);
    let c_tag = compter(LIT_VERSION_TAG);
    let c_ubi = compter(LIT_UBIQUITAIRE);
    let c_absent = compter(LIT_ABSENT);
    let c_206 = compter(LIT_206);
    let c_or_phrase = compter(LIT_OR_PHRASE);

    // Gardes anti-collision de sous-chaîne : si une cardinalité normative n'est pas
    // atteinte, le corpus est faux — échouer plutôt que d'écrire un artefact menteur.
    if c_semver != CARD_SEMVER {
        bail!("cardinalité `{LIT_SEMVER}` = {c_semver}, attendu {CARD_SEMVER}");
    }
    if c_207 != CARD_207 {
        bail!("cardinalité `{LIT_207}` = {c_207}, attendu {CARD_207}");
    }
    if c_tag != CARD_VERSION_TAG {
        bail!("cardinalité `{LIT_VERSION_TAG}` = {c_tag}, attendu {CARD_VERSION_TAG}");
    }
    if c_ubi != N_NOTES {
        bail!("terme ubiquitaire `{LIT_UBIQUITAIRE}` = {c_ubi}, attendu {N_NOTES}");
    }
    if c_absent != 0 {
        bail!("terme de contrôle `{LIT_ABSENT}` = {c_absent}, attendu 0");
    }
    if c_206 != CARD_206 {
        bail!("cardinalité `{LIT_206}` = {c_206}, attendu {CARD_206}");
    }
    if c_or_phrase != CARD_206 {
        bail!(
            "phrase `or` plantée = {c_or_phrase} notes, attendu {CARD_206} — la disjonction \
             « 2.0.7 OR 2.0.6 » exige le jeton `or` dans exactement les notes portant 2.0.7"
        );
    }

    let expected = ExpectedCounts {
        queries: vec![
            ExpectedQuery {
                id: "q01".into(),
                literal: LIT_SEMVER.into(),
                expected_count: c_semver,
            },
            ExpectedQuery {
                id: "q02".into(),
                literal: LIT_207.into(),
                expected_count: c_207,
            },
            ExpectedQuery {
                id: "q03".into(),
                literal: LIT_VERSION_TAG.into(),
                expected_count: c_tag,
            },
            ExpectedQuery {
                id: "q04".into(),
                literal: LIT_UBIQUITAIRE.into(),
                expected_count: c_ubi,
            },
            ExpectedQuery {
                id: "q05".into(),
                literal: LIT_ABSENT.into(),
                expected_count: c_absent,
            },
            ExpectedQuery {
                id: "q06".into(),
                literal: LIT_206.into(),
                expected_count: c_206,
            },
        ],
    };

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    fs::create_dir_all(&dir).with_context(|| format!("création {}", dir.display()))?;

    // Corpus JSONL — une note par ligne, ordre stable (indice croissant).
    let mut jsonl = String::new();
    for n in &notes {
        jsonl.push_str(&serde_json::to_string(n).context("sérialisation note")?);
        jsonl.push('\n');
    }
    let corpus_path = dir.join("corpus-synthetique-v1.jsonl");
    fs::write(&corpus_path, jsonl)
        .with_context(|| format!("écriture {}", corpus_path.display()))?;

    // Décomptes — JSON indenté, terminé par un saut de ligne.
    let counts_path = dir.join("expected-counts-v1.json");
    let mut counts_json =
        serde_json::to_string_pretty(&expected).context("sérialisation décomptes")?;
    counts_json.push('\n');
    fs::write(&counts_path, counts_json)
        .with_context(|| format!("écriture {}", counts_path.display()))?;

    println!("corpus : {} notes → {}", notes.len(), corpus_path.display());
    println!(
        "décomptes : {} requêtes → {} (semver={c_semver}, 2.0.7={c_207}, 2.0.6={c_206}, tag={c_tag}, ubi={c_ubi}, absent={c_absent})",
        expected.queries.len(),
        counts_path.display()
    );
    Ok(())
}
