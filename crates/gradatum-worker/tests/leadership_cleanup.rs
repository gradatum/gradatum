//! Tests de libération de lease leadership au shutdown (patch.6).
//!
//! Vérifie que `LeaderElection::release()` :
//! - Supprime uniquement la row appartenant au holder appelant (race-safe).
//! - Est un no-op si le holder n'a pas de row (double-release, expiry).
//! - Ne supprime pas la row d'un concurrent si la lease a changé de mains.

use std::time::Duration;

use gradatum_db_sqlite::open_queue_db;
use gradatum_worker::leader::{LeaderConfig, LeaderElection};

/// Ouvre une base SQLite WAL avec le schéma queue appliqué.
async fn make_db(path: &std::path::Path) -> gradatum_db_sqlite::QueueDb {
    let db = open_queue_db(path).await.unwrap();
    db.with_conn(|conn| conn.execute_batch(gradatum_queue::schema::SCHEMA_V1))
        .await
        .unwrap();
    db
}

/// Compte les lignes du slot de leadership (0 ou 1).
async fn leadership_row_count(db: &gradatum_db_sqlite::QueueDb) -> i64 {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM worker_leadership WHERE slot = 0",
            [],
            |row| row.get(0),
        )
    })
    .await
    .unwrap()
}

/// Config rapide pour les tests : lease courte pour éviter d'attendre les TTL.
fn fast_cfg() -> LeaderConfig {
    LeaderConfig {
        renew_every: Duration::from_millis(200),
        expires_after: Duration::from_millis(600),
    }
}

/// Un leader acquiert puis libère : la row est supprimée.
#[tokio::test]
async fn release_removes_own_row() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = make_db(tmp.path()).await;

    let el = LeaderElection::new(db.clone(), fast_cfg()).await.unwrap();
    assert!(el.try_acquire().await.unwrap(), "doit acquérir");

    // Vérifier que la row existe avant release
    let count_before = leadership_row_count(&db).await;
    assert_eq!(count_before, 1, "row doit exister avant release");

    el.release().await.unwrap();

    let count_after = leadership_row_count(&db).await;
    assert_eq!(count_after, 0, "row doit être supprimée après release");
}

/// Double release : le second appel est un no-op (row déjà absente).
#[tokio::test]
async fn release_is_idempotent() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = make_db(tmp.path()).await;

    let el = LeaderElection::new(db.clone(), fast_cfg()).await.unwrap();
    assert!(el.try_acquire().await.unwrap(), "doit acquérir");

    el.release().await.unwrap();
    // Deuxième appel : aucune erreur, aucun effet de bord.
    el.release().await.unwrap();

    let count = leadership_row_count(&db).await;
    assert_eq!(count, 0, "row toujours absente après double release");
}

/// Race-safe : el_a libère sa lease, el_b (qui a repris entre-temps) n'est pas affecté.
///
/// Scénario : el_a est leader → el_a expire → el_b acquiert → el_a appelle release()
/// (cas SIGTERM tardif) → la row de el_b NE DOIT PAS être supprimée.
#[tokio::test]
async fn release_only_self_not_other_holder() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = make_db(tmp.path()).await;

    // el_a acquiert avec lease courte (300ms)
    let cfg_short = LeaderConfig {
        renew_every: Duration::from_millis(100),
        expires_after: Duration::from_millis(300),
    };
    let el_a = LeaderElection::new(db.clone(), cfg_short).await.unwrap();
    assert!(el_a.try_acquire().await.unwrap(), "el_a doit acquérir");

    // Attendre l'expiry de el_a sans renouvellement
    tokio::time::sleep(Duration::from_millis(400)).await;

    // el_b acquiert (el_a expiré)
    let el_b = LeaderElection::new(db.clone(), fast_cfg()).await.unwrap();
    assert!(
        el_b.try_acquire().await.unwrap(),
        "el_b doit acquérir après expiry de el_a"
    );

    // el_a appelle release() tardivement (son holder ne correspond plus à la row)
    el_a.release().await.unwrap();

    // La row de el_b doit être intacte
    let count = leadership_row_count(&db).await;
    assert_eq!(
        count, 1,
        "row el_b doit rester après release() tardif de el_a"
    );
}

/// Release sans acquire préalable : no-op sans panique ni erreur.
#[tokio::test]
async fn release_without_acquire_is_noop() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db = make_db(tmp.path()).await;

    let el = LeaderElection::new(db.clone(), fast_cfg()).await.unwrap();
    // Pas d'acquire — release ne doit pas paniquer ni retourner Err.
    el.release().await.unwrap();

    let count = leadership_row_count(&db).await;
    assert_eq!(count, 0, "aucune row créée par release sans acquire");
}
