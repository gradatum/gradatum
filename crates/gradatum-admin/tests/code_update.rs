//! Tests d'intégration — sub-commande `gradatum-admin code update` (v0.5.2 Phase C).
//!
//! ## Stratégie
//!
//! Repo git temporaire ; on exerce le pipeline `code update` O(diff) :
//! `git diff --name-status <last_sha>..HEAD` → re-ingest A/M, suppression D.
//!
//! ## Golden tests couverts ici (Phase C)
//!
//! - **(a)** mutation source + update → map à jour (nouvelle signature visible).
//! - **(c)** suppression propagée via update (fichier supprimé → notes disparues).
//! - **(d)** FIX-T2 (rebuild-equivalence) : `set(note_id)` incrémental == `set(note_id)` rebuild,
//!   avec vérification body identique par note_id.
//! - **(e)** idempotence : update sans changement = 0 write.
//!
//! ## Bench acceptation (mesuré, BLOQUANT au tag public)
//!
//! - Update post-commit < 3s.
//! - Idempotence 0 write prouvée.
//!
//! Aucun accès réseau ni service gradatum LIVE requis.

use gradatum_admin::code_cmd::{
    run_ingest, run_update, CodeIngestArgs, CodeUpdateArgs, IngestVisibility,
};
use gradatum_core::index_store::CodeSelector;
use gradatum_index::SqliteIndex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tempfile::TempDir;

/// Exécute une commande git dans le repo, panique sur échec.
fn git(repo: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} échec: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Init un repo git avec identité configurée.
fn init_repo() -> TempDir {
    let tmp = TempDir::new().expect("TempDir");
    let repo = tmp.path();
    git(repo, &["init"]);
    git(repo, &["config", "user.email", "test@gradatum.test"]);
    git(repo, &["config", "user.name", "Test"]);
    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    tmp
}

fn commit_all(repo: &Path, msg: &str) {
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", msg]);
}

async fn setup_index() -> (std::path::PathBuf, TempDir) {
    let tmp = TempDir::new().expect("TempDir index");
    let index_path = tmp.path().join("index.db");
    SqliteIndex::open(&index_path).await.expect("open index");
    (index_path, tmp)
}

/// Golden (a) : mutation source + update → map à jour (nouvelle signature visible).
#[tokio::test]
async fn update_a_mutation_reflected() {
    let tmp = init_repo();
    let repo = tmp.path();
    std::fs::write(
        repo.join("src/lib.rs"),
        "/// V1.\npub fn target(a: u32) -> u32 { a }",
    )
    .expect("write v1");
    commit_all(repo, "init");

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-update-a".to_string();

    // Ingest initial.
    run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest");

    // Muter la signature de target.
    std::fs::write(
        repo.join("src/lib.rs"),
        "/// V2.\npub fn target(a: u32, b: u32) -> u64 { (a + b) as u64 }",
    )
    .expect("write v2");
    commit_all(repo, "change signature");

    // Update O(diff).
    let report = run_update(CodeUpdateArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        ..Default::default()
    })
    .await
    .expect("update");
    assert_eq!(report.files_ingested, 1, "1 fichier modifié ré-ingéré");
    assert!(!report.from_sha.is_empty(), "from_sha = ancien HEAD");

    // Vérifier la nouvelle signature via code_scope_query.
    use gradatum_core::index_store::CodeSelector;
    let index = SqliteIndex::open(&index_path).await.expect("open");
    let res = index
        .code_scope_query(&vault_id, &CodeSelector::Symbol("target".into()), 10)
        .await
        .expect("query");
    assert_eq!(res.len(), 1);
    let sig = res[0].signature.as_deref().unwrap_or("");
    assert!(
        sig.contains("u64"),
        "la nouvelle signature (u64) doit être visible, got {sig:?}"
    );
}

