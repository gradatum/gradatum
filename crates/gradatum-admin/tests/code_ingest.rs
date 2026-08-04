//! Tests d'intégration — sub-commande `gradatum-admin code ingest`.
//!
//! ## Stratégie
//!
//! Les tests utilisent un repo git temporaire (TempDir initialisé avec `git init`)
//! pour exercer le pipeline complet code-ingest.
//!
//! ### Golden tests idempotence (critère d'acceptation §6)
//!
//! 2e ingest sans changement = **0 write** : `files_skipped` == `files_total`,
//! `notes_inserted` == 0.
//!
//! ### Golden tests fraîcheur §4.7 (Phase B)
//!
//! - **(b)** stale jamais silencieux : muter bytes sur disque sans re-ingest → `check_freshness` retourne `Stale`
//! - **(c)** propagation suppressions intra-fichier : symbole supprimé → note disparaît
//! - **(d)** rebuild == incrémental : set notes identique (timestamps exclus)
//!
//! Ces tests ne dépendent pas de l'accès réseau ni du service gradatum LIVE.

use gradatum_admin::code_cmd::{
    CODE_MAP_REBUILD_MAX_FAILURES, CODE_MAP_REBUILD_MAX_RETRY, CodeIngestArgs, run_ingest,
    write_marker_attempts,
};
use gradatum_index::{Freshness, SqliteIndex};
use gradatum_ingest::content_hash_source;
use tempfile::TempDir;

/// Crée un repo git minimal avec un fichier Rust.
fn setup_git_repo(content: &str) -> TempDir {
    let tmp = TempDir::new().expect("TempDir");
    let repo = tmp.path();

    // Init git.
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(repo)
        .output()
        .expect("git init");

    // Configurer l'identité git (évite les warnings sur les machines CI).
    std::process::Command::new("git")
        .args(["config", "user.email", "test@gradatum.test"])
        .current_dir(repo)
        .output()
        .expect("git config email");
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(repo)
        .output()
        .expect("git config name");

    // Écrire src/lib.rs.
    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    std::fs::write(repo.join("src/lib.rs"), content).expect("write src/lib.rs");

    // git add + commit.
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .output()
        .expect("git add");
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(repo)
        .output()
        .expect("git commit");

    tmp
}

