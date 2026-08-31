//! F-207 critère 3 — `project-map scope` rend un VERDICT, pas seulement une ligne.
//!
//! Ces tests invoquent le VRAI binaire (`CARGO_BIN_EXE_gradatum-admin`) et mesurent son
//! code de sortie, comme `api_key_cmd.rs` : la décision vit dans la commande, pas dans
//! le module — la tester au seul niveau de `scope_exit_code` ne prouverait rien sur le
//! CLI livré, qui est ce que le consommateur (`homelab-audit.sh`, bloc A7.2) exécute.
//!
//! ## Ce que ce fichier couvre, et ce qu'il ne couvre pas
//!
//! Couvert de bout en bout : **écart nul ⇒ code 0**, avec la ligne de réconciliation
//! toujours présente sur stdout. C'est le contrat dont dépend le consommateur
//! aujourd'hui, et le seul chemin que l'ajout de `std::process::exit` pouvait casser.
//!
//! NON couvert de bout en bout, et déclaré plutôt que caché : **écart non nul ⇒ code 2**.
//! `project_scope_from_conn` classe chaque ligne dans exactement un panier et pose
//! `total_count = rows.len()` : l'écart est nul *par construction*, aucune base ne peut
//! le rendre non nul. Le fabriquer exigerait une trappe de test dans un CLI de
//! production — une surface neuve pour verdir un test, refusée. Ce sens est couvert par
//! les tests unitaires `exit_code_is_unreconciled_when_*` de
//! `crates/gradatum-admin/src/project_map_scope.rs`.
//!
//! Le pont entre les deux — vérifié par mutation, pas supposé : forcer
//! `scope_exit_code` à rendre 2 en toutes circonstances fait rougir
//! `reconciled_scope_exits_zero` et `reconciled_scope_emits_no_alert`. Le binaire
//! **consulte** donc bien cette fonction ; les tests unitaires, eux, prouvent qu'elle
//! décide juste dans les deux sens.
//!
//! Écarté délibérément : un test comparant le code du binaire au verdict que la
//! bibliothèque rend sur la même base. Il reste vert sous les deux mutations — il
//! mesure la cohérence d'une chose avec elle-même, pas la valeur attendue.

use gradatum_admin::project_map_scope::{EXIT_SCOPE_RECONCILED, EXIT_SCOPE_UNRECONCILED};
use gradatum_core::paths::vault_index_path;
use rusqlite::Connection;
use tempfile::TempDir;

/// Corps d'une carte project-map portant les wikilinks typés lus par la commande.
fn card_body(project: &str, status: &str, version: &str) -> String {
    format!(
        "[[project:{project}]] [[status:{status}]] [[kind:FEATURE]] [[version:{project}/{version}]]\n\nItem de test."
    )
}

/// Matérialise `<root>/vault/.gradatum/index.db` avec le schéma aligné sur la prod
/// (migration 0001 + 0005 : `title` nullable, `created` en epoch ms) et y insère
/// `cards` cartes portant chacune un statut connu.
fn seed_index(root: &std::path::Path, cards: &[(&str, &str)]) {
    let db_path = vault_index_path(root);
    std::fs::create_dir_all(
        db_path
            .parent()
            .expect("index.db a toujours un répertoire parent"),
    )
    .expect("créer <root>/vault/.gradatum");

    let conn = Connection::open(&db_path).expect("ouvrir index.db de test");
    conn.execute_batch(
        "CREATE TABLE notes (
            id TEXT PRIMARY KEY,
            vault_id TEXT NOT NULL,
            section TEXT NOT NULL,
            body_text TEXT NOT NULL,
            title TEXT,
            status TEXT NOT NULL,
            created INTEGER NOT NULL DEFAULT 0
        )",
    )
    .expect("créer la table notes de test");

    for (i, (id, card_status)) in cards.iter().enumerate() {
        conn.execute(
            "INSERT INTO notes (id, vault_id, section, body_text, title, status, created)
             VALUES (?1, 'main', 'project-map', ?2, ?3, 'live', ?4)",
            rusqlite::params![
                id,
                card_body("gradatum", card_status, "2.0.6"),
                format!("Carte {id}"),
                i64::try_from(i).expect("index de test tient dans un i64"),
            ],
        )
        .expect("insérer une carte de test");
    }
}

