//! Registre de handles multi-vault `Map<VaultId, Vault>` avec index PARTAGÉ.
//!
//! Design cible du routage multi-vault (council `01KXWMCR0N`) : un `AppState` détient un
//! registre de handles `Vault`, tous adossés au **même** `Arc<SqliteIndex>` (un seul pool
//! sur `index.db`, partition par la colonne `vault_id` — PK composite `(vault_id, id)`). À
//! flag `multi_tenant` OFF, le registre LIVE contient EXACTEMENT `{main}` (byte-identical) ;
//! le 2e vault n'existe QUE dans ce harnais (jamais LIVE).
//!
//! Ce test prouve quatre invariants indissociables :
//!   1. **Handle index partagé** — `Vault::with_shared_index` réutilise le pool passé
//!      (`Arc::ptr_eq`), il n'ouvre PAS un 2e pool sur la même DB.
//!   2. **Isolation** — une écriture sur `vault-b` est invisible depuis `main` (scoping
//!      `self.vault_id` du read + colonne `vault_id` de l'index).
//!   3. **Gate C4-1e préservée** — `ensure_witness_owns_vault` : une mutation sur `vault-b`
//!      avec un témoin `main` est refusée (`NoteNotFound`) AVANT toute mutation, alors que
//!      le témoin `vault-b` la laisse aboutir.
//!   4. **Fail-closed provisioning** — insérer dans le registre un `Vault`
//!      dont le `vault_id` réel diverge de la clé attendue est REFUSÉ (mismatch config
//!      silencieux = ré-ouverture de la classe cross-vault).
//!
//! Le régime multi-vault est purement local au harnais : `multi_tenant.enabled` reste OFF.

use std::sync::Arc;

use chrono::Utc;
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::{AclCheckedVaultId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_server::state::{VaultRegistry, VaultRegistryError};
use gradatum_vault::{Registry, Vault};
use tempfile::TempDir;

#[path = "helpers/mod.rs"]
mod helpers;

/// Construit un `Frontmatter` minimal ciblant le vault `vault_id`.
fn frontmatter_for(vault_id: &str) -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault_id),
        locus: None,
        section: Section::Decisions,
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

/// Deux `Vault` (`main`, `vault-b`) adossés au MÊME pool `SqliteIndex`, chacun isolé,
/// gate témoin préservée.
#[tokio::test]
async fn two_vaults_share_index_stay_isolated_and_preserve_witness_gate() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().join("vault");

    // `main` ouvre le pool ; `vault-b` le RÉUTILISE (handle partagé).
    let vault_main = Arc::new(
        Vault::create(&root, VaultId::new("main"))
            .await
            .expect("Vault::create main"),
    );
    let shared_index = Arc::clone(vault_main.index());
    let vault_b = Arc::new(
        Vault::with_shared_index(
            &root,
            VaultId::parse("vault-b").expect("vault-b est un VaultId valide"),
            Arc::clone(&shared_index),
        )
        .await
        .expect("Vault::with_shared_index vault-b"),
    );

    // (1) Handle index PARTAGÉ : un seul pool, pas deux.
    assert!(
        Arc::ptr_eq(vault_main.index(), vault_b.index()),
        "les deux handles DOIVENT partager le même Arc<SqliteIndex> (un seul pool sur index.db)"
    );

    // Écriture d'une note appartenant à `vault-b`.
    let id = NoteId::new();
    vault_b
        .write_note_with_id(frontmatter_for("vault-b"), "# Note B\n\ncorps".into(), id)
        .await
        .expect("write_note_with_id vault-b");
    let id_str = id.0.to_string();

    // (2) Isolation : la note de `vault-b` est invisible depuis `main`.
    let seen_from_main = vault_main.read_note_by_id(&id_str).await;
    assert!(
        matches!(seen_from_main, Err(GradatumError::NoteNotFound(nid)) if nid == id),
        "la note de vault-b ne doit JAMAIS être lisible depuis main, obtenu : {seen_from_main:?}"
    );
    vault_b
        .read_note_by_id(&id_str)
        .await
        .expect("vault-b doit lire sa propre note");

    // (3) Gate C4-1e : une mutation sur `vault-b` avec témoin `main` est refusée AVANT
    // toute mutation (le témoin porte le vault CIBLE dérivé du JWT). On appelle la méthode
    // `Registry::add_tags` (chemin gardé par `ensure_witness_owns_vault`) en UFCS — l'inhérent
    // `Vault::add_tags` (sans témoin) la masquerait sinon par résolution de méthode.
    let witness_main = AclCheckedVaultId::attest_write_checked(VaultId::new("main"));
    let denied = Registry::add_tags(
        vault_b.as_ref(),
        &witness_main,
        &id_str,
        &["hijack".to_string()],
    )
    .await;
    assert!(
        matches!(denied, Err(GradatumError::NoteNotFound(nid)) if nid == id),
        "le témoin `main` ≠ vault servi `vault-b` doit être refusé (NoteNotFound), obtenu : {denied:?}"
    );
    // Le bon témoin laisse la mutation aboutir (le gate n'a pas cassé le chemin nominal).
    let witness_b = AclCheckedVaultId::attest_write_checked(VaultId::new("vault-b"));
    Registry::add_tags(vault_b.as_ref(), &witness_b, &id_str, &["ok".to_string()])
        .await
        .expect("mutation vault-b avec témoin vault-b doit aboutir");
}

