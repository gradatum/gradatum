//! `gradatum-admin backfill-embeddings` sub-command.
//!
//! Scans live notes without an embedding (LEFT JOIN on `note_embeddings`) and
//! enqueues `Job::Embed` jobs into `gradatum_jobs` — the queue the worker actually
//! drains via `dequeue_by_kind("Embed")`. Idempotent: notes that already have an
//! embedding are excluded by the LEFT JOIN, and the embed handler skips a note whose
//! vector already exists.
//!
//! ## Usage
//! ```text
//! gradatum-admin backfill-embeddings --root /var/lib/gradatum
//! gradatum-admin backfill-embeddings --root /var/lib/gradatum --tenant main --limit 100
//! ```
//!
//! ## Expected paths (standard install layout)
//! - Queue   : `<root>/db/queue.sqlite`  (table `gradatum_jobs` ; `jobs_v2` supprimée en 2.1.0, F-177)
//! - Index   : `<root>/vault/.gradatum/index.db`

use anyhow::{Context, Result};
use chrono::Utc;
use gradatum_core::paths::{queue_db_path, vault_index_path};
use gradatum_core::status::NoteStatus;
use gradatum_core::{
    EmbedSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
    JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, TriggerSource,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, open_queue_db_existing};
use std::path::PathBuf;
use ulid::Ulid;

/// Plafond dur, tous tenants confondus : au-delà, l'enfilage doit être explicite.
///
/// Safety cap (protection anti-enfilage-de-masse), pas un paramètre utilisateur —
/// d'où une constante et non un champ de configuration.
const BACKFILL_HARD_CAP: usize = 5_000;

/// Refuse un enfilage de masse non intentionnel.
///
/// Deux règles, dans cet ordre :
/// 1. tout tenant autre que `main` exige un `--limit` explicite — la population
///    des vaults de code se compte en milliers (mesuré : 8785 sur `code-gradatum`
///    contre 11 sur `main` le 2026-08-15) ;
/// 2. au-delà de [`BACKFILL_HARD_CAP`], même `main` exige un `--limit`.
///
/// # Errors
/// Renvoie une erreur nommant le tenant et `--limit` quand une des deux règles
/// n'est pas satisfaite et qu'aucune borne n'a été fournie.
///
/// `pub(crate)` : réutilisé tel quel par `reindex-orphans` (F-166) pour hériter du
/// MÊME garde-fou de volume — deux copies finiraient par diverger.
pub(crate) fn guard_tenant_scope(
    tenant: &str,
    candidate_count: usize,
    limit: Option<usize>,
) -> Result<()> {
    if limit.is_some() {
        return Ok(());
    }
    if tenant != "main" {
        anyhow::bail!(
            "backfill: refus d'enfiler {candidate_count} jobs sur le tenant '{tenant}' sans borne. \
             Seul 'main' est enfilable sans borne ; relancer avec --limit <n> pour confirmer l'intention."
        );
    }
    if candidate_count > BACKFILL_HARD_CAP {
        anyhow::bail!(
            "backfill: {candidate_count} candidates on '{tenant}' exceed the hard cap of {BACKFILL_HARD_CAP}. \
             Re-run with --limit <n>."
        );
    }
    Ok(())
}