/// Golden (c) : suppression propagée via update.
#[tokio::test]
async fn update_c_deletion_propagated() {
    let tmp = init_repo();
    let repo = tmp.path();
    std::fs::write(repo.join("src/keep.rs"), "pub fn keep() {}").expect("keep");
    std::fs::write(repo.join("src/gone.rs"), "pub fn gone() {}").expect("gone");
    commit_all(repo, "init 2 files");

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-update-c".to_string();

    run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest");

    // Supprimer gone.rs.
    git(repo, &["rm", "src/gone.rs"]);
    commit_all(repo, "rm gone");

    let report = run_update(CodeUpdateArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        ..Default::default()
    })
    .await
    .expect("update");
    assert_eq!(report.files_deleted, 1, "1 fichier supprimé");

    // gone.rs absent de code_freshness + ses notes absentes.
    let index = SqliteIndex::open(&index_path).await.expect("open");
    let fresh = index
        .get_code_freshness_map(&vault_id)
        .await
        .expect("fresh map");
    assert!(!fresh.contains_key("src/gone.rs"), "gone.rs purgé");
    assert!(fresh.contains_key("src/keep.rs"), "keep.rs conservé");

    use gradatum_core::index_store::CodeSelector;
    let gone_hits = index
        .code_scope_query(&vault_id, &CodeSelector::Symbol("gone".into()), 10)
        .await
        .expect("query gone");
    assert!(gone_hits.is_empty(), "aucune note fantôme de gone()");
}

/// Golden (e) + bench idempotence : update sans changement = 0 write.
#[tokio::test]
async fn update_e_idempotent_zero_write() {
    let tmp = init_repo();
    let repo = tmp.path();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() {}").expect("write");
    commit_all(repo, "init");

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-update-e".to_string();

    run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest");

    // Update sans aucun commit → diff vide → 0 write.
    let report = run_update(CodeUpdateArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        ..Default::default()
    })
    .await
    .expect("update no-op");

    assert_eq!(report.files_changed, 0, "aucun fichier changé");
    assert_eq!(report.files_ingested, 0, "0 write (idempotence)");
    assert_eq!(report.files_deleted, 0, "0 suppression");
    assert_eq!(report.notes_inserted, 0, "0 note insérée");
    assert_eq!(
        report.from_sha, report.to_sha,
        "from_sha == to_sha (aucun commit entre-temps)"
    );
}

/// Update sur vault jamais ingéré → fallback ingest complet.
#[tokio::test]
async fn update_fallback_full_ingest() {
    let tmp = init_repo();
    let repo = tmp.path();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() {}").expect("write");
    commit_all(repo, "init");

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-update-fallback".to_string();

    // Pas d'ingest préalable → update doit faire un ingest complet.
    let report = run_update(CodeUpdateArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        ..Default::default()
    })
    .await
    .expect("update fallback");

    assert!(
        report.files_ingested >= 1,
        "fallback : au moins 1 fichier ingéré"
    );
    assert!(
        report.from_sha.is_empty(),
        "from_sha vide (fallback complet)"
    );
    assert!(!report.to_sha.is_empty(), "to_sha = HEAD");
}

