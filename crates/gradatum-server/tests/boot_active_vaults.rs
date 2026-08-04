//! Bootstrap N vaults au boot, **gaté `multi_tenant.enabled`**.
//!
//! - **Flag ON** : `bootstrap_active_vaults` itère `list_active_vaults` et enregistre un
//!   handle réel pour chaque vault actif non encore présent (adossé au pool `index.db`
//!   partagé). Les vaults deviennent résolubles.
//! - **Flag OFF** (défaut LIVE) : no-op — le registre reste EXACTEMENT le singleton `{main}`
//!   câblé par `with_vault_path` (byte-identical).
//!
//! Fermeture du caveat ledger **L7** (F-122, decision `01KY828JP28NYSXB7G4ZXBMTR6`) :
//!
//! - **volet 1 — fail-closed** : un vault `active` non instanciable ABORT le boot ;
//! - **volet 2 — réconciliation** : un répertoire de vault sans ligne `tenants` produit un
//!   `warn` + la jauge `gradatum_vault_orphan_dirs`, sans jamais bloquer le boot ;
//! - **idempotence multi-boot** : deux démarrages successifs ne dupliquent ni ne remplacent
//!   rien ;
//! - **byte-identical à OFF** : aucun de ces chemins n'est atteignable flag OFF.

use std::sync::Arc;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::index::Index;
use gradatum_core::scope::VaultId;
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::{AppState, VaultRegistry};
use gradatum_vault::Vault;
use tempfile::TempDir;

/// `AppState` vault racine `main` réel (registre singleton + `shared_index` peuplés,
/// `state.search` = même pool `index.db`). `multi_tenant` selon `on`.
async fn build_state(on: bool) -> (AppState, TempDir) {
    let tmp = TempDir::new().expect("TempDir boot_active_vaults");
    let vault = Arc::new(
        Vault::create(&tmp.path().join("vault"), VaultId::new("main"))
            .await
            .expect("Vault::create main"),
    );
    let idx = vault.index().clone();

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str("").expect("preset ACL vide valide");
    let vault_registry: Arc<dyn gradatum_vault::Registry> = vault.clone();
    let mut app_state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(vault_registry)
        .with_server_config(ServerConfig {
            multi_tenant: MultiTenantConfig { enabled: on },
            ..ServerConfig::default()
        });
    app_state.search = Arc::clone(&idx) as Arc<dyn Index>;
    app_state.vaults = Arc::new(VaultRegistry::singleton(Arc::clone(&vault)));
    app_state.shared_index = Some(idx);
    (app_state, tmp)
}

/// Flag ON : un vault actif provisionné (index) est enregistré au boot → résoluble.
#[tokio::test]
async fn boot_registers_active_vaults_when_on() {
    let (state, _tmp) = build_state(true).await;
    // Provisionne un vault actif `vault-b` dans l'index (lignes tenant/grant only).
    state
        .search
        .provision_vault("vault-b")
        .await
        .expect("provision vault-b");
    // Avant bootstrap : vault-b non résoluble (fantôme index-only).
    assert!(state.vaults.resolve(&VaultId::new("vault-b")).is_err());

    state
        .bootstrap_active_vaults()
        .await
        .expect("bootstrap_active_vaults");

    assert!(
        state.vaults.resolve(&VaultId::new("main")).is_ok(),
        "main reste résoluble"
    );
    assert!(
        state.vaults.resolve(&VaultId::new("vault-b")).is_ok(),
        "vault-b actif doit être enregistré au boot ON"
    );
}

/// Flag OFF (byte-identical) : bootstrap = no-op, registre reste `{main}`.
#[tokio::test]
async fn boot_off_single_main() {
    let (state, _tmp) = build_state(false).await;
    // Même provisioning index — mais à OFF, le bootstrap ne doit RIEN enregistrer.
    state
        .search
        .provision_vault("vault-b")
        .await
        .expect("provision vault-b");

    state
        .bootstrap_active_vaults()
        .await
        .expect("bootstrap_active_vaults");

    assert_eq!(
        state.vaults.len(),
        1,
        "OFF : registre reste {{main}} (byte-identical)"
    );
    assert!(state.vaults.resolve(&VaultId::new("main")).is_ok());
    assert!(
        state.vaults.resolve(&VaultId::new("vault-b")).is_err(),
        "OFF : vault-b jamais enregistré"
    );
}

