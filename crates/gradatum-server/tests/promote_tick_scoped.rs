//! Verrou d'invariant INV-JOB-SCOPE + typage `VaultId` du tick review-promote per-vault.
//!
//! `promote_tick` (`review_promote.rs`) est DÉJÀ per-vault
//! (`list_active_vaults()` + boucle `find_promotable_in_vault(&vault_id)`, sans
//! croisement de vaults). Ce fichier :
//!
//! 1. **verrouille l'invariant par test** — un job scopé à `vault-b` ne voit JAMAIS
//!    les notes de `main` (`find_promotable_in_vault` filtre strictement par `vault_id`) ;
//! 2. **verrouille le typage** — les méthodes du trait [`gradatum_core::IndexStore`]
//!    consommées par la boucle exposent des newtypes `VaultId` (compile-fail tant
//!    qu'elles prenaient `&str` ou retournaient `Vec<String>`) ;
//! 3. **verrouille le routage A1** (`promote_tick_promotes_in_correct_vault`) — le write
//!    est routé vers le handle du vault CIBLE (`vaults.resolve`), jamais le singleton `main` ;
//! 4. **verrouille le caveat C2(b)** (`promote_tick_honors_per_vault_review_promote_disable`)
//!    — un override `[per_vault.<id>.review_promote] enabled = false` SAUTE ce vault
//!    (`continue`) alors que la promotion globale est active. Pendant symétrique du footgun
//!    salience C1, dont la couverture vit dans `vault_search_salience_per_vault_on.rs` (C1 +
//!    C2(a), salience uniquement) : les deux moitiés du per-vault A6 sont donc verrouillées
//!    ici pour `review_promote`, là pour `salience` — ce fichier N'EST PAS limité à
//!    INV-JOB-SCOPE malgré son nom.
//!
//! Régime multi-vault LOCAL au test : les notes de plusieurs vaults coexistent dans la
//! même base SQLite de test. En prod le flag `multi_tenant.enabled` reste OFF (chemin
//! `promote_once` mono-vault, byte-identical).
//!
//! Stratégie d'isolation (identique à `review_promote.rs`) :
//! - `Vault::create` (TempDir) = backend vault réel avec son `SqliteIndex` interne.
//! - notes « âgées » simulées via `now_ms = real_now + 20j` (cutoff 14j) sans bypasser l'index.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::index::Index;
use gradatum_core::scope::{AclCheckedVaultId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_server::{
    config::{PerVaultOverride, ReviewPromoteConfig, ServerConfig},
    metrics::AppMetrics,
    review_promote::promote_tick,
    state::VaultRegistry,
};
use gradatum_vault::{Registry, Vault};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harnais (dupliqué de review_promote.rs — les fichiers tests/ sont des crates
// séparées, pas de partage de helpers privés possible en Rust)
// ---------------------------------------------------------------------------

/// Vault `main` réel (TempDir) + son index SQLite.
async fn build_vault() -> (Arc<Vault>, TempDir) {
    let dir = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    (vault, dir)
}

/// Config par défaut — age_days=14, enabled.
fn default_cfg() -> ReviewPromoteConfig {
    ReviewPromoteConfig {
        enabled: true,
        age_days: 14,
        interval_secs: 3600,
        max_per_tick: 200,
    }
}

/// Emballe une `ReviewPromoteConfig` dans une `ServerConfig` par défaut (L6 : `promote_tick`
/// prend la config complète ; `per_vault` vide ⇒ tout vault retombe sur la config globale).
fn server_cfg(rp: ReviewPromoteConfig) -> ServerConfig {
    ServerConfig {
        review_promote: rp,
        ..Default::default()
    }
}

/// `now_ms` = real_now + 20 jours → notes créées « maintenant » paraissent âgées de 20j
/// (≥ cutoff 14j) donc éligibles à la promotion.
fn now_plus_20_days() -> i64 {
    let real_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    real_now + 20 * 86_400_000
}

/// `Frontmatter` minimal valide (vault `main`).
fn minimal_frontmatter() -> Frontmatter {
    use chrono::Utc;
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Draft,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// Note réelle du vault `main` (fichier MD + index) amenée à `Staging` via la state
/// machine. Requis pour un chemin d'ÉCRITURE Vault (`update_note_status`).
async fn seed_real_main_staging_note(vault: &Vault) -> String {
    let note_id = NoteId::new();
    let note_id_str = note_id.0.to_string();
    vault
        .write_note_with_id(
            minimal_frontmatter(),
            format!("# test {note_id_str}\n\ncorps"),
            note_id,
        )
        .await
        .expect("write_note_with_id");
    vault
        .update_status(note_id, NoteStatus::PendingReview, None)
        .await
        .expect("Draft→PendingReview");
    vault
        .update_status(note_id, NoteStatus::Staging, None)
        .await
        .expect("PendingReview→Staging");
    note_id_str
}

/// Note seedée DIRECTEMENT dans l'index (statut `staging`), rattachée à `vault_id`,
/// SANS fichier markdown. Suffisant pour tester les requêtes de LECTURE de l'index
/// (`find_promotable_in_vault`). ULID généré → toujours valide (Crockford base32).
async fn seed_index_staging_note(vault: &Vault, vault_id: &str) -> String {
    let note_id = NoteId::new();
    let id = note_id.0.to_string();
    let idx = vault.index();
    idx.seed_note_with_fts_vault(&id, vault_id, "reference", None, "corps test")
        .await
        .expect("seed note index");
    idx.patch_note_status(
        &AclCheckedVaultId::for_system_task(VaultId::new(vault_id)),
        &note_id,
        Some("staging"),
        None,
        None,
    )
    .await
    .expect("patch staging");
    id
}

// ---------------------------------------------------------------------------
// Test 1 — OFF : chemin mono-vault legacy (byte-identical à promote_once)
// ---------------------------------------------------------------------------

/// À `multi_tenant_enabled = false`, `promote_tick` délègue à `promote_once` : la note
/// staging âgée du vault mono est promue en `Live`, sans passer par la boucle per-vault.
#[tokio::test]
async fn promote_tick_off_promotes_mono_vault() {
    let (vault, _dir) = build_vault().await;
    let metrics = AppMetrics::new();
    let cfg = default_cfg();

    let note_id_str = seed_real_main_staging_note(&vault).await;

    let index_arc = Arc::clone(vault.index()) as Arc<dyn Index>;
    let vault_arc = Arc::clone(&vault) as Arc<dyn Registry>;
    // OFF : registre non consulté (délégation `promote_once`) — singleton `main` requis
    // par la signature.
    let vaults = Arc::new(VaultRegistry::singleton(Arc::clone(&vault)));

    let stats = promote_tick(
        &index_arc,
        &vault_arc,
        &vaults,
        &metrics,
        &server_cfg(cfg),
        now_plus_20_days(),
        false,
    )
    .await;

    assert_eq!(stats.staging, 1, "OFF : la note staging âgée est promue");
    assert_eq!(stats.errors, 0, "OFF : aucun échec");
    let note = vault
        .read_note_by_id(&note_id_str)
        .await
        .expect("read_note_by_id");
    assert_eq!(note.frontmatter.status, NoteStatus::Live);
}

// ---------------------------------------------------------------------------
// Test 2 — INV-JOB-SCOPE : find_promotable_in_vault scopé par VaultId (typé)
// ---------------------------------------------------------------------------

/// Cœur du verrou : un job scopé à `vault-b` ne voit JAMAIS les notes de `main`, et
/// réciproquement. `find_promotable_in_vault` est appelé via `Arc<dyn Index>` (chemin
/// trait) avec un `&VaultId` — compile-fail tant que le trait prenait `&str`.
#[tokio::test]
async fn find_promotable_in_vault_is_scoped_by_typed_vaultid() {
    let (vault, _dir) = build_vault().await;

    let id_main = seed_index_staging_note(&vault, "main").await;
    let id_b = seed_index_staging_note(&vault, "vault-b").await;
    assert_ne!(id_main, id_b);

    let index_arc = Arc::clone(vault.index()) as Arc<dyn Index>;
    let now_ms = now_plus_20_days();
    let cutoff = now_ms - 14 * 86_400_000;

    // Job scopé à `vault-b` → uniquement la note de vault-b.
    let promotable_b = index_arc
        .find_promotable_in_vault(&VaultId::new("vault-b"), cutoff, 100)
        .await
        .expect("find_promotable_in_vault vault-b");
    let ids_b: Vec<&str> = promotable_b.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids_b,
        vec![id_b.as_str()],
        "INV-JOB-SCOPE : le job vault-b ne voit QUE la note de vault-b, jamais celle de main"
    );

    // Job scopé à `main` → uniquement la note de main.
    let promotable_main = index_arc
        .find_promotable_in_vault(&VaultId::new("main"), cutoff, 100)
        .await
        .expect("find_promotable_in_vault main");
    let ids_main: Vec<&str> = promotable_main.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids_main,
        vec![id_main.as_str()],
        "INV-JOB-SCOPE : le job main ne voit QUE la note de main, jamais celle de vault-b"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — typage du retour list_active_vaults : Vec<VaultId>
// ---------------------------------------------------------------------------

/// `list_active_vaults` retourne des `VaultId` typés (compile-fail tant que `Vec<String>`).
/// `main` est actif par défaut (migration seed 0030). Vérifié via le chemin trait.
#[tokio::test]
async fn list_active_vaults_returns_typed_vaultid() {
    let (vault, _dir) = build_vault().await;
    let index_arc = Arc::clone(vault.index()) as Arc<dyn Index>;

    let vaults: Vec<VaultId> = index_arc
        .list_active_vaults()
        .await
        .expect("list_active_vaults");

    assert!(
        vaults.contains(&VaultId::new("main")),
        "le vault main (seed 0030) est actif : {vaults:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 4 (A1) — ON : le write est routé vers le handle du vault CIBLE via resolve()
// ---------------------------------------------------------------------------

/// `Frontmatter` minimal ciblant `vault_id` (variante multi-vault de `minimal_frontmatter`).
fn frontmatter_for(vault_id: &str) -> Frontmatter {
    let mut fm = minimal_frontmatter();
    fm.vault_id = VaultId::new(vault_id);
    fm
}

/// Note RÉELLE (MD + index) amenée à `Staging` dans `vault`, ciblant `vault_id` — requise
/// pour exercer le chemin d'ÉCRITURE Vault (`update_note_status` via `promote_batch`).
async fn seed_real_staging_note(vault: &Vault, vault_id: &str) -> String {
    let note_id = NoteId::new();
    let note_id_str = note_id.0.to_string();
    vault
        .write_note_with_id(
            frontmatter_for(vault_id),
            format!("# test {note_id_str}\n\ncorps"),
            note_id,
        )
        .await
        .expect("write_note_with_id");
    vault
        .update_status(note_id, NoteStatus::PendingReview, None)
        .await
        .expect("Draft→PendingReview");
    vault
        .update_status(note_id, NoteStatus::Staging, None)
        .await
        .expect("PendingReview→Staging");
    note_id_str
}

/// A1 (caveat pré-flip) : à flag ON, une note promouvable de `vault-b` DOIT être promue
/// DANS `vault-b`. Avant le fix, `promote_tick` passait le singleton `main` à
/// `promote_batch` ; `update_note_status(témoin=vault-b)` sur le handle `main` était rejeté
/// par `ensure_witness_owns_vault` (`NoteNotFound`) → `errors=1`, note jamais promue. Le
/// routage `state.vaults.resolve(&vault_id)` sert le handle `vault-b` → promotion effective.
#[tokio::test]
async fn promote_tick_promotes_in_correct_vault() {
    // 2 vaults réels adossés au MÊME index (un seul pool ; partition par colonne vault_id).
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().join("vault");
    let vault_main = Arc::new(
        Vault::create(&root, VaultId::new("main"))
            .await
            .expect("Vault::create main"),
    );
    let shared_index = Arc::clone(vault_main.index());
    let vault_b = Arc::new(
        Vault::with_shared_index(&root, VaultId::new("vault-b"), Arc::clone(&shared_index))
            .await
            .expect("Vault::with_shared_index vault-b"),
    );

    // `vault-b` doit être un tenant ACTIF pour que la boucle ON le traite (list_active_vaults
    // lit `tenants WHERE status='active'`). `main` l'est déjà (seed migration 0030).
    shared_index
        .provision_vault("vault-b")
        .await
        .expect("provision_vault vault-b");

    // Note promouvable RÉELLE dans vault-b (chemin d'écriture Vault).
    let note_id_str = seed_real_staging_note(&vault_b, "vault-b").await;

    // Registre 2-vaults — le choke-point de routage (`resolve`).
    let registry = VaultRegistry::new();
    registry
        .insert(VaultId::new("main"), Arc::clone(&vault_main))
        .expect("insert main");
    registry
        .insert(VaultId::new("vault-b"), Arc::clone(&vault_b))
        .expect("insert vault-b");
    let vaults = Arc::new(registry);

    let metrics = AppMetrics::new();
    let cfg = default_cfg();
    let index_arc = Arc::clone(&shared_index) as Arc<dyn Index>;
    // Singleton `main` (chemin OFF) — non emprunté à ON, mais requis par la signature.
    let vault_singleton = Arc::clone(&vault_main) as Arc<dyn Registry>;

    let stats = promote_tick(
        &index_arc,
        &vault_singleton,
        &vaults,
        &metrics,
        &server_cfg(cfg),
        now_plus_20_days(),
        true,
    )
    .await;

    assert_eq!(
        stats.staging, 1,
        "ON : la note staging de vault-b est promue (write routé vers le bon handle)"
    );
    assert_eq!(
        stats.errors, 0,
        "ON : aucun NoteNotFound — le write vise vault-b, jamais le singleton main"
    );
    let note = vault_b
        .read_note_by_id(&note_id_str)
        .await
        .expect("read_note_by_id vault-b");
    assert_eq!(
        note.frontmatter.status,
        NoteStatus::Live,
        "la note de vault-b doit être Live après promotion"
    );
}

// ---------------------------------------------------------------------------
// Test 5 (C2b, post-mortem L6) — ON : un override review_promote `enabled=false`
// per-vault SAUTE ce vault (symétrie du fix salience C1). Verrou de couverture.
// ---------------------------------------------------------------------------

/// `ServerConfig` : review_promote GLOBALE active, mais `vault-b` désactivé via override A6.
fn server_cfg_disable_vault_b() -> ServerConfig {
    let mut per_vault = std::collections::HashMap::new();
    per_vault.insert(
        "vault-b".to_string(),
        PerVaultOverride {
            salience: None,
            // `enabled=false` ⇒ ce vault doit être sauté par la boucle (`continue`).
            review_promote: Some(ReviewPromoteConfig {
                enabled: false,
                ..default_cfg()
            }),
        },
    );
    ServerConfig {
        review_promote: default_cfg(),
        per_vault,
        ..Default::default()
    }
}

/// C2(b) : à flag ON, deux vaults actifs (`main`, `vault-b`) portent chacun une note
/// promouvable âgée. `vault-b` a un override `review_promote { enabled = false }`. La boucle
/// per-vault DOIT promouvoir `main` (config effective = globale active) et SAUTER `vault-b`
/// (`cfg_eff.enabled == false` ⇒ `continue`) : sa note reste `Staging`. Preuve que la
/// désactivation per-vault est honorée — le pendant symétrique du footgun salience C1.
#[tokio::test]
async fn promote_tick_honors_per_vault_review_promote_disable() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().join("vault");
    let vault_main = Arc::new(
        Vault::create(&root, VaultId::new("main"))
            .await
            .expect("Vault::create main"),
    );
    let shared_index = Arc::clone(vault_main.index());
    let vault_b = Arc::new(
        Vault::with_shared_index(&root, VaultId::new("vault-b"), Arc::clone(&shared_index))
            .await
            .expect("Vault::with_shared_index vault-b"),
    );
    shared_index
        .provision_vault("vault-b")
        .await
        .expect("provision_vault vault-b");

    // Note promouvable RÉELLE (MD + index) dans CHAQUE vault.
    let note_main = seed_real_staging_note(&vault_main, "main").await;
    let note_b = seed_real_staging_note(&vault_b, "vault-b").await;

    let registry = VaultRegistry::new();
    registry
        .insert(VaultId::new("main"), Arc::clone(&vault_main))
        .expect("insert main");
    registry
        .insert(VaultId::new("vault-b"), Arc::clone(&vault_b))
        .expect("insert vault-b");
    let vaults = Arc::new(registry);

    let metrics = AppMetrics::new();
    let index_arc = Arc::clone(&shared_index) as Arc<dyn Index>;
    let vault_singleton = Arc::clone(&vault_main) as Arc<dyn Registry>;

    let stats = promote_tick(
        &index_arc,
        &vault_singleton,
        &vaults,
        &metrics,
        &server_cfg_disable_vault_b(),
        now_plus_20_days(),
        true,
    )
    .await;

    // main : config effective = globale active ⇒ promu.
    assert_eq!(
        stats.staging, 1,
        "C2(b) : seule la note de main est promue (vault-b sauté)"
    );
    assert_eq!(stats.errors, 0, "C2(b) : aucun échec");
    let m = vault_main
        .read_note_by_id(&note_main)
        .await
        .expect("read main");
    assert_eq!(
        m.frontmatter.status,
        NoteStatus::Live,
        "main promu en Live (override globale active)"
    );

    // vault-b : override enabled=false ⇒ SAUTÉ ⇒ note toujours Staging.
    let b = vault_b
        .read_note_by_id(&note_b)
        .await
        .expect("read vault-b");
    assert_eq!(
        b.frontmatter.status,
        NoteStatus::Staging,
        "C2(b) : vault-b (review_promote enabled=false) est sauté — note NON promue"
    );
}
