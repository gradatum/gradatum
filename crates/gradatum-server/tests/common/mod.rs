//! Fixtures communes aux tests d'intégration de gradatum-server.
//!
//! La file legacy `SqliteQueue` (`jobs_v2`) a été supprimée en 2.1.0 (F-177) :
//! les tests d'intégration qui nécessitaient une queue in-memory legacy ont été
//! réalignés — aucun helper de queue legacy ne subsiste ici.