/// Le harnais `spawn_two_vault_state` produit un `AppState` dont le
/// registre résout LES DEUX vaults (`main` + `vault-b`). C'est le choke-point de routage
/// (`state.vaults.resolve()`) que tous les tests ON W1-W3 consommeront. `resolve` est une
/// simple résolution BTreeMap (aucun I/O disque), fail-closed `VaultNotFound` sur absence.
#[tokio::test]
async fn two_vault_state_resolves_both() {
    let state = helpers::spawn_two_vault_state().await;
    assert!(
        state.vaults.resolve(&VaultId::new("main")).is_ok(),
        "le vault `main` doit être résoluble dans l'état 2-vaults"
    );
    assert!(
        state.vaults.resolve(&VaultId::new("vault-b")).is_ok(),
        "le vault `vault-b` doit être résoluble dans l'état 2-vaults"
    );
    assert!(
        state.vaults.resolve(&VaultId::new("vault-absent")).is_err(),
        "un vault non enregistré doit rester fail-closed (VaultNotFound)"
    );
}

/// Le registre indexe les handles par `VaultId` et sert le handle du vault demandé.
#[tokio::test]
async fn registry_indexes_handles_by_vault_id() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().join("vault");

    let vault_main = Arc::new(Vault::create(&root, VaultId::new("main")).await.unwrap());
    let vault_b = Arc::new(
        Vault::with_shared_index(
            &root,
            VaultId::new("vault-b"),
            Arc::clone(vault_main.index()),
        )
        .await
        .unwrap(),
    );

    let reg = VaultRegistry::new();
    reg.insert(VaultId::new("main"), Arc::clone(&vault_main))
        .expect("insert main");
    reg.insert(VaultId::new("vault-b"), Arc::clone(&vault_b))
        .expect("insert vault-b");

    assert_eq!(reg.len(), 2);
    assert_eq!(
        reg.get(&VaultId::new("vault-b"))
            .expect("handle vault-b présent")
            .vault_id()
            .as_str(),
        "vault-b"
    );
    assert!(
        reg.get(&VaultId::new("absent")).is_none(),
        "un vault non enregistré doit retourner None"
    );
}

/// Fail-closed : insérer un `Vault` dont le `vault_id` réel diverge de la
/// clé attendue est REFUSÉ — aucun handle n'est enregistré au vault_id incohérent.
#[tokio::test]
async fn registry_insert_is_fail_closed_on_vault_id_mismatch() {
    let dir = TempDir::new().expect("tempdir");
    // Un vault dont l'identité réelle est `vault-z` (namespace figé au boot).
    let vault_z = Arc::new(
        Vault::create(&dir.path().join("vault"), VaultId::new("vault-z"))
            .await
            .unwrap(),
    );

    let reg = VaultRegistry::new();
    // Tentative d'insertion sous une clé MENSONGÈRE `vault-b` ≠ identité réelle `vault-z`.
    let err = reg
        .insert(VaultId::new("vault-b"), Arc::clone(&vault_z))
        .expect_err("insertion sous une clé divergente doit échouer (fail-closed)");
    assert!(
        matches!(
            err,
            VaultRegistryError::VaultIdMismatch { ref expected, ref actual }
                if expected.as_str() == "vault-b" && actual.as_str() == "vault-z"
        ),
        "l'erreur doit nommer la divergence attendu/réel, obtenu : {err:?}"
    );
    // Aucun handle ne doit avoir été enregistré au vault_id incohérent.
    assert!(
        reg.get(&VaultId::new("vault-b")).is_none(),
        "aucun handle ne doit être enregistré sous une clé divergente"
    );
    assert_eq!(reg.len(), 0, "le registre doit rester vide après un refus");
}