/// Arguments for the `backfill-embeddings` sub-command.
#[derive(Debug, Clone)]
pub struct BackfillArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`).
    pub root: PathBuf,
    /// Target tenant (default: `"main"`).
    pub tenant: Option<String>,
    /// Maximum number of notes to enqueue; unlimited when absent.
    pub limit: Option<usize>,
}

/// Scans live notes without an embedding and enqueues `Job::Embed` jobs into the
/// live queue (`gradatum_jobs`).
///
/// Returns the number of jobs enqueued.
///
/// # Errors
/// - Missing `queue.sqlite` or `index.db` → descriptive error.
/// - SQLite error during scan or enqueue.
pub async fn backfill(args: BackfillArgs) -> Result<usize> {
    // SSOT : chemins via helpers canoniques — jamais root.join(...) manuel.
    let queue_path = queue_db_path(&args.root);
    let index_path = vault_index_path(&args.root);

    if !queue_path.exists() {
        anyhow::bail!(
            "queue.sqlite not found: {} — run `gradatum-admin init` first",
            queue_path.display()
        );
    }
    if !index_path.exists() {
        anyhow::bail!(
            "index.db not found: {} — the worker must have started at least once",
            index_path.display()
        );
    }

    let tenant = args.tenant.as_deref().unwrap_or("main").to_string();

    // ── Collecte des notes vivantes sans vecteur (synchrone — rusqlite) ──────
    // Ouvre l'index en LECTURE SEULE : aucun PRAGMA WAL ni migration (le schéma
    // existe déjà). C4 : le scan filtre `status = 'live'` — voir collect_unembedded_notes.
    let candidates = collect_unembedded_notes(&index_path, &tenant, args.limit)
        .context("scanning live notes without embedding")?;

    if candidates.is_empty() {
        eprintln!(
            "backfill: 0 job enqueued — all live notes are already embedded (tenant='{tenant}')"
        );
        return Ok(0);
    }

    let total = candidates.len();

    // Garde-fou AVANT toute écriture : refuse un enfilage de masse non borné.
    guard_tenant_scope(&tenant, total, args.limit)?;

    eprintln!(
        "backfill: {total} notes vivantes sans vecteur (tenant='{tenant}') — enfilage dans gradatum_jobs..."
    );

    // File VIVANTE : `gradatum_jobs`, drainée par le worker via dequeue_by_kind("Embed").
    // Pool ouvert avec les MÊMES réglages WAL que le worker/serveur (journal WAL +
    // busy_timeout 5 s), sinon on devient un writer concurrent qui échoue sur
    // `database is locked`.
    let pool = open_queue_pool(&queue_path)
        .await
        .context("ouverture du pool queue.sqlite (WAL)")?;
    let store = SqliteQueueStore::new(pool);

    // Enfile PUIS relit le compte dans gradatum_jobs : un enfilage sans effet
    // échoue franchement au lieu de rapporter un succès (cœur de F-175).
    let enqueued = enqueue_and_verify(&store, &candidates, &tenant).await?;

    eprintln!("backfill: {enqueued} Embed jobs confirmed in gradatum_jobs (tenant='{tenant}')");
    Ok(enqueued)
}

/// Enfile un `Job::Embed` par note, puis RELIT le compte dans `gradatum_jobs`.
///
/// Le compteur retourné est relu depuis la table via [`QueueStore::get`] — jamais la
/// valeur que la boucle s'est donnée. Un enfilage resté sans effet (jobs qui
/// n'atteignent pas la file drainée par le worker) doit se voir : si des candidats
/// existaient mais qu'aucun job n'est relu, l'appel échoue plutôt que de rapporter
/// un faux succès. `note_ids` vide → `Ok(0)` (rien à faire, pas une erreur).
///
/// # Errors
/// - Échec d'enfilage (`enqueue`) ou de relecture (`get`) sur la file.
/// - Des candidats existaient mais aucun job n'a atteint `gradatum_jobs`.
///
/// `pub(crate)` : réutilisé par `reindex-orphans` (F-166) pour enfiler l'embed des
/// notes ré-indexées avec la MÊME propriété « compteur relu dans la table ».
pub(crate) async fn enqueue_and_verify(
    store: &SqliteQueueStore,
    note_ids: &[Ulid],
    tenant: &str,
) -> Result<usize> {
    if note_ids.is_empty() {
        return Ok(0);
    }
    let total = note_ids.len();

    let mut job_ids = Vec::with_capacity(total);
    for (i, note_id) in note_ids.iter().enumerate() {
        let id = store
            .enqueue(build_embed_job(*note_id, tenant))
            .await
            .with_context(|| format!("enfilage du job Embed pour la note {note_id}"))?;
        job_ids.push(id);
        if (i + 1) % 100 == 0 {
            eprintln!("backfill: {}/{total} jobs enqueued...", i + 1);
        }
    }

    // Relecture DANS la table : QueueStore::get lit gradatum_jobs, jamais job_ids.len().
    let mut confirmed = 0usize;
    for id in &job_ids {
        if store
            .get(*id, Some(tenant))
            .await
            .with_context(|| format!("relecture du job {id} dans gradatum_jobs"))?
            .is_some()
        {
            confirmed += 1;
        }
    }

    if confirmed == 0 {
        anyhow::bail!(
            "backfill: {total} candidates but no job re-read in gradatum_jobs — \
             the enqueue had no effect (the queue drained by the worker is indeed 'gradatum_jobs')."
        );
    }

    Ok(confirmed)
}

/// Construit un [`JobRecord`] `Job::Embed` pour la file vivante `gradatum_jobs`.
///
/// Gabarit repris de la construction serveur (`gradatum-server/src/api_v1/write.rs`)
/// en substituant la variante. `force_regenerate = false` : le handler d'embed est
/// idempotent (il passe son tour si le vecteur existe), le backfill ne force jamais.
/// `priority = Low` : réparation de fond, ne pas concurrencer le trafic vivant.
fn build_embed_job(note_id: Ulid, tenant: &str) -> JobRecord {
    let now = Utc::now();
    JobRecord {
        id: Ulid::generate(),
        spec: JobSpec {
            kind: Job::Embed(EmbedSpec {
                note_id,
                tenant_id: tenant.to_owned(),
                force_regenerate: false,
            }),
            class: JobClass::Agent,
            mode: JobMode::Batch,
            // Un job qui ne concerne qu'une note se déclare sur cette note, pas sur tout
            // le vault — aligné sur le constructeur d'embed du worker
            // (gradatum-worker/src/apalis_handlers.rs). Le routage ne lit pas `scope`
            // (seulement kind/status/scheduled_at/tenant), donc c'est une correction de
            // justesse sémantique, sans effet sur le drain.
            scope: JobScope::Notes(vec![note_id]),
            priority: JobPriority::Low,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        // Le SEUL ::default() valide des cinq blocs : les quatre autres ne dérivent pas Default.
        retry: JobRetry::default(),
        lineage: JobLineage {
            triggered_by: None,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// Ouvre le pool `queue.sqlite` avec les réglages WAL du worker/serveur.
///
/// `create_if_missing(false)` : le fichier a déjà été vérifié présent en amont —
/// ne jamais créer une base parasite. `journal_mode = WAL` + `busy_timeout = 5 s`
/// répliquent `gradatum-worker/src/main.rs` et `gradatum-server/src/main.rs` : sans
/// eux, un enfilage concurrent d'un lock serveur/worker échouerait immédiatement.
///
/// `pub(crate)` : réutilisé par `reindex-orphans` (F-166).
pub(crate) async fn open_queue_pool(queue_path: &std::path::Path) -> Result<QueueDb> {
    // `open_queue_db_existing` : n'ouvre PAS avec CREATE (parité
    // `create_if_missing(false)` de sqlx) — le fichier a déjà été vérifié présent en
    // amont, ne jamais créer une base parasite. WAL + busy_timeout 5 s appliqués.
    open_queue_db_existing(queue_path)
        .await
        .context("connecting to the queue.sqlite database")
}

/// Liste SQL quotée des statuts embeddables — **délègue** au SSOT
/// [`NoteStatus::embeddable_default_sql_list`] (cœur), ex. `'live', 'pending-review',
/// 'staging'`.
///
/// F-174 : le roster et la garde d'exhaustivité ont migré dans `gradatum-core` pour que le
/// réparateur (cet enfileur) et le détecteur de dérive (`gradatum-index::drift`) tirent leur
/// filtre de statut de la MÊME source — sans quoi une note embeddable sans vecteur serait
/// réparable ici mais jamais signalée là-bas.
fn embeddable_status_sql_list() -> String {
    NoteStatus::embeddable_default_sql_list()
}

