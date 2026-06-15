//! Tests de libération de lease leadership au shutdown (patch.6).
//!
//! Vérifie que `LeaderElection::release()` :
//! - Supprime uniquement la row appartenant au holder appelant (race-safe).
//! - Est un no-op si le holder n'a pas de row (double-release, expiry).
//! - Ne supprime pas la row d'un concurrent si la lease a changé de mains.

use std::sync::Arc;
use std::time::Duration;

use gradatum_worker::leader::{LeaderConfig, LeaderElection};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

/// Ouvre un pool SQLite WAL avec le schéma queue appliqué.
async fn make_pool(path: &std::path::Path) -> sqlx::SqlitePool {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::query(gradatum_queue::schema::SCHEMA_V1)
        .execute(&pool)
        .await
        .unwrap();
    pool
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
    let pool = Arc::new(make_pool(tmp.path()).await);

    let el = LeaderElection::new(pool.clone(), fast_cfg()).await.unwrap();
    assert!(el.try_acquire().await.unwrap(), "doit acquérir");

    // Vérifier que la row existe avant release
    let count_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM worker_leadership WHERE slot = 0")
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
    assert_eq!(count_before, 1, "row doit exister avant release");

    el.release().await.unwrap();

    let count_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM worker_leadership WHERE slot = 0")
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
    assert_eq!(count_after, 0, "row doit être supprimée après release");
}

/// Double release : le second appel est un no-op (row déjà absente).
#[tokio::test]
async fn release_is_idempotent() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let pool = Arc::new(make_pool(tmp.path()).await);

    let el = LeaderElection::new(pool.clone(), fast_cfg()).await.unwrap();
    assert!(el.try_acquire().await.unwrap(), "doit acquérir");

    el.release().await.unwrap();
    // Deuxième appel : aucune erreur, aucun effet de bord.
    el.release().await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM worker_leadership WHERE slot = 0")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(count, 0, "row toujours absente après double release");
}

/// Race-safe : el_a libère sa lease, el_b (qui a repris entre-temps) n'est pas affecté.
///
/// Scénario : el_a est leader → el_a expire → el_b acquiert → el_a appelle release()
/// (cas SIGTERM tardif) → la row de el_b NE DOIT PAS être supprimée.
#[tokio::test]
async fn release_only_self_not_other_holder() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let pool = Arc::new(make_pool(tmp.path()).await);

    // el_a acquiert avec lease courte (300ms)
    let cfg_short = LeaderConfig {
        renew_every: Duration::from_millis(100),
        expires_after: Duration::from_millis(300),
    };
    let el_a = LeaderElection::new(pool.clone(), cfg_short).await.unwrap();
    assert!(el_a.try_acquire().await.unwrap(), "el_a doit acquérir");

    // Attendre l'expiry de el_a sans renouvellement
    tokio::time::sleep(Duration::from_millis(400)).await;

    // el_b acquiert (el_a expiré)
    let el_b = LeaderElection::new(pool.clone(), fast_cfg()).await.unwrap();
    assert!(
        el_b.try_acquire().await.unwrap(),
        "el_b doit acquérir après expiry de el_a"
    );

    // el_a appelle release() tardivement (son holder ne correspond plus à la row)
    el_a.release().await.unwrap();

    // La row de el_b doit être intacte
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM worker_leadership WHERE slot = 0")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "row el_b doit rester après release() tardif de el_a"
    );
}

/// Release sans acquire préalable : no-op sans panique ni erreur.
#[tokio::test]
async fn release_without_acquire_is_noop() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let pool = Arc::new(make_pool(tmp.path()).await);

    let el = LeaderElection::new(pool.clone(), fast_cfg()).await.unwrap();
    // Pas d'acquire — release ne doit pas paniquer ni retourner Err.
    el.release().await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM worker_leadership WHERE slot = 0")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(count, 0, "aucune row créée par release sans acquire");
}