/// Lance `gradatum-admin project-map scope --root <root> --vault main gradatum`.
fn run_scope(root: &std::path::Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .args(["project-map", "scope"])
        .arg("--root")
        .arg(root)
        .args(["--vault", "main", "gradatum"])
        .output()
        .expect("lancer le binaire gradatum-admin")
}

/// Écart nul ⇒ le binaire sort en 0.
///
/// Régression que ce test interdit : que l'ajout du verdict fasse échouer le cas
/// nominal. Le consommateur bascule en `_degraded` sur tout code non nul qu'il ne sait
/// pas discriminer, donc un 2 rendu ici ferait disparaître le contrôle entier.
#[test]
fn reconciled_scope_exits_zero() {
    let dir = TempDir::new().expect("tempdir");
    seed_index(
        dir.path(),
        &[
            ("n1", "OPEN"),
            ("n2", "DONE"),
            ("n3", "IN_PROGRESS"),
            ("n4", "OBSOLETE"),
        ],
    );

    let out = run_scope(dir.path());

    assert_eq!(
        out.status.code(),
        Some(EXIT_SCOPE_RECONCILED),
        "écart nul doit rendre {EXIT_SCOPE_RECONCILED} — obtenu {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// La ligne de réconciliation reste sur **stdout**, écart nul compris.
///
/// Le consommateur ne lit pas le code de sortie seul : il extrait la ligne d'écart.
/// Déplacer cette ligne sur stderr ou la conditionner au verdict la lui retirerait.
#[test]
fn reconciled_scope_still_prints_the_reconciliation_line() {
    let dir = TempDir::new().expect("tempdir");
    seed_index(dir.path(), &[("n1", "OPEN"), ("n2", "DONE")]);

    let stdout = String::from_utf8(run_scope(dir.path()).stdout).expect("stdout UTF-8");

    assert!(
        stdout.contains("gap total−sum: 0"),
        "la ligne d'écart doit rester sur stdout — obtenu :\n{stdout}"
    );
}

/// Aucune alerte sur stderr quand la réconciliation tient.
///
/// Un canal d'alerte qui parle aussi dans le cas sain n'a aucun pouvoir discriminant.
#[test]
fn reconciled_scope_emits_no_alert() {
    let dir = TempDir::new().expect("tempdir");
    seed_index(dir.path(), &[("n1", "DONE")]);

    let stderr = String::from_utf8(run_scope(dir.path()).stderr).expect("stderr UTF-8");

    assert!(
        !stderr.contains("ALERTE"),
        "aucune ALERTE ne doit être émise sur un écart nul — obtenu :\n{stderr}"
    );
}

/// Index absent ⇒ échec d'exécution, et ce code ne doit PAS être celui du verdict.
///
/// C'est la moitié qui rend le verdict exploitable : le consommateur doit pouvoir
/// séparer « je n'ai pas pu mesurer » de « j'ai mesuré un écart ».
#[test]
fn missing_index_exits_with_a_code_distinct_from_the_verdict() {
    let dir = TempDir::new().expect("tempdir");
    // Aucun index.db créé : `project_scope` remonte une erreur anyhow depuis `main`.

    let code = run_scope(dir.path()).status.code();

    assert_ne!(
        code,
        Some(EXIT_SCOPE_RECONCILED),
        "un index absent ne doit pas passer pour un succès"
    );
    assert_ne!(
        code,
        Some(EXIT_SCOPE_UNRECONCILED),
        "un échec d'exécution ne doit pas se confondre avec le verdict « non réconcilié »"
    );
}
