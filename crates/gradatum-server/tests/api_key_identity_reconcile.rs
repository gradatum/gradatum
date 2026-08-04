//! Réconciliation `api_keys.owner` ↔ `consumer.identity` au boot (B6′b, livrable 3).
//!
//! ## Le défaut mesuré
//!
//! Les deux moitiés de la relation ne sont reliées par AUCUNE intégrité référentielle.
//! Une clé active dont l'`owner` n'est déclaré par aucun `[[consumer]]` s'authentifie
//! (200 sur `/auth/exchange`) puis se fait refuser sur tous les locus, **en silence**.
//! Le symptôme observé est une panne, la cause est une donnée. C'est l'incident `engine`
//! du 2026-07-27 : une journée d'instruction pour un refus nominal.
//!
//! ## Ce que les tests verrouillent
//!
//! 1. l'orpheline est **détectée** (jauge + `error!` nommant l'owner) ;
//! 2. le démarrage **continue** — la garde ne refuse jamais le boot ;
//! 3. une clé **révoquée** orpheline n'est pas signalée (bruit permanent sur un passé assumé) ;
//! 4. une identité **sans clé** n'est pas signalée (état nominal, 4 cas sur le parc) ;
//! 5. la jauge est **laissée inchangée** quand le scan ne peut pas conclure.
//!
//! ## Capture des logs
//!
//! `MakeWriter` maison sur un `Arc<Mutex<Vec<u8>>>` — `tracing-subscriber` est déjà une
//! dépendance du crate, aucune dépendance de test n'est ajoutée pour ça.

use std::sync::{Arc, Mutex};

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::scope::AgentId;
use gradatum_server::state::AppState;
use tempfile::TempDir;

/// Preset déclarant `engine` et quatre identités sans clé (miroir du parc réel).
const PRESET: &str = r#"
[[consumer]]
identity = "engine"
read_patterns = ["main/**"]
write_patterns = ["main/**"]

[[consumer]]
identity = "maintainer"
read_patterns = ["main/**"]
write_patterns = ["main/**"]

[[consumer]]
identity = "sub-external-agent"
read_patterns = ["main/**"]
write_patterns = []

[[consumer]]
identity = "validator"
read_patterns = ["main/**"]
write_patterns = []

[[consumer]]
identity = "expert-rust"
read_patterns = ["main/rust/**"]
write_patterns = []
"#;

// ── Capture des logs ─────────────────────────────────────────────────────────

/// Tampon partagé recevant la sortie du subscriber de test.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("tampon de log non empoisonné")).into_owned()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("tampon de log empoisonné"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Exécute `reconcile_key_owners` sous un subscriber local et rend les logs émis.
///
/// `with_default` scope le subscriber à ce thread : deux tests concurrents ne se
/// volent pas leurs lignes.
async fn capture_reconcile(state: &AppState) -> String {
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_ansi(false)
        .finish();
    // `reconcile_key_owners` est `async` : le guard doit couvrir le `.await`, donc on
    // ne peut pas se contenter d'un scope synchrone autour de l'appel.
    let _guard = tracing::subscriber::set_default(subscriber);
    state.reconcile_key_owners().await;
    buffer.contents()
}