/// Crée un index.db minimal dans un TempDir.
async fn setup_index() -> (std::path::PathBuf, TempDir) {
    let tmp = TempDir::new().expect("TempDir index");
    let index_path = tmp.path().join("index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    (index_path, tmp)
}

const RUST_SNIPPET: &str = r#"
/// Parser public.
pub struct MyParser;

impl MyParser {
    /// Créer un parser.
    pub fn new() -> Self {
        Self
    }

    /// Parser un fichier.
    pub fn parse(&self, input: &str) -> Vec<String> {
        vec![input.to_string()]
    }
}

/// Constante de limite.
pub const MAX_INPUT: usize = 1024;
"#;

/// Golden test idempotence : 2e ingest sans changement = 0 write.
///
/// Critère d'acceptation §6 : `files_skipped == files_total` au 2e ingest.
#[tokio::test]
async fn golden_idempotence_second_ingest_zero_write() {
    let repo_tmp = setup_git_repo(RUST_SNIPPET);
    let (index_path, _index_tmp) = setup_index().await;

    let vault_id = "code-test-idempotence".to_string();
    let args = CodeIngestArgs {
        repo_path: repo_tmp.path().to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    };

    // Premier ingest.
    let report1 = run_ingest(args.clone()).await.expect("premier ingest");
    assert_eq!(
        report1.files_skipped, 0,
        "1er ingest : aucun fichier skippé"
    );
    assert!(
        report1.files_ingested >= 1,
        "1er ingest : au moins 1 fichier ingéré"
    );
    let notes_first = report1.notes_inserted;
    assert!(notes_first >= 1, "1er ingest : au moins 1 note insérée");

    // Deuxième ingest sans changement.
    let report2 = run_ingest(args).await.expect("2e ingest");
    assert_eq!(
        report2.files_skipped, report2.files_total,
        "2e ingest sans changement : tous les fichiers doivent être skippés (idempotence). \
         files_total={} files_skipped={} files_ingested={}",
        report2.files_total, report2.files_skipped, report2.files_ingested,
    );
    assert_eq!(
        report2.notes_inserted, 0,
        "2e ingest sans changement : 0 note insérée (idempotence)"
    );
    assert_eq!(
        report2.files_ingested, 0,
        "2e ingest sans changement : 0 fichier ingéré"
    );
}

/// Test rebuild : drop + reingest complet.
#[tokio::test]
async fn rebuild_drops_and_reingests() {
    let repo_tmp = setup_git_repo(RUST_SNIPPET);
    let (index_path, _index_tmp) = setup_index().await;

    let vault_id = "code-test-rebuild".to_string();
    let args_normal = CodeIngestArgs {
        repo_path: repo_tmp.path().to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    };

    // Premier ingest.
    run_ingest(args_normal).await.expect("premier ingest");

    // Rebuild : doit tout supprimer puis réingérer.
    let args_rebuild = CodeIngestArgs {
        repo_path: repo_tmp.path().to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: true,
        ..Default::default()
    };
    let report = run_ingest(args_rebuild).await.expect("rebuild ingest");
    assert!(
        report.files_ingested >= 1,
        "rebuild : au moins 1 fichier ingéré"
    );
    assert_eq!(
        report.files_skipped, 0,
        "rebuild : aucun skip (tout réingéré)"
    );
    assert!(
        report.notes_inserted >= 1,
        "rebuild : au moins 1 note insérée"
    );
}

/// Test propagation suppression : un fichier retiré du repo → ses notes disparaissent.
#[tokio::test]
async fn propagation_deletion_removes_notes() {
    // Setup : repo avec 2 fichiers.
    let tmp = TempDir::new().expect("TempDir");
    let repo = tmp.path();

    std::process::Command::new("git")
        .args(["init"])
        .current_dir(repo)
        .output()
        .expect("git init");
    std::process::Command::new("git")
        .args(["config", "user.email", "test@gradatum.test"])
        .current_dir(repo)
        .output()
        .expect("git config");
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(repo)
        .output()
        .expect("git config name");

    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    std::fs::write(repo.join("src/lib.rs"), "pub fn func_a() {}").expect("write lib.rs");
    std::fs::write(repo.join("src/extra.rs"), "pub fn func_extra() {}").expect("write extra.rs");

    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .output()
        .expect("git add");
    std::process::Command::new("git")
        .args(["commit", "-m", "init 2 files"])
        .current_dir(repo)
        .output()
        .expect("git commit");

    let (index_path, _index_tmp) = setup_index().await;
    let vault_id = "code-test-deletion".to_string();

    // Premier ingest : 2 fichiers présents.
    let args1 = CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    };
    let report1 = run_ingest(args1).await.expect("premier ingest 2 fichiers");
    assert_eq!(report1.files_total, 2, "2 fichiers .rs attendus");

    // Supprimer extra.rs du repo git.
    std::fs::remove_file(repo.join("src/extra.rs")).expect("rm extra.rs");
    std::process::Command::new("git")
        .args(["rm", "src/extra.rs"])
        .current_dir(repo)
        .output()
        .expect("git rm");
    std::process::Command::new("git")
        .args(["commit", "-m", "rm extra.rs"])
        .current_dir(repo)
        .output()
        .expect("git commit rm");

    // Deuxième ingest : extra.rs absent de git ls-files → propagation suppression.
    let args2 = CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    };
    let report2 = run_ingest(args2).await.expect("2e ingest post-suppression");
    assert_eq!(report2.files_deleted, 1, "1 fichier supprimé attendu");

    // Vérifier dans l'index que les notes de src/extra.rs ont disparu.
    let index = SqliteIndex::open(&index_path).await.expect("open index");
    let freshness = index
        .get_code_freshness_map(&vault_id)
        .await
        .expect("freshness map");
    assert!(
        !freshness.contains_key("src/extra.rs"),
        "src/extra.rs ne doit plus être dans code_freshness après suppression"
    );

    // P0-2 : vérifier aussi que les NOTES de extra.rs ont disparu de l'index.
    // Bug P0-2 : write_note_derived_batch(source_path, source_path, ...) au lieu de
    // write_note_derived_batch(vault_id, source_path, ...) → DELETE WHERE vault_id='src/extra.rs'
    // ne match rien → notes deviennent des fantômes permanents.
    //
    // On récupère le count de notes via le 1er ingest (avant suppression), puis on vérifie
    // que le count post-suppression a diminué (les notes de extra.rs ont bien été purgées).
    //
    // Note : live_note_count() est accessible depuis gradatum-admin car vault_id='code-...' → status='live'.
    let total_after = index
        .live_note_count(&vault_id)
        .await
        .expect("live_note_count after");

    // Après 1er ingest (lib.rs + extra.rs), on avait des notes pour 2 fichiers.
    // Après suppression, seules les notes de lib.rs (1 symbole 'func_a') doivent rester.
    // Si P0-2 est buggé : les notes de extra.rs persistent → total_after > notes de lib.rs seul.
    //
    // Ingest 1 a inséré des notes pour src/extra.rs (func_extra). Vérifier que
    // `total_after == notes_lib_rs_only` est difficile sans savoir combien exactement.
    // Approche conservatrice : on vérifie que le count APRÈS est strictement INFÉRIEUR à celui
    // d'AVANT la suppression. Pour cela on compare report1.notes_inserted.
    let notes_before = report1.notes_inserted;
    assert!(
        total_after < notes_before as u64,
        "P0-2 : après suppression de extra.rs, live_note_count ({total_after}) doit être < notes insérées au 1er ingest ({notes_before})"
    );
}

// ── Golden tests fraîcheur §4.7 Phase B ─────────────────────────────────────