// ─── L7 volet 1 — fail-closed ────────────────────────────────────────────────

/// Un vault `active` non instanciable ABORT le boot (au lieu de warn + skip).
///
/// Le namespace md est rendu non créable en posant un **fichier** régulier là où le handle
/// doit matérialiser `<root>/vault-ko/` : l'I/O d'instanciation échoue, et cet échec doit
/// remonter jusqu'à l'appelant (`main` fait `?` dessus → le service ne démarre pas).
#[tokio::test]
async fn boot_aborts_when_active_vault_not_instantiable() {
    let (state, tmp) = build_state(true).await;
    state
        .search
        .provision_vault("vault-ko")
        .await
        .expect("provision vault-ko");
    // Collision : `<root>/vault-ko` existe déjà en tant que fichier → create_dir impossible.
    std::fs::write(
        tmp.path().join("vault").join("vault-ko"),
        b"pas un repertoire",
    )
    .expect("poser le fichier de collision");

    let err = state
        .bootstrap_active_vaults()
        .await
        .expect_err("fail-closed : le boot doit échouer, pas continuer");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("fail-closed") && msg.contains("vault-ko"),
        "l'erreur doit nommer le vault fautif et le mode fail-closed, obtenu : {msg}"
    );
    assert!(
        state.vaults.resolve(&VaultId::new("vault-ko")).is_err(),
        "aucun handle partiel ne doit rester enregistré"
    );
}

/// Flag OFF : le MÊME état (vault actif non instanciable) ne déclenche rien — la sortie
/// anticipée est atteinte avant toute I/O. Preuve que le chemin fail-closed est inatteignable
/// à OFF (byte-identical).
#[tokio::test]
async fn boot_off_ignores_broken_active_vault() {
    let (state, tmp) = build_state(false).await;
    state
        .search
        .provision_vault("vault-ko")
        .await
        .expect("provision vault-ko");
    std::fs::write(
        tmp.path().join("vault").join("vault-ko"),
        b"pas un repertoire",
    )
    .expect("poser le fichier de collision");

    state
        .bootstrap_active_vaults()
        .await
        .expect("OFF : aucun chemin fail-closed atteignable");

    assert_eq!(state.vaults.len(), 1, "OFF : registre reste {{main}}");
}

// ─── L7 — idempotence multi-boot ─────────────────────────────────────────────

/// Deux démarrages successifs sur le même état : le 2e ne duplique rien, ne remplace aucun
/// handle vivant, n'échoue pas.
#[tokio::test]
async fn boot_twice_is_idempotent() {
    let (state, _tmp) = build_state(true).await;
    state
        .search
        .provision_vault("vault-b")
        .await
        .expect("provision vault-b");

    state.bootstrap_active_vaults().await.expect("1er boot");
    let len_after_first = state.vaults.len();
    let handle_after_first = state
        .vaults
        .get(&VaultId::new("vault-b"))
        .expect("vault-b enregistré au 1er boot");

    state
        .bootstrap_active_vaults()
        .await
        .expect("2e boot (multi-boot) ne doit pas échouer");

    assert_eq!(
        state.vaults.len(),
        len_after_first,
        "2e boot : aucun handle supplémentaire"
    );
    let handle_after_second = state
        .vaults
        .get(&VaultId::new("vault-b"))
        .expect("vault-b toujours enregistré après le 2e boot");
    assert!(
        Arc::ptr_eq(&handle_after_first, &handle_after_second),
        "2e boot : le handle vivant ne doit pas être remplacé (ré-instanciation silencieuse)"
    );
}

// ─── L7 volet 2 — réconciliation disque → registre ───────────────────────────

/// Un répertoire de vault sans ligne `tenants` est compté comme orphelin — sans bloquer le
/// boot ni devenir résoluble.
#[tokio::test]
async fn boot_on_counts_orphan_dir_without_blocking() {
    let (state, tmp) = build_state(true).await;
    std::fs::create_dir(tmp.path().join("vault").join("ghost"))
        .expect("créer le répertoire orphelin");

    state
        .bootstrap_active_vaults()
        .await
        .expect("volet 2 ne bloque jamais le boot");

    assert_eq!(
        state.metrics.vault_orphan_dirs.get(),
        1,
        "un répertoire sans ligne tenants doit être compté"
    );
    assert!(
        state.vaults.resolve(&VaultId::new("ghost")).is_err(),
        "un orphelin n'est jamais rendu résoluble par la réconciliation"
    );
}