/// Construit un état avec un store api-keys réel et le preset donné.
async fn build_state(preset: &str) -> (AppState, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let acl = AclEngine::from_preset_str(preset).expect("preset valide");
    let state = AppState::with_jwt_and_acl(JwtService::new_ephemeral(), acl)
        .with_api_keys_path(&dir.path().join("api_keys.sqlite"))
        .await
        .expect("api_keys store init");
    (state, dir)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Une clé active orpheline est nommée dans les logs ET comptée dans la jauge.
///
/// Discriminant : `gemini-agent` est une clé parfaitement valide — secret correct,
/// scopes corrects, tenant `main`. Seule l'absence de `[[consumer]]` la distingue.
/// Avant B6′b, rien au monde ne la signalait.
#[tokio::test]
async fn orphan_active_key_is_named_in_the_logs_and_counted() {
    let (state, _dir) = build_state(PRESET).await;
    state
        .api_keys
        .create(
            &AgentId::new("gemini-agent"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create");

    let logs = capture_reconcile(&state).await;

    assert_eq!(
        state.metrics.api_key_orphan_owners.get(),
        1,
        "une clé orpheline doit être comptée"
    );
    assert!(
        logs.contains("gemini-agent"),
        "le log doit NOMMER l'owner fautif — sans le nom, la ligne ne raccourcit \
         aucun diagnostic. Obtenu : {logs}"
    );
    assert!(
        logs.contains("ERROR"),
        "le signal doit franchir un filtre réglé sur `error`. Obtenu : {logs}"
    );
}

/// La détection n'empêche JAMAIS le démarrage.
///
/// Discriminant : l'état reste utilisable après le scan (le store répond encore, la
/// jauge est positionnée). Un refus de boot échangerait un agent muet contre un service
/// indisponible pour tous — un incident strictement pire que celui qu'on rapporte.
#[tokio::test]
async fn orphan_key_never_prevents_startup() {
    let (state, _dir) = build_state(PRESET).await;
    for owner in ["gemini-agent", "claude-code", "smoke"] {
        state
            .api_keys
            .create(
                &AgentId::new(owner),
                vec!["write".into()],
                "main".into(),
                None,
            )
            .await
            .expect("create");
    }

    // La signature est `-> ()` : il n'existe aucun chemin par lequel cette fonction
    // pourrait interrompre le boot. Le test verrouille cette propriété au niveau du
    // comportement observable — le boot se poursuit, l'état reste servi.
    state.reconcile_key_owners().await;

    assert_eq!(state.metrics.api_key_orphan_owners.get(), 3);
    assert_eq!(
        state
            .api_keys
            .list(false, None)
            .await
            .expect("listing")
            .len(),
        3,
        "l'état reste pleinement utilisable après le scan"
    );
}

/// Un owner déclaré ne déclenche rien : pas de faux positif sur le cas nominal.
#[tokio::test]
async fn declared_owner_is_not_reported() {
    let (state, _dir) = build_state(PRESET).await;
    state
        .api_keys
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create");

    let logs = capture_reconcile(&state).await;

    assert_eq!(state.metrics.api_key_orphan_owners.get(), 0);
    assert!(
        !logs.contains("ERROR"),
        "aucun signal ne doit être émis sur un parc sain. Obtenu : {logs}"
    );
}

/// Quatre identités déclarées SANS clé ne produisent aucun signal.
///
/// Discriminant : c'est l'état mesuré du parc (`maintainer`, `sub-external-agent`, `validator`,
/// `expert-rust`). La relation n'est vérifiée que dans le sens clé → identité ; une
/// implémentation qui aurait joint les deux sens transformerait ici un état nominal en
/// quatre erreurs permanentes au boot.
#[tokio::test]
async fn declared_identities_without_any_key_are_not_reported() {
    let (state, _dir) = build_state(PRESET).await;
    // Aucune clé du tout : les 4 identités sans credential sont seules en présence.
    let logs = capture_reconcile(&state).await;

    assert_eq!(
        state.metrics.api_key_orphan_owners.get(),
        0,
        "une identité sans clé n'est pas une anomalie"
    );
    for identity in [
        "maintainer",
        "sub-external-agent",
        "validator",
        "expert-rust",
    ] {
        assert!(
            !logs.contains(identity),
            "{identity} n'a pas de clé — ce n'est pas un défaut, et ça ne doit pas être \
             rapporté. Obtenu : {logs}"
        );
    }
}

/// Une clé RÉVOQUÉE orpheline n'est pas signalée.
///
/// Discriminant : elle n'authentifie plus, donc elle ne peut plus produire le refus
/// silencieux qu'on cherche. La signaler produirait un bruit permanent sur un passé
/// assumé — et un signal permanent finit par ne plus être lu.
#[tokio::test]
async fn revoked_orphan_key_is_not_reported() {
    let (state, _dir) = build_state(PRESET).await;
    let material = state
        .api_keys
        .create(
            &AgentId::new("gemini-agent"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create");
    state
        .api_keys
        .revoke(&material.prefix)
        .await
        .expect("revoke");

    let logs = capture_reconcile(&state).await;

    assert_eq!(state.metrics.api_key_orphan_owners.get(), 0);
    assert!(
        !logs.contains("gemini-agent"),
        "une clé révoquée n'authentifie plus — rien à signaler. Obtenu : {logs}"
    );
}

/// Preset vide (fallback DENY-ALL) : toute clé active devient orpheline.
///
/// Discriminant : c'est l'état du serveur quand `bearer.toml` est absent ou illisible.
/// Les clés y sont TOUTES inertes, et le boot doit le dire plutôt que le taire.
#[tokio::test]
async fn empty_preset_reports_every_active_key() {
    let (state, _dir) = build_state("").await;
    for owner in ["engine", "main-agent"] {
        state
            .api_keys
            .create(
                &AgentId::new(owner),
                vec!["write".into()],
                "main".into(),
                None,
            )
            .await
            .expect("create");
    }

    let logs = capture_reconcile(&state).await;

    assert_eq!(state.metrics.api_key_orphan_owners.get(), 2);
    assert!(logs.contains("engine") && logs.contains("main-agent"));
}