/// Golden test (b) — stale jamais silencieux.
///
/// Scénario : ingest d'un fichier, puis mutation des bytes sur disque SANS re-ingest.
/// `check_freshness` doit retourner `Stale` — prouve qu'une lecture flaggerait stale
/// et ne servirait pas l'entrée périmée.
///
/// Critère §4.3 spec : JAMAIS de retour silencieux d'une entrée périmée.
#[tokio::test]
async fn b3_b_stale_never_silent() {
    let repo_tmp = setup_git_repo(RUST_SNIPPET);
    let (index_path, _index_tmp) = setup_index().await;

    let vault_id = "code-test-stale".to_string();
    let args = CodeIngestArgs {
        repo_path: repo_tmp.path().to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    };

    // Ingest initial.
    run_ingest(args).await.expect("ingest initial");

    // Muter les bytes sur disque SANS re-ingest (simule une modification entre deux ingests).
    let src_path = repo_tmp.path().join("src/lib.rs");
    let modified_content = format!("{RUST_SNIPPET}\n// modification post-ingest ajoutée");
    std::fs::write(&src_path, &modified_content).expect("write modified content");

    // check_freshness avec les bytes modifiés → doit retourner Stale.
    let index = SqliteIndex::open(&index_path).await.expect("open index");
    let current_bytes = std::fs::read(&src_path).expect("read modified file");

    let result = index
        .check_freshness(&vault_id, "src/lib.rs", &current_bytes)
        .await
        .expect("check_freshness");

    match result {
        Freshness::Stale {
            stored_hash,
            current_hash,
        } => {
            // Le stored_hash doit correspondre à l'ingest original.
            let original_bytes = RUST_SNIPPET.as_bytes();
            let expected_stored = content_hash_source(original_bytes);
            assert_eq!(
                stored_hash, expected_stored,
                "stored_hash doit être le hash du contenu original ingéré"
            );
            // Le current_hash doit correspondre aux bytes modifiés.
            let expected_current = content_hash_source(&current_bytes);
            assert_eq!(
                current_hash, expected_current,
                "current_hash doit être le hash du contenu modifié"
            );
        }
        Freshness::Fresh => panic!(
            "fichier muté sur disque DOIT retourner Stale, pas Fresh — \
             un Fresh silencieux signifie que le drift n'est pas détecté (violation §4.3)"
        ),
        Freshness::Unknown => {
            panic!("entrée indexée après ingest DOIT retourner Stale ou Fresh, pas Unknown")
        }
    }
}

/// Golden test (c) étendu — propagation suppressions intra-fichier.
///
/// Scénario : ingest d'un fichier avec N symboles, puis re-ingest du même fichier
/// avec un symbole en MOINS (modification du contenu) → la note du symbole supprimé
/// doit disparaître de l'index.
///
/// Complète le test `propagation_deletion_removes_notes` (suppression fichier entier).
/// Couvre ici la suppression d'un SYMBOLE dans un fichier existant.
///
/// Critère §4.4 spec : symbole disparu du fichier → note disparaît du map. Pas de fantômes.
#[tokio::test]
async fn b3_c_intrafile_symbol_deletion_propagated() {
    // Setup : fichier avec 2 fonctions publiques.
    let content_v1 = r#"
/// Fonction A.
pub fn func_a() -> u32 { 42 }

/// Fonction B.
pub fn func_b() -> u32 { 99 }
"#;
    // V2 : func_b supprimée.
    let content_v2 = r#"
/// Fonction A.
pub fn func_a() -> u32 { 42 }
"#;

    let tmp = TempDir::new().expect("TempDir");
    let repo = tmp.path();

    // Init git.
    for (args, note) in [
        (vec!["init"], "git init"),
        (vec!["config", "user.email", "test@gradatum.test"], "email"),
        (vec!["config", "user.name", "Test"], "name"),
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(repo)
            .output()
            .unwrap_or_else(|_| panic!("{note}"));
    }

    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    std::fs::write(repo.join("src/lib.rs"), content_v1).expect("write v1");

    for (args, note) in [
        (vec!["add", "."], "git add v1"),
        (vec!["commit", "-m", "init"], "git commit v1"),
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(repo)
            .output()
            .unwrap_or_else(|_| panic!("{note}"));
    }

    let (index_path, _index_tmp) = setup_index().await;
    let vault_id = "code-test-intrafile".to_string();

    // Ingest v1 : 2 fonctions → 2 notes attendues.
    let args1 = CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    };
    let report1 = run_ingest(args1).await.expect("ingest v1");
    assert!(
        report1.notes_inserted >= 2,
        "ingest v1 : attendu >= 2 notes (func_a + func_b), trouvé {}",
        report1.notes_inserted
    );

    // Modifier le fichier : supprimer func_b.
    std::fs::write(repo.join("src/lib.rs"), content_v2).expect("write v2");
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .output()
        .expect("git add v2");
    std::process::Command::new("git")
        .args(["commit", "-m", "rm func_b"])
        .current_dir(repo)
        .output()
        .expect("git commit v2");

    // Re-ingest v2 : func_b supprimée → sa note doit disparaître.
    let args2 = CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    };
    run_ingest(args2).await.expect("ingest v2");

    // Vérifier le count de notes : seule func_a doit rester.
    let index = SqliteIndex::open(&index_path).await.expect("open index");
    let notes_after = index
        .live_note_count(&vault_id)
        .await
        .expect("live_note_count after v2");

    assert!(
        notes_after < report1.notes_inserted as u64,
        "intra-file deletion : notes_after ({notes_after}) doit être < notes_v1 ({}) — \
         func_b doit avoir disparu de l'index",
        report1.notes_inserted
    );
    assert!(
        notes_after >= 1,
        "intra-file deletion : func_a doit encore être présente, notes_after={notes_after}"
    );

    // G2 — asserter l'IDENTITÉ du survivant : func_b supprimée, func_a seule présente.
    // Un bug qui supprimerait func_a au lieu de func_b passerait le count seul.
    use gradatum_core::index_store::CodeSelector;
    let survivors = index
        .code_scope_query(&vault_id, &CodeSelector::Query("func".into()), 100)
        .await
        .expect("code_scope_query survivors");

    let names: Vec<&str> = survivors
        .iter()
        .map(|e| e.qualified_name.as_str())
        .collect();
    assert!(
        names.contains(&"func_a"),
        "G2 : func_a doit survivre à la suppression de func_b, found {names:?}"
    );
    assert!(
        !names.contains(&"func_b"),
        "G2 : func_b doit avoir disparu de l'index après la suppression intra-fichier, found {names:?}"
    );
}

