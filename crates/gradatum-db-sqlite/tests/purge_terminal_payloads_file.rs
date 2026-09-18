//! Step 4 (F-160, L-C) — la purge des payloads terminaux, mesurée **sur le fichier**.
//!
//! Les tests unitaires de `queue_store_sqlite` mesurent la purge à travers l'API du
//! store. Celui-ci mesure autre chose : ce qui reste **dans le fichier** une fois la purge
//! commise. Un test en mémoire ne dirait rien de ce point — c'est le fichier qui porte la
//! donnée personnelle.
//!
//! Protocole : 250 jobs terminaux, chacun portant un témoin **unique** dans le corps de sa
//! note (`CurateSpec.body`), écrits dans un `queue.sqlite` de `TempDir`. Purge. La
//! connexion est ensuite **fermée** (dernier handle lâché : SQLite checkpointe le WAL),
//! puis le fichier est rouvert et relu — colonne `payload` d'une part, octets bruts
//! d'autre part. Un témoin partagé par les 250 jobs ne prouverait rien — les 200 conservés
//! le porteraient encore et le contrôle serait vert par construction ; chaque job porte
//! donc le sien.
//!
//! # Deux mesures, deux statuts
//!
//! 1. **Colonne `payload`, fichier rouvert** — *assertion*. Les 50 lignes purgées ne
//!    portent plus leur témoin (la sentinelle `PayloadPurged` a remplacé `spec.kind`), les
//!    200 conservées le portent encore. C'est la garantie que la purge apporte.
//! 2. **Octets bruts du fichier** — *assertion*. Ce test **comptait** les témoins purgés
//!    encore lisibles dans les octets et les imprimait : c'était vrai tant que la purge
//!    n'était que logique. Depuis que la connexion s'ouvre avec `secure_delete=ON` et que
//!    la purge termine par un checkpoint tronquant, le compte vaut **0** — les octets que
//!    la réécriture libère sont effacés, y compris les pages entières qu'elle rend au
//!    libre, et le journal qui portait l'ancienne image de page est tronqué. Le constat est
//!    donc devenu l'assertion qu'il mesurait : 0 témoin résiduel, fichier principal **et**
//!    `-wal`. `VACUUM` n'est exécuté nulle part — ni ici ni dans la purge — et n'est pas
//!    nécessaire à ce résultat : il compacterait le fichier, ce qui n'est pas la même
//!    propriété.
//!
//! Le contrôle qui garde l'assertion honnête : les témoins des 200 lignes conservées
//! doivent, eux, être présents — sans quoi l'absence serait verte sur un fichier vide,
//! illisible, ou qui n'est pas celui qu'on a purgé.

use std::path::Path;

use chrono::Utc;
use gradatum_core::job::CurateSpec;
use gradatum_core::{
    Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
    JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, TriggerSource,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use tempfile::TempDir;
use ulid::Ulid;

/// Nombre de jobs écrits : 250 terminaux pour une fenêtre de conservation de 200.
const TOTAL: u64 = 250;
/// Jobs hors fenêtre — ceux dont le corps doit disparaître.
const PURGED: u64 = 50;

/// Témoin **unique** du job `seq` — la chaîne dont l'absence est la preuve cherchée.
fn witness(seq: u64) -> String {
    format!("TEMOIN-F160-CORPS-{seq:03}")
}

/// ULID du job `seq` : explicitement ordonné, pour que la fenêtre « les 200 derniers »
/// soit déterministe sans dépendre de l'horloge du générateur d'ULID.
fn job_id(seq: u64) -> Ulid {
    Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq) + 1)
}

/// Job terminal `Done` portant `witness(seq)` dans le corps de sa note.
fn terminal_record(seq: u64) -> JobRecord {
    let now = Utc::now();
    JobRecord {
        id: job_id(seq),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                body: Some(witness(seq)),
                ..Default::default()
            }),
            class: JobClass::Agent,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::default_for(&JobClass::Agent),
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Done,
            created_at: now,
            started_at: None,
            completed_at: Some(now),
            lease_until: None,
            result: None,
        },
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