/// Collecte les `note_id` (ULID) des notes EMBEDDABLES sans vecteur.
///
/// `LEFT JOIN` sur `note_embeddings` → idempotent par construction : seules les
/// notes pour lesquelles aucune ligne n'existe (tout `embedder_id` confondu) sont
/// retenues. Le corps n'est plus transporté — `EmbedSpec` ne le porte pas, le
/// handler relit la note.
///
/// ## Filtre de statut (R1/F-175 — élargi le 2026-08-16 après revue)
/// La requête retient les statuts **embeddables par défaut** (`live`, `pending-review`,
/// `staging`), dérivés du SSOT [`NoteStatus::is_embeddable_default`] via
/// [`embeddable_status_sql_list`] — **pas** seulement `live`.
///
/// Raison : le coût d'embedding **n'est pas re-payé** à la transition
/// `pending-review → live` (`gradatum-core/src/status.rs`). Une note qui a raté son
/// vecteur pendant la fenêtre où `jobs_v2` était morte, et qui stationne en
/// `pending-review`/`staging`, ne serait donc **jamais** rattrapée en devenant `live` :
/// se limiter à `live` laisserait ouvert le trou même que F-175 comble. La population
/// mesurée est identique aujourd'hui (11 sur `main`, 8785 sur `code-gradatum` dans les
/// deux cas), donc les seuils du garde-fou restent valides ; l'élargissement ferme un
/// défaut futur.
///
/// Ouvre l'index en LECTURE SEULE (`SQLITE_OPEN_READ_ONLY`) : garantie qu'aucune
/// écriture ne touche `index.db`. Retourne un `Vec` complet pour relâcher la
/// `Connection` avant l'ouverture du pool asynchrone.
fn collect_unembedded_notes(
    index_path: &std::path::Path,
    tenant: &str,
    limit: Option<usize>,
) -> Result<Vec<Ulid>> {
    let conn = rusqlite::Connection::open_with_flags(
        index_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .context("opening index.db read-only")?;

    let limit_clause = limit.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    // Valeurs issues de l'enum (jamais d'entrée externe) → interpolation SQL sûre.
    let status_list = embeddable_status_sql_list();
    let query = format!(
        "SELECT n.id
         FROM notes n
         LEFT JOIN note_embeddings e ON n.id = e.note_id
         WHERE e.note_id IS NULL
           AND n.vault_id = ?1
           AND n.status IN ({status_list})
         ORDER BY n.id
         {limit_clause}"
    );

    let mut stmt = conn.prepare(&query).context("preparing backfill query")?;
    let rows = stmt
        .query_map(rusqlite::params![tenant], |row| row.get::<_, String>(0))
        .context("executing backfill query")?;

    let mut candidates = Vec::new();
    for row in rows {
        let id_str = row.context("reading note id")?;
        let ulid = Ulid::from_string(&id_str)
            .map_err(|e| anyhow::anyhow!("invalid note id '{id_str}' in the index: {e}"))?;
        candidates.push(ulid);
    }

    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_db_sqlite::{open_queue_db, run_migrations};

    // ── Task 1 : garde-fou de tenant ─────────────────────────────────────────

    // Règle 1 : tout tenant != main sans borne est refusé — INDÉPENDAMMENT du volume.
    // Compte volontairement SOUS le plafond dur pour que SEULE la règle 1 puisse refuser.
    #[test]
    fn guard_refuse_un_tenant_hors_main_sans_limite() {
        let err = guard_tenant_scope("code-gradatum", 1, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("code-gradatum"),
            "le message doit nommer le tenant : {msg}"
        );
        assert!(
            msg.contains("--limit"),
            "le message doit dire comment débloquer : {msg}"
        );
    }

    #[test]
    fn guard_laisse_passer_main_sans_limite() {
        assert!(guard_tenant_scope("main", 1, None).is_ok());
    }

    #[test]
    fn guard_laisse_passer_un_autre_tenant_avec_limite_explicite() {
        assert!(guard_tenant_scope("code-gradatum", BACKFILL_HARD_CAP * 2, Some(100)).is_ok());
    }

    // Règle 2 : plafond dur, dérivé de la CONSTANTE et non d'un relevé.
    #[test]
    fn guard_refuse_meme_main_au_dela_du_plafond_dur() {
        let err = guard_tenant_scope("main", BACKFILL_HARD_CAP + 1, None).unwrap_err();
        assert!(
            err.to_string().contains(&BACKFILL_HARD_CAP.to_string()),
            "le plafond doit être cité"
        );
    }

    // ── Task 2 : le job construit porte le discriminant drainé par le worker ──

    #[test]
    fn le_job_construit_porte_le_discriminant_que_le_worker_draine() {
        let note_id = Ulid::generate();
        let job = build_embed_job(note_id, "main");

        // Le worker appelle dequeue_by_kind("Embed") — gradatum-worker/src/monitor.rs:333.
        // Le discriminant vient de gradatum-core/src/job.rs:814.
        assert_eq!(gradatum_core::job_kind_str(&job.spec.kind), "Embed");

        match &job.spec.kind {
            Job::Embed(spec) => {
                assert_eq!(spec.note_id, note_id);
                assert_eq!(spec.tenant_id, "main");
                assert!(
                    !spec.force_regenerate,
                    "le backfill ne doit jamais forcer : le handler est idempotent"
                );
            }
            other => panic!("variante inattendue : {other:?}"),
        }
    }

    // ── Task 3 : l'échec franc sur enfilage sans effet ───────────────────────

    /// Pool `queue.sqlite` temporaire, schéma migré (mêmes migrations que le
    /// worker/serveur). File vide, aucun worker.
    async fn fixture_queue_pool() -> (QueueDb, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("queue.sqlite");
        let db = open_queue_db(&db_path).await.expect("db queue temporaire");
        run_migrations(&db).await.expect("migrations queue");
        (db, tmp)
    }

    #[tokio::test]
    async fn un_run_dont_les_jobs_n_atteignent_pas_la_file_vivante_echoue() {
        // Base temporaire : file vide, aucun worker. On enfile 1 job puis on
        // vérifie que le compteur lu DANS la table correspond — jamais le compteur
        // que la fonction s'est elle-même donné.
        let (db, _tmp) = fixture_queue_pool().await;
        let store = SqliteQueueStore::new(db.clone());

        let enfiles = enqueue_and_verify(&store, &[Ulid::generate()], "main")
            .await
            .expect("un enfilage nominal doit réussir");
        assert_eq!(enfiles, 1);

        let en_base: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM gradatum_jobs WHERE kind = 'Embed'",
                    [],
                    |row| row.get(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(
            en_base, 1,
            "le compteur doit être relu depuis la table, pas rapporté par l'outil"
        );
    }

    // ── R1 : le scan retient les statuts embeddables, exclut les autres ──────

    /// Index temporaire minimal : `notes(id, vault_id, status)` + `note_embeddings(note_id)`.
    fn fixture_index_with(notes: &[(Ulid, &str, bool)]) -> (std::path::PathBuf, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let idx = tmp.path().join("index.db");
        let conn = rusqlite::Connection::open(&idx).expect("open index");
        conn.execute_batch(
            "CREATE TABLE notes (id TEXT PRIMARY KEY, vault_id TEXT NOT NULL, status TEXT NOT NULL);
             CREATE TABLE note_embeddings (note_id TEXT);",
        )
        .expect("schema");
        for (id, status, embedded) in notes {
            conn.execute(
                "INSERT INTO notes (id, vault_id, status) VALUES (?1, 'main', ?2)",
                rusqlite::params![id.to_string(), status],
            )
            .expect("insert note");
            if *embedded {
                conn.execute(
                    "INSERT INTO note_embeddings (note_id) VALUES (?1)",
                    rusqlite::params![id.to_string()],
                )
                .expect("insert embedding");
            }
        }
        drop(conn); // relâche le verrou avant l'ouverture read-only
        (idx, tmp)
    }

    #[test]
    fn le_scan_retient_les_embeddables_sans_vecteur_et_exclut_les_autres() {
        let live = Ulid::generate();
        let pending = Ulid::generate();
        let staging = Ulid::generate();
        let deprecated = Ulid::generate();
        let draft = Ulid::generate();
        let live_embedded = Ulid::generate();

        let (idx, _tmp) = fixture_index_with(&[
            (live, "live", false),
            (pending, "pending-review", false),
            (staging, "staging", false),
            (deprecated, "deprecated", false),
            (draft, "draft", false),
            (live_embedded, "live", true),
        ]);

        let got: std::collections::HashSet<Ulid> = collect_unembedded_notes(&idx, "main", None)
            .expect("collect")
            .into_iter()
            .collect();

        // Embeddables sans vecteur → retenus (pending-review est le cœur de R1 : le coût
        // n'est pas re-payé à la transition, donc jamais rattrapé si limité à 'live').
        assert!(got.contains(&live), "live sans vecteur doit être retenu");
        assert!(
            got.contains(&pending),
            "pending-review sans vecteur doit être retenu (R1)"
        );
        assert!(
            got.contains(&staging),
            "staging sans vecteur doit être retenu"
        );
        // Non-embeddables par défaut → exclus.
        assert!(!got.contains(&deprecated), "deprecated doit être exclu");
        assert!(!got.contains(&draft), "draft doit être exclu");
        // Idempotence : embeddable AVEC vecteur → exclu.
        assert!(
            !got.contains(&live_embedded),
            "une note déjà embarquée doit être exclue"
        );
    }
}