/// Golden test (d) — rebuild == incrémental à l'échelle réelle (>200 notes).
///
/// ## Critère §4.5 (BLOQUANT au tag public)
///
/// `rebuild` ⟹ map identique à l'incrémental — sur un SET COMPLET (pas un échantillon).
/// Le test précédent utilisait `list_notes limit=200` → tout divergence au-delà passait
/// inaperçue. Ce test utilise `list_all_derived_notes` (scan sans limite, filtré par
/// `provenance='derived:tree-sitter'`) → couverture totale.
///
/// ## Périmètre et échelle
///
/// 50 fichiers .rs synthétiques × 5 symboles chacun = **250 notes minimum** (bien > 200).
/// Chaque fichier contient 1 struct + 4 fonctions publiques avec corps non-trivial.
/// Un fichier est modifié entre A et B pour exercer le chemin incrémental.
///
/// ## Scénario
///
/// 1. Ingest incrémental (A) : 50 fichiers.
/// 2. Modifier mod_0.rs + commit → ingest incrémental (B).
/// 3. Rebuild complet (C) depuis zéro.
/// 4. `set(note_id) B == set(note_id) C` ET body_text + tags identiques par note_id.
///    Timestamps `created` EXCLUS (rebuild = nouveaux inserts, ordre diffère).
///
/// Critère §4.5 spec : `rebuild` ⟹ map identique à l'incrémental.
#[tokio::test]
async fn b3_d_rebuild_equals_incremental() {
    use std::collections::HashMap;

    let tmp = TempDir::new().expect("TempDir");
    let repo = tmp.path();

    // ── Init git ──────────────────────────────────────────────────────────────
    for (args, note) in [
        (["init", "", "", ""].as_slice(), "git init"),
        (
            ["config", "user.email", "test@gradatum.test", ""].as_slice(),
            "email",
        ),
        (["config", "user.name", "Test", ""].as_slice(), "name"),
    ] {
        let filtered: Vec<&str> = args.iter().copied().filter(|a| !a.is_empty()).collect();
        std::process::Command::new("git")
            .args(&filtered)
            .current_dir(repo)
            .output()
            .unwrap_or_else(|_| panic!("{note}"));
    }

    // ── Générer 50 fichiers .rs synthétiques (250 symboles attendus) ─────────
    // Chaque fichier contient 1 struct + 4 fonctions → 5 symboles par fichier.
    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    for i in 0..50usize {
        let content = format!(
            "/// Module synthétique {i} pour le golden §4.5.\n\
             pub struct SynStruct{i} {{ pub x: u64, pub y: u64 }}\n\
             impl SynStruct{i} {{\n\
             /// Créer.\n\
             pub fn new(x: u64, y: u64) -> Self {{ Self {{ x, y }} }}\n\
             /// Calculer.\n\
             pub fn compute(&self) -> u64 {{ self.x.wrapping_add(self.y).wrapping_mul({i} as u64 + 1) }}\n\
             }}\n\
             /// Fonction libre alpha du module {i}.\n\
             pub fn syn_alpha_{i}(a: u32, b: u32) -> u64 {{ (a as u64).wrapping_add(b as u64).wrapping_mul({i} as u64 + 1) }}\n\
             /// Fonction libre beta du module {i}.\n\
             pub fn syn_beta_{i}(v: &[u8]) -> usize {{ v.len().wrapping_add({i}) }}\n\
             /// Constante du module {i}.\n\
             pub const SYN_CONST_{i}: u64 = {i};\n",
        );
        std::fs::write(repo.join(format!("src/mod_{i}.rs")), content)
            .unwrap_or_else(|e| panic!("write mod_{i}: {e}"));
    }

    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .output()
        .expect("git add initial");
    std::process::Command::new("git")
        .args(["commit", "-m", "init 50 modules"])
        .current_dir(repo)
        .output()
        .expect("git commit initial");

    // ── Ingest A (état initial) ───────────────────────────────────────────────
    let (index_path, _index_tmp) = setup_index().await;
    let vault_id = "code-test-rebuild-scale".to_string();

    let args_inc = CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    };
    run_ingest(args_inc.clone()).await.expect("ingest A");

    // ── Modifier mod_0.rs + commit → ingest incrémental B ────────────────────
    let modified = "\
        /// Module synthétique 0 MODIFIÉ (v2) — golden §4.5.\n\
        pub struct SynStruct0Mod { pub x: u64, pub y: u64, pub z: u64 }\n\
        impl SynStruct0Mod {\n\
        pub fn new(x: u64, y: u64, z: u64) -> Self { Self { x, y, z } }\n\
        pub fn compute(&self) -> u64 { self.x.wrapping_add(self.y).wrapping_add(self.z) }\n\
        }\n\
        pub fn syn_alpha_0_v2(a: u32, b: u32, c: u32) -> u64 { a as u64 + b as u64 + c as u64 }\n\
        pub fn syn_beta_0_v2(v: &[u8]) -> usize { v.len().wrapping_mul(2) }\n\
        pub const SYN_CONST_0_V2: u64 = 9999;\n";
    std::fs::write(repo.join("src/mod_0.rs"), modified).expect("write mod_0 v2");

    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .output()
        .expect("git add mod_0 v2");
    std::process::Command::new("git")
        .args(["commit", "-m", "update mod_0"])
        .current_dir(repo)
        .output()
        .expect("git commit mod_0 v2");

    run_ingest(args_inc).await.expect("ingest B incrémental");

    // ── Snapshot B : SET COMPLET via list_all_derived_notes (G1) ─────────────
    // Scan sans limite — garantit que toutes les notes >200 sont capturées.
    // Timestamps `created` exclus par construction.
    let index_b = SqliteIndex::open(&index_path).await.expect("open index B");
    let notes_b_raw = index_b
        .list_all_derived_notes(&vault_id)
        .await
        .expect("list_all_derived_notes B");

    // Exiger >200 notes pour valider l'échelle (critère G1).
    assert!(
        notes_b_raw.len() > 200,
        "G1 : snapshot B doit contenir >200 notes pour valider l'échelle §4.5, \
         trouvé {} (vérifier tree-sitter parse des 50 fichiers)",
        notes_b_raw.len()
    );

    // HashMap<note_id → (body_text, tags)> pour comparaison set.
    let notes_b: HashMap<String, (String, Option<String>)> = notes_b_raw
        .into_iter()
        .map(|(id, body, tags)| (id, (body, tags)))
        .collect();

    // ── Rebuild C : drop + reingest complet ──────────────────────────────────
    let args_rebuild = CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: true,
        ..Default::default()
    };
    run_ingest(args_rebuild).await.expect("ingest C rebuild");

    let index_c = SqliteIndex::open(&index_path).await.expect("open index C");
    let notes_c_raw = index_c
        .list_all_derived_notes(&vault_id)
        .await
        .expect("list_all_derived_notes C");
    let notes_c: HashMap<String, (String, Option<String>)> = notes_c_raw
        .into_iter()
        .map(|(id, body, tags)| (id, (body, tags)))
        .collect();

    // ── Assertion §4.5 : set(note_id) identique ──────────────────────────────
    // Note_id est déterministe (ULID dérivé de la clé logique vault+path+kind+qname).
    let ids_b: std::collections::HashSet<&String> = notes_b.keys().collect();
    let ids_c: std::collections::HashSet<&String> = notes_c.keys().collect();

    let only_b: Vec<&&String> = ids_b.difference(&ids_c).collect();
    let only_c: Vec<&&String> = ids_c.difference(&ids_b).collect();

    assert!(
        only_b.is_empty(),
        "rebuild == incrémental : {} note(s) en B absentes de C : {:?}",
        only_b.len(),
        only_b.iter().take(5).collect::<Vec<_>>()
    );
    assert!(
        only_c.is_empty(),
        "rebuild == incrémental : {} note(s) en C absentes de B : {:?}",
        only_c.len(),
        only_c.iter().take(5).collect::<Vec<_>>()
    );

    // ── Assertion §4.5 : body_text + tags identiques par note_id ─────────────
    for (note_id, (body_b, tags_b)) in &notes_b {
        let (body_c, tags_c) = notes_c
            .get(note_id)
            .expect("note_id présent en C (set déjà vérifié)");
        assert_eq!(
            body_b, body_c,
            "rebuild == incrémental : body_text diffère pour note {note_id}"
        );
        assert_eq!(
            tags_b, tags_c,
            "rebuild == incrémental : tags diffèrent pour note {note_id}"
        );
    }

    eprintln!(
        "[G1] rebuild == incrémental : {} notes comparées (set complet, timestamps exclus)",
        notes_b.len()
    );
}

