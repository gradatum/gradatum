//! Isolation cross-vault du validator de fraîcheur moka (C4-1e, Slice C / C2).
//!
//! `Vault::read_note` valide un cache-hit via une closure moka qui appelle
//! `Index::get_content_hash` pour comparer le `ContentHash` courant au hash mémorisé.
//! Avant le durcissement, la closure appelait `get_content_hash(id)` (id-only) : avec
//! la clé primaire composite `(vault_id, id)` (migration 0032), une note homonyme d'un
//! AUTRE vault pouvait satisfaire la requête et renvoyer un hash étranger, provoquant
//! une invalidation de cache erronée (le hit valide était refusé → cache_hits non
//! incrémenté, re-lecture disque superflue). La closure est désormais scopée sur
//! `self.tenant_id`.
//!
//! - `read_note_cache_hit_scoped_to_own_vault` : régime multi-vault — une note
//!   homonyme dans `main` ne casse PAS le hit valide de `vault-b` (`cache_hits == 1`) ;
//! - `read_note_off_single_vault_cache_hit_unchanged` : régime mono-vault — le hit
//!   valide reste honoré à l'identique (byte-identical flag OFF).
//!
//! Le régime multi-vault est purement local au harnais de test ; aucune configuration
//! serveur n'est touchée.

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_vault::Vault;
use tempfile::TempDir;

/// Un cache-hit valide dans `vault-b` ne doit PAS être invalidé par l'existence d'une
/// note homonyme (même ULID) dans `main`.
///
/// Séquence : l'instance `Vault` est enracinée sur `vault-b`. Deux notes de MÊME ULID
/// mais de contenu distinct sont écrites — une dans `main`, une dans `vault-b`. Un
/// premier `read_note` peuple le cache avec le hash de `vault-b` (miss). Le second
/// `read_note` déclenche la closure validator : scopée, elle relit le hash de
/// `vault-b`, confirme le hit (`cache_hits == 1`) et renvoie le contenu de `vault-b`.
/// Sans scoping, la closure relit un hash de `main` (≠) → invalidation erronée →
/// `cache_hits == 0`.
#[tokio::test]
async fn read_note_cache_hit_scoped_to_own_vault() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("vault-b"))
        .await
        .unwrap();

    let id = NoteId::new();

    // Note homonyme dans `main` (contenu distinct) — présente uniquement pour créer la
    // collision d'ULID côté index ; jamais lue directement.
    let fm_main = build_minimal_frontmatter(); // vault_id = "main" par défaut
    vault
        .write_note_with_id(fm_main, "# Main\n\ncorps-main".into(), id)
        .await
        .expect("write note homonyme main");

    // Note du vault racine de l'instance.
    let mut fm_b = build_minimal_frontmatter();
    fm_b.vault_id = VaultId::new("vault-b");
    vault
        .write_note_with_id(fm_b, "# VaultB\n\ncorps-b".into(), id)
        .await
        .expect("write note vault-b");

    // 1er read : cache miss → peuple le cache avec le hash de vault-b.
    let r1 = vault.read_note(id).await.expect("read_note #1");
    assert_eq!(vault.cache_hits(), 0, "1er read doit être un cache miss");
    assert_eq!(
        r1.body.markdown.trim(),
        "# VaultB\n\ncorps-b",
        "le miss doit résoudre le contenu de vault-b (self.tenant_id)"
    );

    // 2e read : cache hit — la closure validator scopée relit le hash de vault-b et
    // confirme la fraîcheur malgré la note homonyme de main.
    let r2 = vault.read_note(id).await.expect("read_note #2");
    assert_eq!(
        vault.cache_hits(),
        1,
        "le hit valide de vault-b ne doit PAS être invalidé par la note homonyme de main"
    );
    assert_eq!(
        r2.body.markdown.trim(),
        "# VaultB\n\ncorps-b",
        "le hit doit renvoyer le contenu de vault-b, jamais celui de main"
    );
}

/// Régime mono-vault : le cache-hit valide reste honoré à l'identique (byte-identical).
///
/// Aucune note homonyme ; le validator relit le hash de la note et confirme le hit
/// (`cache_hits == 1`) — comportement inchangé par rapport à l'ancien contrat id-only.
#[tokio::test]
async fn read_note_off_single_vault_cache_hit_unchanged() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();
    let fm = build_minimal_frontmatter(); // vault_id = "main"
    vault
        .write_note_with_id(fm, "# Mono\n\ncorps-mono".into(), id)
        .await
        .expect("write note mono-vault");

    let _r1 = vault.read_note(id).await.expect("read_note #1");
    assert_eq!(vault.cache_hits(), 0, "1er read doit être un cache miss");

    let r2 = vault.read_note(id).await.expect("read_note #2");
    assert_eq!(
        vault.cache_hits(),
        1,
        "cache hit mono-vault honoré (byte-identical flag OFF)"
    );
    assert_eq!(
        r2.body.markdown.trim(),
        "# Mono\n\ncorps-mono",
        "le hit doit renvoyer le contenu inséré"
    );
}