/// Lit **en octets** chaque fichier de la base (`queue.sqlite`, et son `-wal` s'il
/// subsiste) et rend, par fichier, le texte décodé.
fn read_db_files(dir: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read_dir TempDir") {
        let entry = entry.expect("entrée TempDir");
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("queue.sqlite") {
            let bytes = std::fs::read(entry.path()).expect("relire un fichier de la base");
            files.push((name, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    files
}

#[tokio::test]
async fn purged_bodies_are_gone_from_the_file() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("queue.sqlite");

    {
        let pool = QueueDb::open(&path).await.expect("open queue.sqlite");
        run_migrations(&pool).await.expect("migrations");
        let store = SqliteQueueStore::new(pool);

        for seq in 0..TOTAL {
            store
                .enqueue(terminal_record(seq))
                .await
                .expect("enqueue par le chemin réel (jamais un INSERT littéral)");
        }

        let purged = store
            .purge_terminal_payloads(None)
            .await
            .expect("purge_terminal_payloads");
        assert_eq!(purged, PURGED, "250 terminaux − 200 conservés = 50 purgés");
    } // store ET poignée `QueueDb` lâchés → connexion fermée

    // ── Mesure 2 (assertion) : octets bruts, par fichier ──────────────────────────
    let files = read_db_files(dir.path());
    let whole: String = files.iter().map(|(_, text)| text.as_str()).collect();
    for seq in (TOTAL - PURGED)..TOTAL {
        let w = witness(seq);
        assert!(
            whole.contains(&w),
            "témoin {w} (dans la fenêtre de conservation) doit survivre à la purge — \
             sans lui, l'absence ci-dessous ne prouverait rien : elle serait verte parce que \
             le fichier est vide ou illisible"
        );
    }
    for (name, text) in &files {
        let residual: Vec<u64> = (0..PURGED)
            .filter(|seq| text.contains(&witness(*seq)))
            .collect();
        assert!(
            residual.is_empty(),
            "purge L-C — {name} : {}/{PURGED} témoins purgés encore lisibles dans les octets \
             bruts (lignes {:?}) ; attendu 0 : secure_delete=ON + checkpoint tronquant \
             effacent les octets que la réécriture libère, pages d'overflow comprises. \
             VACUUM n'est pas exécuté et n'est pas nécessaire",
            residual.len(),
            residual,
        );
    }
    eprintln!(
        "purge L-C — {} fichier(s) relu(s) en octets bruts : 0/{PURGED} témoin purgé \
         résiduel ; la purge retire le corps de la ligne ET les octets qu'il occupait",
        files.len(),
    );

    // ── Mesure 1 (assertion) : la colonne `payload`, fichier rouvert ──────────────
    let pool = QueueDb::open_existing(&path)
        .await
        .expect("reopen queue.sqlite");
    let payloads: Vec<String> = pool
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT payload FROM gradatum_jobs WHERE status = 'Done' ORDER BY id")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect()
        })
        .await
        .expect("relire les payloads depuis le fichier rouvert");
    assert_eq!(
        payloads.len(),
        TOTAL as usize,
        "les {TOTAL} lignes terminales sont toujours là — la purge ne supprime aucune ligne"
    );

    for (seq, payload) in payloads.iter().enumerate() {
        let seq = seq as u64;
        let w = witness(seq);
        if seq < PURGED {
            assert!(
                !payload.contains(&w),
                "la ligne purgée #{seq} ne doit plus porter son corps dans son payload"
            );
            assert!(
                payload.contains("PayloadPurged"),
                "la ligne purgée #{seq} porte la sentinelle à la place de son `spec.kind`"
            );
        } else {
            assert!(
                payload.contains(&w),
                "la ligne conservée #{seq} garde son corps intact"
            );
        }
    }

    // La ligne purgée est toujours là, terminale : la purge ne touche pas au cycle de vie.
    let store = SqliteQueueStore::new(pool);
    let row = store
        .get(job_id(0), None)
        .await
        .expect("get après réouverture")
        .expect("la ligne purgée survit à la purge");
    assert!(
        matches!(row.spec.kind, Job::PayloadPurged),
        "le payload purgé porte la sentinelle"
    );
    assert_eq!(
        row.lifecycle.status,
        JobStatus::Done,
        "le statut terminal est conservé : la purge ne touche que `spec.kind`"
    );
}