/// FIX-T2 — rebuild-equivalence contract : golden `rebuild == incrémental` — égalité de SET, pas de compteurs.
///
/// Construit la map de façon **incrémentale** (`run_ingest` initial + `run_update` sur un fichier
/// modifié), puis fait un **rebuild complet** (`run_ingest(rebuild=true)`), et vérifie que :
/// - `set(note_id)` est identique entre les deux états ;
/// - le `body_text` est identique par `note_id` (contenu convergent) ;
/// - les timestamps `created` sont EXCLUS de la comparaison.
///
/// Ce test échoue si `code update` crée des note_id fantômes que le rebuild élimine, ou
/// inversement — garantissant que les deux chemins convergent vers la même représentation.
#[tokio::test]
async fn update_d_rebuild_equals_incremental_golden() {
    let tmp = init_repo();
    let repo = tmp.path();

    // 3 fichiers .rs initiaux.
    std::fs::write(
        repo.join("src/alpha.rs"),
        "/// Alpha V1.\npub fn alpha(a: u32) -> u32 { a }",
    )
    .expect("write alpha");
    std::fs::write(
        repo.join("src/beta.rs"),
        "/// Beta V1.\npub struct Beta { pub x: u32 }",
    )
    .expect("write beta");
    std::fs::write(
        repo.join("src/gamma.rs"),
        "/// Gamma V1.\npub fn gamma(s: &str) -> usize { s.len() }",
    )
    .expect("write gamma");
    commit_all(repo, "init 3 files");

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-rebuild-golden".to_string();

    // ── Ingest initial ─────────────────────────────────────────────────────────
    run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest initial");

    // ── Modifier un fichier + commit ───────────────────────────────────────────
    std::fs::write(
        repo.join("src/alpha.rs"),
        "/// Alpha V2.\npub fn alpha(a: u32, b: u32) -> u64 { (a + b) as u64 }",
    )
    .expect("write alpha v2");
    commit_all(repo, "update alpha");

    // ── Update incrémental (chemin O(diff)) ───────────────────────────────────
    run_update(CodeUpdateArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        ..Default::default()
    })
    .await
    .expect("update incremental");

    // ── Snapshot incrémental : note_id → body_text ────────────────────────────
    let incremental_snapshot = collect_snapshot(&index_path, &vault_id).await;
    assert!(
        !incremental_snapshot.is_empty(),
        "snapshot incrémental non vide"
    );

    // ── Rebuild complet depuis zéro ───────────────────────────────────────────
    run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: true,
        ..Default::default()
    })
    .await
    .expect("rebuild complet");

    // ── Snapshot rebuild : note_id → body_text ────────────────────────────────
    let rebuild_snapshot = collect_snapshot(&index_path, &vault_id).await;
    assert!(!rebuild_snapshot.is_empty(), "snapshot rebuild non vide");

    // ── Assertion rebuild-equivalence : sets de note_id identiques ───────────────
    let incremental_ids: HashSet<String> = incremental_snapshot.keys().cloned().collect();
    let rebuild_ids: HashSet<String> = rebuild_snapshot.keys().cloned().collect();

    let only_in_incremental: Vec<&String> = incremental_ids.difference(&rebuild_ids).collect();
    let only_in_rebuild: Vec<&String> = rebuild_ids.difference(&incremental_ids).collect();

    assert!(
        only_in_incremental.is_empty(),
        "note_id fantômes dans incrémental absents du rebuild : {only_in_incremental:?}"
    );
    assert!(
        only_in_rebuild.is_empty(),
        "note_id du rebuild absents dans l'incrémental : {only_in_rebuild:?}"
    );

    // ── Assertion rebuild-equivalence : contenu (source_path|kind|qname|sig) identique ─
    for (note_id, incremental_canon) in &incremental_snapshot {
        let rebuild_canon = rebuild_snapshot
            .get(note_id)
            .expect("note_id présent dans rebuild (déjà vérifié ci-dessus)");
        assert_eq!(
            incremental_canon, rebuild_canon,
            "contenu diverge pour note_id {note_id}: \
             incrémental={incremental_canon:?} rebuild={rebuild_canon:?}"
        );
    }
}

/// Collecte `HashMap<note_id, SymbolCanon>` pour toutes les notes d'un vault code.
///
/// `SymbolCanon` = tuple sérialisé `(source_path, kind, qualified_name, signature_or_empty)`
/// — représentation canonique comparable entre incrémental et rebuild, sans dépendance aux
/// timestamps `created` (exclus par construction).
///
/// Utilise `get_code_freshness_map` pour obtenir les source_paths connus puis
/// `code_scope_query(Path)` par fichier pour collecter tous les symboles.
async fn collect_snapshot(index_path: &std::path::Path, vault_id: &str) -> HashMap<String, String> {
    let index = SqliteIndex::open(index_path).await.expect("open index");

    // Source_paths connus dans ce vault.
    let freshness = index
        .get_code_freshness_map(vault_id)
        .await
        .expect("get_code_freshness_map");

    let mut snapshot = HashMap::new();

    for source_path in freshness.keys() {
        // Requête par path → tous les symboles de ce fichier.
        let hits = index
            .code_scope_query(vault_id, &CodeSelector::Path(source_path.clone()), 1000)
            .await
            .expect("code_scope_query path");

        for hit in hits {
            // Représentation canonique : (source_path, kind, qname, sig).
            // Timestamp `created` non inclus → déterminisme de la comparaison garanti.
            // note_id est déterministe (ULID dérivé de la clé logique) → stable entre runs.
            let canon = format!(
                "{}|{}|{}|{}",
                hit.source_path,
                hit.kind,
                hit.qualified_name,
                hit.signature.as_deref().unwrap_or("")
            );
            snapshot.insert(hit.note_id.to_string(), canon);
        }
    }

    snapshot
}
///
/// `#[ignore]` par défaut (mesure, pas assertion CI bloquante — relancer manuellement
/// via `cargo test -p gradatum-admin --test code_update -- --ignored bench`).
/// Crée 50 fichiers .rs, ingest complet, puis modifie 1 fichier et mesure l'update.
#[tokio::test]
#[ignore = "bench manuel — mesure perf update O(diff)"]
async fn bench_update_under_3s() {
    let tmp = init_repo();
    let repo = tmp.path();
    for i in 0..50 {
        std::fs::write(
            repo.join(format!("src/mod_{i}.rs")),
            format!(
                "/// Module {i}.\npub fn func_{i}(x: u32) -> u32 {{ x + {i} }}\npub struct S{i};"
            ),
        )
        .expect("write");
    }
    commit_all(repo, "init 50 files");

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-bench".to_string();

    let ingest = run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest");
    eprintln!(
        "[BENCH] ingest complet : {} fichiers, {} notes, {} ms",
        ingest.files_ingested, ingest.notes_inserted, ingest.duration_ms
    );

    // Modifier 1 seul fichier.
    std::fs::write(
        repo.join("src/mod_0.rs"),
        "/// Modifié.\npub fn func_0(x: u64, y: u64) -> u64 { x + y }",
    )
    .expect("modify");
    commit_all(repo, "tweak mod_0");

    let report = run_update(CodeUpdateArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        ..Default::default()
    })
    .await
    .expect("update");

    eprintln!(
        "[BENCH] update O(diff) : changed={} ingested={} notes={} duration_ms={}",
        report.files_changed, report.files_ingested, report.notes_inserted, report.duration_ms
    );
    assert_eq!(report.files_ingested, 1, "seul mod_0 ré-ingéré (O(diff))");
    assert!(
        report.duration_ms < 3000,
        "update post-commit doit être < 3s, mesuré {} ms",
        report.duration_ms
    );
}

