//! Helpers tests partagés Task 13 (Phase 2.x.4 alpha.13 — council Round 2 rev2.1).
//!
//! Pattern TDD : `#[path = "helpers/mod.rs"] mod helpers;` au début de chaque
//! fichier test integration touchant le worker B5 (correctif A-rev2-3).
//!
//! Fournit un constructeur `test_dispatcher_with_index` qui assemble :
//! - `SqliteQueue::in_memory()` (queue vide)
//! - `Vault::create(TempDir)` (registry de notes)
//! - `CuratorPipeline::new()` (heuristique défaut)
//! - `SqliteIndex` partagé entre dispatcher (`with_index`) et tests (vérif `note_links`)
//!
//! Le `SqliteIndex` retourné est `vault.index().clone()` — donc le même Arc partagé
//! avec le `Dispatcher` via `with_index`. Les tests peuvent vérifier le résultat
//! des wikilinks B5 directement via `idx.get_all_links(...)`.

#![allow(dead_code)]

use std::sync::Arc;

use bincode::config::standard as bincode_std;
use gradatum_core::scope::VaultId;
use gradatum_index::SqliteIndex;
use gradatum_queue::{NewJob, Queue, SqliteQueue};
use gradatum_vault::Vault;
use gradatum_worker::dispatch::{Dispatcher, NoopAuditSink};
use tempfile::TempDir;

/// Bundle retourné par `test_dispatcher_with_index` — garde les ressources vivantes
/// pour la durée du test (TempDir, Vault, queue, dispatcher, index).
///
/// Le `TempDir` n'est PAS supprimé tant que le bundle est vivant — sécurité
/// pour éviter qu'un drop prématuré n'efface la base SQLite avant l'assertion.
pub struct DispatcherFixture {
    pub dispatcher: Dispatcher,
    pub queue: Arc<SqliteQueue>,
    pub vault: Arc<Vault>,
    pub index: Arc<SqliteIndex>,
    pub _tmp: TempDir,
}

/// Construit un dispatcher avec vault, queue, curator et index partagés.
///
/// L'index est cloné depuis `vault.index()` pour garantir que `Dispatcher::with_index`
/// et les assertions de tests opèrent sur la même base SQLite (sans fichier double).
pub async fn test_dispatcher_with_index() -> DispatcherFixture {
    let tmp = TempDir::new().expect("TempDir");
    let queue = Arc::new(
        SqliteQueue::new(&tmp.path().join("queue.db"))
            .await
            .expect("SqliteQueue::new"),
    );
    let vault = Arc::new(
        Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let index: Arc<SqliteIndex> = vault.index().clone();

    let dispatcher = Dispatcher::new(queue.clone())
        .with_vault(vault.clone())
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink))
        .with_index(index.clone());

    DispatcherFixture {
        dispatcher,
        queue,
        vault,
        index,
        _tmp: tmp,
    }
}

/// Encode un payload `VaultWriteRequest` minimal (titre, body, section_hint).
///
/// `tenant_id="main"` codé en dur — cohérent avec le vault `VaultId::new("main")`.
fn encode_write_payload(title: &str, body: &str, section_hint: Option<&str>) -> Vec<u8> {
    #[derive(serde::Serialize, serde::Deserialize, Debug)]
    struct WriteReq {
        title: String,
        body: String,
        #[serde(default)]
        author: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        section_hint: Option<String>,
        #[serde(default = "default_main")]
        tenant_id: String,
    }
    fn default_main() -> String {
        "main".into()
    }
    let req = WriteReq {
        title: title.into(),
        body: body.into(),
        author: None,
        tags: vec![],
        section_hint: section_hint.map(|s| s.to_string()),
        tenant_id: "main".into(),
    };
    bincode::serde::encode_to_vec(&req, bincode_std()).expect("encode WriteReq bincode")
}

/// Enqueue un job `curate` pour titre + body donnés.
///
/// Le worker générera une décision `Admitted` (chemin par défaut sans heuristique
/// spéciale — le titre n'a pas le préfixe `[DECISIONS]/[BUG]/...` qui forcerait Pending).
pub async fn enqueue_curate_job(fixture: &DispatcherFixture, title: &str, body: &str) {
    let payload = encode_write_payload(title, body, None);
    fixture
        .queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "curate".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .expect("enqueue curate");
}

/// Enqueue un job `curate` qui produira un `Pending` côté curator.
///
/// Mécanisme déclencheur Pending : le titre court (< 10 chars) sans préfixe explicite
/// + body court → confidence basse → CuratorPipeline défaut renvoie Pending.
///
/// Si le curator par défaut ne déclenche jamais Pending, le test peut être marqué
/// `#[ignore]` ou utiliser un curator stub. Vérifié empiriquement par le test
/// `curate_pending_outcome_also_upserts_wikilinks` (Task 13 Step 1) — si il échoue
/// avec un Admitted en lieu de Pending, ajuster le mécanisme déclencheur.
pub async fn enqueue_pending_curate_job(fixture: &DispatcherFixture, title: &str, body: &str) {
    let payload = encode_write_payload(title, body, None);
    fixture
        .queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "curate".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .expect("enqueue pending curate");
}

/// Vérifie qu'une note source pointe vers `dst_id` dans `note_links` (vault `main`).
///
/// Wrapper sémantique autour de `idx.backlinks("main", dst_id)` — retourne `true` si
/// au moins un lien existe vers `dst_id`. Utilisé par Task 13 pour valider le
/// branchage B5 wikilinks post-curate.
pub async fn has_backlink_to(idx: &SqliteIndex, dst_id: &str) -> bool {
    let backs = idx.backlinks("main", dst_id).await.expect("backlinks main");
    !backs.is_empty()
}

/// Renvoie le nombre de backlinks vers `dst_id` (vault `main`).
pub async fn count_backlinks(idx: &SqliteIndex, dst_id: &str) -> usize {
    idx.backlinks("main", dst_id)
        .await
        .expect("backlinks main")
        .len()
}