/// Un vault `suspended` a une ligne `tenants` : son répertoire n'est PAS un orphelin.
/// Sans cette distinction, chaque suspension produirait un faux positif permanent.
#[tokio::test]
async fn boot_on_does_not_count_suspended_vault_dir() {
    let (state, tmp) = build_state(true).await;
    state
        .search
        .provision_vault("vault-frozen")
        .await
        .expect("provision vault-frozen");
    state
        .search
        .set_tenant_status(
            "vault-frozen",
            gradatum_core::scope::TenantStatus::Suspended,
        )
        .await
        .expect("suspend vault-frozen");
    // Le répertoire md reste sur disque après la suspension.
    std::fs::create_dir(tmp.path().join("vault").join("vault-frozen"))
        .expect("créer le répertoire du vault suspendu");

    state.bootstrap_active_vaults().await.expect("boot");

    assert_eq!(
        state.metrics.vault_orphan_dirs.get(),
        0,
        "un vault suspendu est connu du registre — pas un orphelin"
    );
}

/// `.gradatum/` et les répertoires cachés ne sont jamais des namespaces de vault.
#[tokio::test]
async fn boot_on_ignores_hidden_dirs() {
    let (state, tmp) = build_state(true).await;
    std::fs::create_dir(tmp.path().join("vault").join(".archive")).expect("créer .archive");

    state.bootstrap_active_vaults().await.expect("boot");

    assert_eq!(
        state.metrics.vault_orphan_dirs.get(),
        0,
        ".gradatum/ et .archive/ ne sont pas des vaults"
    );
}

/// Flag OFF : aucun scan disque n'est fait — la jauge reste à sa valeur d'initialisation.
/// Preuve que le volet 2 est lui aussi inatteignable à OFF (byte-identical).
#[tokio::test]
async fn boot_off_never_scans_disk() {
    let (state, tmp) = build_state(false).await;
    std::fs::create_dir(tmp.path().join("vault").join("ghost"))
        .expect("créer le répertoire orphelin");

    state.bootstrap_active_vaults().await.expect("boot OFF");

    assert_eq!(
        state.metrics.vault_orphan_dirs.get(),
        0,
        "OFF : aucune réconciliation, donc aucune observation"
    );
}

// ─── L7 volet 2 — la métrique sort RÉELLEMENT du side-channel :19091 ─────────

/// Preuve d'observabilité de bout en bout : la jauge alimentée par le code de PRODUCTION
/// (`bootstrap_active_vaults`) est exposée par le listener de PRODUCTION
/// (`spawn_metrics_listener` → handler `/metrics`), scrapée en HTTP sur loopback.
///
/// Ce test existe pour interdire la classe « métrique `#[cfg(test)]`-only » : il ne lit
/// aucun champ interne, il lit le corps de la réponse `/metrics`.
#[tokio::test]
async fn orphan_gauge_is_exposed_on_metrics_endpoint() {
    let (state, tmp) = build_state(true).await;
    std::fs::create_dir(tmp.path().join("vault").join("ghost"))
        .expect("créer le répertoire orphelin");
    state.bootstrap_active_vaults().await.expect("boot");

    // Port éphémère : réservé puis relâché juste avant le bind du listener de prod.
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("réserver un port libre");
        probe.local_addr().expect("local_addr").port()
    };
    let bind: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    let metrics = state.metrics.clone();
    tokio::spawn(async move {
        let _ = gradatum_server::metrics::spawn_metrics_listener(bind, metrics).await;
    });

    let url = format!("http://127.0.0.1:{port}/metrics");
    let client = reqwest::Client::new();
    let mut body = String::new();
    for _ in 0..50 {
        if let Ok(resp) = client.get(&url).send().await {
            body = resp.text().await.expect("corps /metrics");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    assert!(
        body.contains("gradatum_vault_orphan_dirs"),
        "la série doit être exposée par le registre de production, corps obtenu : {body}"
    );
    assert!(
        body.contains("gradatum_vault_orphan_dirs 1"),
        "la valeur observée au boot doit être celle scrapée, corps obtenu : {body}"
    );
}