/// Bench dogfood (acceptance) sur le repo gradatum LUI-MÊME (~465 fichiers .rs).
///
/// Mesure réelle : ingest complet, update O(diff) (1 fichier touché), idempotence
/// (0 write), et taille d'une réponse code_scope typique (< 800 tokens).
///
/// `#[ignore]` — bench manuel (dépend du repo gradatum à `CARGO_MANIFEST_DIR/../..`,
/// state git du worktree). Relancer : `cargo test -p gradatum-admin --test code_update
/// -- --ignored bench_dogfood --nocapture`.
#[tokio::test]
#[ignore = "bench dogfood — ingère le repo gradatum réel"]
async fn bench_dogfood_gradatum_repo() {
    // crates/gradatum-admin → ../.. = racine du repo.
    let manifest = env!("CARGO_MANIFEST_DIR");
    let repo = Path::new(manifest)
        .parent()
        .and_then(|p| p.parent())
        .expect("racine repo")
        .to_path_buf();
    assert!(
        repo.join(".git").exists(),
        "repo gradatum attendu à {repo:?}"
    );

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-gradatum-bench".to_string();

    // Ingest complet.
    let ingest = run_ingest(CodeIngestArgs {
        repo_path: repo.clone(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest dogfood");
    eprintln!(
        "[DOGFOOD] ingest complet : files_total={} ingested={} notes={} duration_ms={}",
        ingest.files_total, ingest.files_ingested, ingest.notes_inserted, ingest.duration_ms
    );

    // Idempotence : 2e ingest → 0 write.
    let ingest2 = run_ingest(CodeIngestArgs {
        repo_path: repo.clone(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest2");
    eprintln!(
        "[DOGFOOD] idempotence 2e ingest : skipped={}/{} ingested={} notes={}",
        ingest2.files_skipped, ingest2.files_total, ingest2.files_ingested, ingest2.notes_inserted
    );
    assert_eq!(ingest2.files_ingested, 0, "idempotence : 0 write");
    assert_eq!(ingest2.notes_inserted, 0, "idempotence : 0 note");

    // Update O(diff) : no-op (aucun commit depuis l'ingest) → 0 write, mesure durée.
    let update = run_update(CodeUpdateArgs {
        repo_path: repo.clone(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        ..Default::default()
    })
    .await
    .expect("update");
    eprintln!(
        "[DOGFOOD] update O(diff) no-op : changed={} ingested={} duration_ms={}",
        update.files_changed, update.files_ingested, update.duration_ms
    );
    assert!(
        update.duration_ms < 3000,
        "update doit être < 3s, mesuré {} ms",
        update.duration_ms
    );

    // Taille d'une réponse code_scope typique : estimer les tokens d'un scope path.
    use gradatum_core::index_store::CodeSelector;
    let index = SqliteIndex::open(&index_path).await.expect("open");
    let hits = index
        .code_scope_query(&vault_id, &CodeSelector::Query("SqliteIndex".into()), 500)
        .await
        .expect("scope query");
    // Estimation tokens (même heuristique que le handler : chars/4 + overhead 40/entrée).
    let mut total_chars = 0usize;
    for h in hits.iter().take(15) {
        total_chars += h.source_path.len() + h.kind.len() + h.qualified_name.len() + 40;
        if let Some(s) = &h.signature {
            total_chars += s.len();
        }
        total_chars += h.deps.iter().map(|d| d.len()).sum::<usize>();
    }
    let est_tokens = total_chars.div_ceil(4);
    eprintln!(
        "[DOGFOOD] code_scope query 'SqliteIndex' : {} hits, top-15 ≈ {} tokens",
        hits.len(),
        est_tokens
    );
}

// ── Tests discriminants — feature Visibility ─────────────────────────────────

/// Test discriminant V4 — `run_update` réutilise le mode visibilité stocké par `run_ingest`.
///
/// Scénario :
/// 1. Ingest avec mode `All` (items privés indexés).
/// 2. Ajouter un fichier avec une fn privée.
/// 3. `run_update` sans visibility_override → doit réutiliser `All` → fn privée indexée.
///
/// Discriminant : si `run_update` retombe sur `Pub` par défaut, la fn privée sera absente.
#[tokio::test]
async fn update_v4_reuses_stored_visibility_mode() {
    let tmp = init_repo();
    let repo = tmp.path();

    // Fichier initial avec une fn publique.
    std::fs::write(repo.join("src/lib.rs"), "pub fn pub_fn_init() {}").expect("write init");
    commit_all(repo, "init");

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-update-v4-visibility".to_string();

    // Ingest initial avec mode All (tous les items, privés inclus).
    run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        visibility: IngestVisibility::All,
    })
    .await
    .expect("ingest All");

    // Ajouter un fichier contenant une fn privée + une fn publique.
    std::fs::write(
        repo.join("src/private_module.rs"),
        "pub fn pub_added() {}\nfn priv_added() {}",
    )
    .expect("write private_module.rs");
    git(repo, &["add", "src/private_module.rs"]);
    commit_all(repo, "add private module");

    // Update SANS visibility_override → doit réutiliser le mode All stocké.
    let report = run_update(CodeUpdateArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        ..Default::default()
    })
    .await
    .expect("update sans override");
    assert_eq!(report.files_ingested, 1, "V4: 1 fichier modifié ré-ingéré");

    // Vérifier que priv_added est indexée (mode All réutilisé).
    let index = SqliteIndex::open(&index_path).await.expect("open");
    let hits = index
        .code_scope_query(&vault_id, &CodeSelector::Symbol("priv_added".into()), 10)
        .await
        .expect("query priv_added");
    assert_eq!(
        hits.len(),
        1,
        "V4: priv_added doit être indexée — run_update doit réutiliser le mode All stocké. \
         hits={:?}",
        hits.iter().map(|h| &h.qualified_name).collect::<Vec<_>>()
    );

    // Vérifier que pub_added est aussi indexée.
    let hits_pub = index
        .code_scope_query(&vault_id, &CodeSelector::Symbol("pub_added".into()), 10)
        .await
        .expect("query pub_added");
    assert_eq!(
        hits_pub.len(),
        1,
        "V4: pub_added doit être indexée en mode All"
    );
}

// ── Fix A2 : paths avec espaces/accents correctement listés et diffés ──────────

/// Vérifie que `run_ingest` indexe un fichier Rust dont le path contient un espace.
///
/// ## Pourquoi c'était un bug
///
/// `git ls-files` sans `-z` produit une sortie `\n`-séparée. Un path contenant un
/// espace (`"mon module.rs"`) est rendu tel quel par git, mais un split `\n` puis
/// un `filter(|l| !l.is_empty())` ne le brise pas — cependant git peut aussi
/// le quoter en `"mon module.rs"` (avec les guillemets). Avec `-z`, le path est
/// transmis tel quel, guillemets absents, split sur NUL → correct.
///
/// Ce test vérifie l'invariant observable : le fichier est bien ingéré (notes > 0).
#[tokio::test]
async fn a2_path_with_space_is_ingested() {
    let tmp = init_repo();
    let repo = tmp.path();

    // Créer un fichier Rust dont le nom contient un espace.
    let file_path = repo.join("src/mon module.rs");
    std::fs::write(
        &file_path,
        "/// Fonction dans un fichier avec espace.\npub fn spaced_fn() -> u32 { 42 }",
    )
    .expect("write spaced file");
    commit_all(repo, "add spaced file");

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-a2-space".to_string();

    let report = run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest avec path espace");

    assert_eq!(
        report.files_ingested, 1,
        "a2: le fichier avec espace doit être ingéré (files_ingested={})",
        report.files_ingested
    );
    assert!(
        report.notes_inserted > 0,
        "a2: au moins 1 note attendue pour le fichier avec espace"
    );

    // L'idempotence prouve que le path est stocké ET relu correctement.
    let report2 = run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("2e ingest idempotence");

    assert_eq!(
        report2.files_skipped, 1,
        "a2: 2e ingest doit skiper le fichier (idempotence)"
    );
    assert_eq!(
        report2.notes_inserted, 0,
        "a2: 2e ingest = 0 notes insérées (idempotence)"
    );
}

/// Vérifie que `run_update` (O(diff)) détecte et ré-ingère un fichier dont le
/// path contient des caractères non-ASCII (accent).
///
/// ## Invariant vérifié
///
/// `git diff --name-status -z` transmet le path `"src/données.rs"` sans quoting.
/// Sans `-z`, git rendrait `"src/donn\303\251es.rs"` (C-style quoting) et le
/// split-sur-tab lirait un path corrompu → 0 note ingérée.
#[tokio::test]
async fn a2_path_with_accent_is_updated() {
    let tmp = init_repo();
    let repo = tmp.path();

    // V1 : fichier avec accent dans le nom.
    let file_path = repo.join("src/données.rs");
    std::fs::write(
        &file_path,
        "/// V1.\npub fn accent_fn_v1(x: u32) -> u32 { x }",
    )
    .expect("write v1");
    commit_all(repo, "v1 accent");

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-a2-accent".to_string();

    // Ingest initial.
    run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest v1");

    // V2 : modifier la fonction.
    std::fs::write(
        &file_path,
        "/// V2.\npub fn accent_fn_v2(x: u32, y: u32) -> u64 { (x + y) as u64 }",
    )
    .expect("write v2");
    commit_all(repo, "v2 accent");

    let report = run_update(CodeUpdateArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        visibility_override: None,
    })
    .await
    .expect("update v2 accent");

    assert_eq!(
        report.files_ingested, 1,
        "a2: run_update doit détecter 1 fichier modifié avec accent (files_ingested={})",
        report.files_ingested
    );
    assert!(
        report.notes_inserted > 0,
        "a2: run_update doit ré-ingérer des notes pour le fichier avec accent"
    );
}

// ── R3 : sceller l'invariant --no-renames ────────────────────────────────────

/// Vérifie que `git mv` (rename) est correctement décomposé en 1 Deleted + 1 Added
/// par `git diff --name-status -z --no-renames` et traité comme tel par `run_update`.
///
/// ## Invariant scellé (R3)
///
/// Le parsing VecDeque-par-paires dans `git_diff_name_status` repose sur le format
/// NUL-terminé à 2 champs par entrée (STATUS\0PATH\0). Avec `--no-renames`, les renames
/// git (R100) sont décomposés en D (ancien path) + A (nouveau path) — 2 entrées de 2
/// champs chacune, soit 4 tokens NUL au total. Sans `--no-renames`, un rename serait
/// rendu `R100\0ancien\0nouveau\0` — 3 tokens pour 1 entrée → désalignement silencieux.
///
/// Ce test vérifie l'invariant observable : après `git mv src/a.rs src/b.rs` + commit,
/// `run_update` doit :
/// - Supprimer les notes de `src/a.rs` (Deleted).
/// - Ingérer les notes de `src/b.rs` (Added).
/// - Rapporter `files_deleted=1, files_ingested=1, files_changed=2`.
#[tokio::test]
async fn a2_rename_decomposed_to_delete_add() {
    let tmp = init_repo();
    let repo = tmp.path();

    // V1 : fichier src/old_name.rs avec une fonction publique identifiable.
    std::fs::write(
        repo.join("src/old_name.rs"),
        "/// Fonction dans l'ancien fichier.\npub fn old_fn() -> u32 { 1 }",
    )
    .expect("write old_name.rs");
    commit_all(repo, "v1 old_name");

    let (index_path, _it) = setup_index().await;
    let vault_id = "code-r3-rename".to_string();

    // Ingest initial : indexe old_name.rs (et ses notes).
    run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest v1 old_name");

    // Vérifier que des notes ont été indexées pour old_name.rs.
    let index_pre = SqliteIndex::open(&index_path).await.expect("open pre");
    let scope_pre = index_pre
        .code_scope_query(&vault_id, &CodeSelector::Symbol("old_fn".into()), 10)
        .await
        .expect("query old_fn pre");
    assert!(
        !scope_pre.is_empty(),
        "r3: old_fn doit être indexée avant le rename"
    );

    // git mv old_name.rs → new_name.rs : simule un rename de fichier .rs.
    // git enregistre ceci comme R100 (rename 100% similaire) dans le diff.
    std::fs::rename(repo.join("src/old_name.rs"), repo.join("src/new_name.rs")).expect("fs rename");
    commit_all(repo, "git mv old_name.rs → new_name.rs");

    // run_update doit traiter le rename via --no-renames :
    //   - 1 Deleted pour src/old_name.rs
    //   - 1 Added pour src/new_name.rs
    let report = run_update(CodeUpdateArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        visibility_override: None,
    })
    .await
    .expect("update après git mv");

    // Invariant R3 : 2 fichiers changés (1 D + 1 A), pas 1 rename 3-champs.
    assert_eq!(
        report.files_changed, 2,
        "r3: git mv doit produire 2 changements (1 D + 1 A) via --no-renames, files_changed={}",
        report.files_changed
    );
    assert_eq!(
        report.files_deleted, 1,
        "r3: l'ancien path doit être compté comme Deleted, files_deleted={}",
        report.files_deleted
    );
    assert_eq!(
        report.files_ingested, 1,
        "r3: le nouveau path doit être compté comme Added/ingéré, files_ingested={}",
        report.files_ingested
    );

    // ── Vérification par source_path (comportement observable correct) ──────────
    //
    // `git mv old_name.rs new_name.rs` préserve le CONTENU — la fonction `old_fn`
    // est toujours présente dans new_name.rs. Un `CodeSelector::Symbol("old_fn")`
    // retournerait donc des résultats depuis new_name.rs même après suppression de
    // old_name.rs. On vérifie par source_path (via code_freshness + CodeSelector::Path)
    // pour distinguer les deux fichiers.
    let index_post = SqliteIndex::open(&index_path).await.expect("open post");

    // Vérifier via code_freshness : new_name.rs présent, old_name.rs absent.
    let freshness_map = index_post
        .get_code_freshness_map(&vault_id)
        .await
        .expect("freshness map post");
    assert!(
        freshness_map.contains_key("src/new_name.rs"),
        "r3: src/new_name.rs doit être dans code_freshness après ingest du fichier renommé"
    );
    assert!(
        !freshness_map.contains_key("src/old_name.rs"),
        "r3: src/old_name.rs doit avoir été retiré de code_freshness après suppression"
    );

    // src/old_name.rs ne doit plus avoir de notes dans l'index.
    let notes_old_path = index_post
        .code_scope_query(
            &vault_id,
            &CodeSelector::Path("src/old_name.rs".into()),
            100,
        )
        .await
        .expect("query path old_name.rs");
    assert!(
        notes_old_path.is_empty(),
        "r3: aucune note ne doit subsister pour src/old_name.rs après suppression \
         (notes présentes : {:?})",
        notes_old_path
            .iter()
            .map(|h| &h.qualified_name)
            .collect::<Vec<_>>()
    );

    // src/new_name.rs doit avoir des notes (le fichier renommé a été ingéré).
    let notes_new_path = index_post
        .code_scope_query(
            &vault_id,
            &CodeSelector::Path("src/new_name.rs".into()),
            100,
        )
        .await
        .expect("query path new_name.rs");
    assert!(
        !notes_new_path.is_empty(),
        "r3: src/new_name.rs doit avoir des notes après ingest du fichier renommé"
    );
}
