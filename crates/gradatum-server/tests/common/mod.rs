//! Fixtures communes aux tests d'intégration de gradatum-server.
//!
//! Remplace `NoopQueue` (supprimé de `stubs.rs` en T1 P2.0c) par une queue
//! SQLite in-memory appropriée aux tests — isolation totale, pas de fichier sur disque.

use std::sync::Arc;

use gradatum_queue::{Queue, SqliteQueue};

pub mod test_app_jobs;

/// Retourne une `SqliteQueue` in-memory prête à l'emploi pour les tests.
///
/// La base est créée en mémoire (`:memory:`) — isolation totale entre tests.
/// Peut être utilisée comme `Arc<dyn Queue>` dans les constructeurs `AppState`.
///
/// # Panics
///
/// Panique si sqlx ne peut pas ouvrir la base en mémoire — ne devrait jamais
/// arriver dans un environnement de test standard.
#[allow(dead_code)] // API utilitaire partagée — utilisée dans d'autres test binaires du crate.
pub fn test_queue() -> Arc<dyn Queue> {
    let queue = tokio::runtime::Handle::current()
        .block_on(SqliteQueue::in_memory())
        .expect("SqliteQueue::in_memory() — doit réussir dans les tests");
    Arc::new(queue)
}