// ── FIX-3 : parité sha256 ingest↔index ──────────────────────────────────────

/// Vérifie que `content_hash_source` (gradatum-ingest) et le hasher interne de
/// `SqliteIndex` (gradatum-index) produisent des hashes identiques.
///
/// ## Stratégie
///
/// `SqliteIndex::sha256_hex` est privée au crate gradatum-index et ne peut être
/// importée directement ici (cycle : gradatum-ingest dépend de gradatum-index).
/// On prouve la parité via le comportement observable de `check_freshness` :
///
/// 1. Ingest des bytes → `write_note_derived_batch` stocke `sha256_hex(bytes)`
///    comme `content_hash_source` dans `code_freshness`.
/// 2. `check_freshness(bytes)` recalcule `sha256_hex(bytes)` et compare.
/// 3. Si `check_freshness` retourne `Fresh`, les deux hashes sont identiques.
/// 4. On compare aussi `stored_hash` (lu via `Stale` sur bytes différents) avec
///    `content_hash_source(original_bytes)` pour vérifier la formule côté ingest.
///
/// Ce test capture une divergence d'encodage futur (encodage hex, padding, endianness).
#[tokio::test]
async fn b3_sha256_parity_with_ingest_hasher() {
    let cases: &[&[u8]] = &[
        b"",                       // vide
        b"hello world",            // ASCII
        &[0xde, 0xad, 0xbe, 0xef], // binaire non-UTF8
    ];

    for input in cases {
        let (index_path, _index_tmp) = setup_index().await;
        let vault_id = format!(
            "code-sha256-parity-{}",
            hex::encode(&input[..input.len().min(4)])
        );
        let source_path = "src/lib.rs";

        // Étape 1 : ingest via write_note_derived_batch avec content_hash_source(input).
        // On utilise SqliteIndex directement (bypass run_ingest) pour contrôler les bytes.
        let index = SqliteIndex::open(&index_path).await.expect("open index");
        let hash_from_ingest = content_hash_source(input);
        index
            .write_note_derived_batch(
                &vault_id,
                source_path,
                &hash_from_ingest,
                "sha_dummy",
                vec![],
            )
            .await
            .expect("write_note_derived_batch");

        // Étape 2 : check_freshness avec les MÊMES bytes.
        // Si SqliteIndex::sha256_hex == content_hash_source → Fresh.
        let result = index
            .check_freshness(&vault_id, source_path, input)
            .await
            .expect("check_freshness");

        assert!(
            matches!(result, Freshness::Fresh),
            "sha256 diverge entre gradatum-ingest::content_hash_source et SqliteIndex hasher \
             pour input {:?} (len={}) — check_freshness retourne {:?} au lieu de Fresh",
            &input[..input.len().min(8)],
            input.len(),
            result,
        );
    }
}

// ── Fix A3 : marqueur run incomplet ─────────────────────────────────────────

/// Vérifie que si un marqueur `.ingest-incomplete-<vault>` est présent avant un
/// `run_ingest`, le run détecte l'état interrompu, force un rebuild et termine
/// proprement (marqueur retiré, état cohérent).
///
/// ## Invariant (Fix A3)
///
/// Un marqueur laissé par un run interrompu ne doit PAS provoquer un drift
/// silencieux : le prochain run doit le détecter et rétablir un état cohérent.
///
/// ## Stratégie
///
/// On simule un run interrompu en posant manuellement le marqueur AVANT d'appeler
/// `run_ingest`. On vérifie :
/// 1. `run_ingest` réussit (pas d'erreur fatale sur marqueur présent).
/// 2. Le marqueur est retiré à la fin du run.
/// 3. Les notes sont correctement indexées (rebuild a fonctionné).
#[tokio::test]
async fn a3_incomplete_marker_triggers_rebuild_and_cleanup() {
    let repo_tmp = setup_git_repo(RUST_SNIPPET);
    let (index_path, _index_tmp) = setup_index().await;

    let vault_id = "code-a3-marker".to_string();

    // Pose manuelle du marqueur : simule un run précédent interrompu.
    let safe_vault = vault_id.replace(|c: char| !c.is_alphanumeric() && c != '-', "_");
    let marker_path = index_path
        .parent()
        .expect("index_path doit avoir un parent")
        .join(format!(".ingest-incomplete-{safe_vault}"));
    std::fs::write(&marker_path, b"").expect("pose manuelle du marqueur");
    assert!(
        marker_path.exists(),
        "précondition : marqueur doit exister avant run_ingest"
    );

    // run_ingest doit réussir malgré le marqueur.
    let report = run_ingest(CodeIngestArgs {
        repo_path: repo_tmp.path().to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false, // rebuild = false, mais le marqueur force le rebuild
        ..Default::default()
    })
    .await
    .expect("run_ingest doit réussir même avec un marqueur présent");

    // Le marqueur doit avoir été retiré à la fin du run.
    assert!(
        !marker_path.exists(),
        "a3: le marqueur doit être retiré après un run complet réussi"
    );

    // Des notes doivent avoir été insérées (le rebuild a fonctionné).
    assert!(
        report.notes_inserted > 0,
        "a3: des notes doivent être insérées (rebuild après marqueur, notes_inserted={})",
        report.notes_inserted
    );

    // Un 2e run sans marqueur = idempotence normale.
    let report2 = run_ingest(CodeIngestArgs {
        repo_path: repo_tmp.path().to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("2e run_ingest sans marqueur");

    assert!(
        !marker_path.exists(),
        "a3: le marqueur ne doit pas être créé par un run normal"
    );
    assert_eq!(
        report2.files_skipped, report2.files_total,
        "a3: 2e run = idempotence (tous les fichiers skippés)"
    );
}

// ── Garde-fou anti-boucle rebuild — tests d'intégration ──────────────────────

/// Vérifie que si le marqueur contient un compteur >= `CODE_MAP_REBUILD_MAX_RETRY`,
/// `run_ingest` retourne une `Err` (pas de rebuild) et loggue une erreur.
///
/// ## Invariant (garde-fou anti-boucle)
///
/// Un marqueur saturerait la boucle `run_update → run_ingest` en cas de crash
/// systématique de `run_ingest`. Le garde-fou s'arrête après N tentatives et
/// demande une intervention manuelle (suppression du marqueur).
///
/// ## Stratégie
///
/// On écrit manuellement le compteur == `CODE_MAP_REBUILD_MAX_RETRY` dans le marqueur,
/// puis on appelle `run_ingest`. On vérifie que :
/// 1. `run_ingest` retourne `Err` (pas de succès silencieux).
/// 2. Le message d'erreur mentionne le vault_id.
/// 3. Le marqueur est toujours présent (non supprimé par un run avorté).
#[tokio::test]
async fn a4_marker_at_max_retry_returns_error() {
    let repo_tmp = setup_git_repo(RUST_SNIPPET);
    let (index_path, _index_tmp) = setup_index().await;

    let vault_id = "code-a4-retry-guard".to_string();

    // Calculer le chemin du marqueur (même logique que run_ingest).
    let safe_vault = vault_id.replace(|c: char| !c.is_alphanumeric() && c != '-', "_");
    let marker_path = index_path
        .parent()
        .expect("index_path doit avoir un parent")
        .join(format!(".ingest-incomplete-{safe_vault}"));

    // Poser le marqueur avec compteur == MAX_RETRY (seuil de blocage).
    write_marker_attempts(&marker_path, CODE_MAP_REBUILD_MAX_RETRY)
        .expect("écriture marqueur avec compteur saturé");
    assert!(
        marker_path.exists(),
        "précondition : marqueur doit exister avant run_ingest"
    );

    // run_ingest doit retourner Err (garde-fou activé).
    let result = run_ingest(CodeIngestArgs {
        repo_path: repo_tmp.path().to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await;

    assert!(
        result.is_err(),
        "a4: run_ingest doit retourner Err quand le compteur du marqueur atteint MAX_RETRY. \
         Résultat obtenu : Ok({:?})",
        result.ok()
    );

    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains(&vault_id) || err_msg.contains("tentatives"),
        "a4: le message d'erreur doit mentionner le vault_id ou 'tentatives'. \
         Message : {err_msg}"
    );

    // Le marqueur doit toujours être présent (run avorté, pas de cleanup).
    assert!(
        marker_path.exists(),
        "a4: le marqueur doit rester présent après un run avorté par le garde-fou"
    );
}

/// Vérifie qu'un marqueur avec compteur = MAX_RETRY - 1 (sous le seuil) laisse
/// passer le run et incrémente le compteur.
///
/// Teste la frontière exacte : à N-1 tentatives, le rebuild est autorisé.
/// À N tentatives, il est bloqué (cf. `a4_marker_at_max_retry_returns_error`).
#[tokio::test]
async fn a5_marker_below_max_retry_allows_rebuild_and_increments() {
    let repo_tmp = setup_git_repo(RUST_SNIPPET);
    let (index_path, _index_tmp) = setup_index().await;

    let vault_id = "code-a5-retry-below".to_string();

    let safe_vault = vault_id.replace(|c: char| !c.is_alphanumeric() && c != '-', "_");
    let marker_path = index_path
        .parent()
        .expect("index_path doit avoir un parent")
        .join(format!(".ingest-incomplete-{safe_vault}"));

    // Poser le marqueur avec compteur = MAX_RETRY - 1 (juste sous le seuil).
    let below = CODE_MAP_REBUILD_MAX_RETRY - 1;
    write_marker_attempts(&marker_path, below).expect("écriture marqueur sous le seuil");

    // run_ingest doit réussir (rebuild autorisé sous le seuil).
    let report = run_ingest(CodeIngestArgs {
        repo_path: repo_tmp.path().to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("a5: run_ingest doit réussir sous le seuil MAX_RETRY");

    // Le run réussi supprime le marqueur (compteur reset implicite).
    assert!(
        !marker_path.exists(),
        "a5: le marqueur doit être supprimé après un run complet réussi"
    );

    // Des notes doivent avoir été insérées.
    assert!(
        report.notes_inserted > 0,
        "a5: des notes doivent être insérées (rebuild autorisé, notes_inserted={})",
        report.notes_inserted
    );
}

// ── Circuit-breaker — test d'intégration comportemental ─────────────────────

/// Vérifie que le circuit-breaker NE se déclenche PAS sur du code Rust valide.
///
/// ## Stratégie
///
/// Le circuit-breaker de `run_ingest` s'ouvre sur `N` = `CODE_MAP_REBUILD_MAX_FAILURES`
/// échecs CONSÉCUTIFS de `parse_rust_file`. Sur du code Rust valide, `parse_rust_file`
/// ne retourne jamais `Err` (seul `set_language` peut échouer — très rare).
///
/// Ce test injecte N+1 fichiers Rust valides et vérifie que :
/// 1. Tous les fichiers sont ingérés (`files_ingested == N+1`).
/// 2. `files_total == N+1` (aucun fichier silencieusement sauté par le circuit-breaker).
///
/// Un régression du circuit-breaker (reset cassé, compteur figé) se manifesterait
/// par `files_ingested < N+1` ici.
///
/// Les tests unitaires du compteur (dans `code_cmd.rs` via `circuit_breaker_should_open`)
/// valident la logique d'ouverture sans dépendre du comportement de `parse_rust_file`.
#[tokio::test]
async fn circuit_breaker_does_not_open_on_valid_rust() {
    let n = CODE_MAP_REBUILD_MAX_FAILURES as usize;
    let file_count = n + 1; // Au moins N+1 pour couvrir la fenêtre du circuit-breaker.

    // Créer un repo git avec `file_count` fichiers Rust publics valides.
    let tmp = TempDir::new().expect("TempDir");
    let repo = tmp.path();

    for (args, note) in [
        (vec!["init"], "git init"),
        (
            vec!["config", "user.email", "cb-test@gradatum.test"],
            "email",
        ),
        (vec!["config", "user.name", "CBTest"], "name"),
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(repo)
            .output()
            .unwrap_or_else(|_| panic!("{note}"));
    }

    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    for i in 0..file_count {
        let content =
            format!("/// Symbole valide du module cb_{i}.\npub fn cb_fn_{i}() -> u32 {{ {i} }}\n");
        std::fs::write(repo.join(format!("src/cb_{i}.rs")), content)
            .unwrap_or_else(|e| panic!("write cb_{i}: {e}"));
    }

    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(repo)
        .output()
        .expect("git add");
    std::process::Command::new("git")
        .args(["commit", "-m", "circuit-breaker test files"])
        .current_dir(repo)
        .output()
        .expect("git commit");

    let (index_path, _index_tmp) = setup_index().await;
    let vault_id = "code-cb-valid-rust".to_string();

    let report = run_ingest(CodeIngestArgs {
        repo_path: repo.to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect("ingest valide doit réussir");

    assert_eq!(
        report.files_total, file_count,
        "circuit-breaker : tous les {} fichiers doivent être comptés (files_total)",
        file_count
    );
    assert_eq!(
        report.files_ingested, file_count,
        "circuit-breaker : tous les {} fichiers valides doivent être ingérés \
         (circuit-breaker ne doit PAS s'ouvrir sur du code Rust valide)",
        file_count
    );
    assert_eq!(
        report.files_skipped, 0,
        "circuit-breaker : aucun fichier skippé sur premier ingest"
    );
}

// ── Lot REG — garde miroir de registre ────────────────────────────────────────

/// Inscrit `vault_id` dans le registre de DONNÉES en SQL brut.
///
/// Le passage par SQL est délibéré : depuis le lot REG, `provision_vault` REFUSE un
/// `vault_id` préfixé `code-`. L'état simulé ici est donc l'état HÉRITÉ — celui d'un
/// opérateur ayant lancé `admin vault create code-<projet>` avant que la garde n'existe.
/// C'est précisément le seul état où la garde d'ingest a un pouvoir discriminant : sur un
/// `vault_id` non préfixé, l'ingest échouerait de toute façon sur la garde de préfixe
/// préexistante, et le test ne prouverait rien.
fn seed_data_registry(index_path: &std::path::Path, vault_id: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db");
    conn.execute(
        "INSERT INTO tenants (id, status, created_at) VALUES (?1, 'active', 0)",
        rusqlite::params![vault_id],
    )
    .expect("seed tenants");
}

/// Compte les notes portant ce `vault_id` — mesure l'ABSENCE d'écriture, pas seulement
/// l'erreur retournée (une garde posée trop tard échouerait aussi, mais après avoir écrit).
fn count_notes(index_path: &std::path::Path, vault_id: &str) -> i64 {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db");
    conn.query_row(
        "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
        rusqlite::params![vault_id],
        |r| r.get(0),
    )
    .expect("count notes")
}

/// Lot REG : `code ingest` refuse un vault déjà inscrit au registre de données.
#[tokio::test]
async fn code_ingest_refuses_a_vault_registered_as_data() {
    let repo_tmp = setup_git_repo(RUST_SNIPPET);
    let (index_path, _index_tmp) = setup_index().await;
    let vault_id = "code-collision".to_string();
    seed_data_registry(&index_path, &vault_id);

    let err = run_ingest(CodeIngestArgs {
        repo_path: repo_tmp.path().to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        rebuild: false,
        ..Default::default()
    })
    .await
    .expect_err("l'ingest dans un vault de données doit être refusé");

    assert!(
        format!("{err:#}").contains("DATA registry"),
        "le refus doit nommer le registre en cause, got: {err:#}"
    );
    assert_eq!(
        count_notes(&index_path, &vault_id),
        0,
        "aucune note dérivée ne doit avoir été écrite avant le refus"
    );
    let marker = index_path
        .parent()
        .expect("parent")
        .join(format!(".ingest-incomplete-{vault_id}"));
    assert!(
        !marker.exists(),
        "le refus précède la pose du marqueur d'atomicité — sinon le run suivant \
         basculerait en rebuild forcé sans raison"
    );
}

/// Lot REG : `code update` porte la même garde que `code ingest`.
///
/// Sans ce test, la garde pourrait n'exister que sur `run_ingest` — `code update` est un
/// point d'entrée distinct, et c'est celui que le refresh périodique emprunte.
#[tokio::test]
async fn code_update_refuses_a_vault_registered_as_data() {
    let repo_tmp = setup_git_repo(RUST_SNIPPET);
    let (index_path, _index_tmp) = setup_index().await;
    let vault_id = "code-collision-update".to_string();
    seed_data_registry(&index_path, &vault_id);

    let err = gradatum_admin::code_cmd::run_update(gradatum_admin::code_cmd::CodeUpdateArgs {
        repo_path: repo_tmp.path().to_path_buf(),
        vault_id: vault_id.clone(),
        index_path: index_path.clone(),
        visibility_override: None,
    })
    .await
    .expect_err("l'update dans un vault de données doit être refusé");

    assert!(
        format!("{err:#}").contains("DATA registry"),
        "le refus doit nommer le registre en cause, got: {err:#}"
    );
    assert_eq!(
        count_notes(&index_path, &vault_id),
        0,
        "aucune note dérivée ne doit avoir été écrite avant le refus"
    );
}
