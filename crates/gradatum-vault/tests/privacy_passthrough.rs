//! F-160 — la passe de détection au point d'insertion unique.
//!
//! ## Ce que ces tests exigent
//!
//! La frontière de détection est **consultée, jamais obéie** : en 2.4.0 le mode est
//! `report_only`, aucun octet de la note ne dépend du verdict. Le seul fait observable
//! depuis l'extérieur du crate est donc **combien de fois, et sous quelles conditions,
//! le détecteur est appelé**. C'est ce que ces tests mesurent, avec un espion monté par
//! `with_detector` — le seul chemin d'injection.
//!
//! Trois familles :
//!
//! - **Les gardes en dur** (`code-*`, section `identity`) : évaluées sur le vault
//!   **résolu**, jamais sur le frontmatter entrant. Un frontmatter à `vault_id` vide
//!   écrit dans un vault `code-*` doit être sauté — c'est le cas que la résolution
//!   tardive aurait laissé passer.
//! - **Le point d'insertion unique** : chaque chemin d'écriture atteignable depuis ce
//!   crate appelle le détecteur exactement une fois. Un write refusé par le
//!   compare-and-swap n'en consomme aucune : la passe appartient à l'écriture, pas à la
//!   tentative.
//! - **L'identité sous no-op** : le no-op n'ajoute rien. L'oracle est sémantique
//!   (`sha256_for_history`, qui écarte le seul champ volatil, `updated`) doublé d'une
//!   comparaison octet à octet du `.md` écrit, la ligne `updated:` neutralisée.

mod common;
use common::build_minimal_frontmatter;

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::pii::{
    Budget, DetectInput, Detection, NoopDetector, PiiDetectError, PiiDetector, PrivacyBudgets,
    Surface,
};
use gradatum_core::scope::{AclCheckedVaultId, VaultId};
use gradatum_core::section::Section;
use gradatum_vault::{Registry, Vault, WriteResult};
use tempfile::TempDir;

// ── Espion ───────────────────────────────────────────────────────────────────

/// Compte les appels et retient ce que chaque appel a reçu.
///
/// Le détecteur est synchrone et appelé depuis un thread bloquant : le `Mutex` n'est
/// jamais tenu au travers d'un `await`.
#[derive(Default)]
struct Spy {
    calls: AtomicUsize,
    seen: Mutex<Vec<(Surface, Budget)>>,
}

impl Spy {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Les couples `(surface, budget)` vus, dans l'ordre des appels.
    fn seen(&self) -> Vec<(Surface, Budget)> {
        self.seen.lock().expect("verrou non empoisonné").clone()
    }
}

impl PiiDetector for Spy {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .expect("verrou non empoisonné")
            .push((input.surface, input.budget));
        // Report-only : un détecteur qui ne trouve rien ne change rien.
        Ok(Detection::new(Vec::new(), 0))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn fm_for(vault: &str, section: Section) -> Frontmatter {
    let mut fm = build_minimal_frontmatter();
    fm.vault_id = VaultId::new(vault);
    fm.section = section;
    fm
}

/// Monte l'espion sur un vault neuf du namespace demandé.
async fn spy_vault(root: &Path, namespace: &str, spy: &Arc<Spy>, budgets: PrivacyBudgets) -> Vault {
    Vault::create(root, VaultId::new(namespace))
        .await
        .expect("création du vault de test")
        .with_detector(Arc::clone(spy) as Arc<dyn PiiDetector>, budgets)
}

/// Le `.md` tel qu'il est sur disque, la ligne `updated:` neutralisée.
///
/// `write_note_inner` estampille `updated = Utc::now()` à **chaque** écriture : deux
/// notes sémantiquement identiques diffèrent par cette seule ligne. La neutraliser est
/// la condition pour que « identique » veuille dire autre chose que « écrit au même
/// instant ».
fn md_without_updated_line(root: &Path, namespace: &str, id: NoteId) -> String {
    let path = root.join(namespace).join(format!("{id}.md"));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("lecture de {} : {e}", path.display()));
    raw.lines()
        .map(|line| {
            if line.starts_with("updated:") {
                "updated: <neutre>"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── (a) les gardes en dur ────────────────────────────────────────────────────

#[tokio::test]
async fn code_star_vault_never_calls_the_detector() {
    let dir = TempDir::new().unwrap();
    let spy = Arc::new(Spy::default());
    let vault = spy_vault(dir.path(), "code-demo", &spy, PrivacyBudgets::default()).await;

    let note = vault
        .write_note(
            fm_for("code-demo", Section::Decisions),
            "let token = std::env::var(\"TOKEN\");".into(),
        )
        .await
        .expect("l'écriture a lieu");

    assert!(
        dir.path()
            .join("code-demo")
            .join(format!("{}.md", note.id))
            .exists(),
        "l'écriture n'est pas refusée : elle est seulement non analysée"
    );
    assert_eq!(
        spy.calls(),
        0,
        "un namespace `code-*` n'est jamais soumis au détecteur"
    );
}

#[tokio::test]
async fn code_star_vault_with_empty_frontmatter_vault_id_is_still_skipped() {
    let dir = TempDir::new().unwrap();
    let spy = Arc::new(Spy::default());
    let vault = spy_vault(dir.path(), "code-demo", &spy, PrivacyBudgets::default()).await;

    // `vault_id` vide : la garde doit lire la valeur RÉSOLUE (celle du vault), sinon
    // l'écriture passe pour un namespace ordinaire et le `code-*` est contourné par un
    // champ laissé vide.
    let mut fm = fm_for("main", Section::Decisions);
    fm.vault_id = VaultId::new("");

    vault
        .write_note(fm, "let token = std::env::var(\"TOKEN\");".into())
        .await
        .expect("l'écriture a lieu");

    assert_eq!(
        spy.calls(),
        0,
        "la garde est évaluée sur le vault résolu, jamais sur le frontmatter entrant"
    );
}

#[tokio::test]
async fn identity_section_never_calls_the_detector() {
    let dir = TempDir::new().unwrap();
    let spy = Arc::new(Spy::default());
    let vault = spy_vault(dir.path(), "main", &spy, PrivacyBudgets::default()).await;

    vault
        .write_note(
            fm_for("main", Section::Identity),
            "INVARIANTS\n- INV-CANARY: today\n".into(),
        )
        .await
        .expect("l'écriture a lieu");

    assert_eq!(
        spy.calls(),
        0,
        "une âme d'agent n'est jamais soumise au détecteur, quel que soit le namespace"
    );
}

// ── (b) le point d'insertion unique ──────────────────────────────────────────

#[tokio::test]
async fn every_write_path_calls_the_detector_exactly_once() {
    let dir = TempDir::new().unwrap();
    let spy = Arc::new(Spy::default());
    let vault = spy_vault(dir.path(), "main", &spy, PrivacyBudgets::default()).await;

    // 1. `write_note` — création avec un ULID frais.
    vault
        .write_note(fm_for("main", Section::Decisions), "corps a".into())
        .await
        .expect("écriture a");
    assert_eq!(spy.calls(), 1, "`write_note` : une passe, pas zéro ni deux");

    // 2. `write_note_with_id` — l'ULID pré-alloué du worker.
    let id = NoteId::new();
    let v1 = vault
        .write_note_with_id(fm_for("main", Section::Decisions), "corps b".into(), id)
        .await
        .expect("écriture b");
    assert_eq!(spy.calls(), 2, "`write_note_with_id` : une passe");

    // 3. `write_if_match` — le compare-and-swap, hash courant fourni.
    let v2 = vault
        .write_if_match(
            fm_for("main", Section::Decisions),
            "corps b v2".into(),
            id,
            Some(v1.content_hash.0),
        )
        .await
        .expect("cas accepté");
    assert!(
        matches!(v2, WriteResult::Written { .. }),
        "le hash courant doit être accepté"
    );
    assert_eq!(spy.calls(), 3, "`write_if_match` : une passe");

    // 4. `history_restore` — la restauration CoW réécrit la note comme version courante.
    let versions = vault
        .history_versions(id)
        .await
        .expect("historique lisible");
    assert!(
        !versions.is_empty(),
        "la v2 a dû créer un snapshot (le corps a changé)"
    );
    let witness = AclCheckedVaultId::for_system_task(VaultId::new("main"));
    vault
        .history_restore(&witness, &id.to_string(), versions[0])
        .await
        .expect("restauration");
    assert_eq!(
        spy.calls(),
        4,
        "`history_restore` écrit via le même point d'insertion : une passe"
    );
}

#[tokio::test]
async fn cas_write_calls_the_detector_exactly_once() {
    let dir = TempDir::new().unwrap();
    let spy = Arc::new(Spy::default());
    let vault = spy_vault(dir.path(), "main", &spy, PrivacyBudgets::default()).await;

    let id = NoteId::new();
    let v1 = vault
        .write_note_with_id(fm_for("main", Section::Decisions), "corps v1".into(), id)
        .await
        .expect("écriture v1");
    assert_eq!(spy.calls(), 1);

    // Hash courant : la garde passe, l'écriture a lieu, la passe est consommée une fois.
    let accepted = vault
        .write_if_match(
            fm_for("main", Section::Decisions),
            "corps v2".into(),
            id,
            Some(v1.content_hash.0),
        )
        .await
        .expect("cas accepté");
    assert!(matches!(accepted, WriteResult::Written { .. }));
    assert_eq!(
        spy.calls(),
        2,
        "un compare-and-swap accepté consomme exactement une passe"
    );

    // Hash périmé : la note n'est PAS écrite — aucune passe ne doit être dépensée pour
    // une écriture qui n'a pas eu lieu.
    let refused = vault
        .write_if_match(
            fm_for("main", Section::Decisions),
            "corps v3".into(),
            id,
            Some([0u8; 32]),
        )
        .await
        .expect("le conflit n'est pas une erreur");
    assert!(
        matches!(refused, WriteResult::Conflict { .. }),
        "un hash périmé doit rendre un conflit"
    );
    assert_eq!(spy.calls(), 2, "un write refusé ne consomme aucune passe");
}

// ── (c) surface et budget sont dérivés, jamais fournis ───────────────────────

#[tokio::test]
async fn surface_is_derived_for_every_section() {
    let dir = TempDir::new().unwrap();
    let spy = Arc::new(Spy::default());
    let vault = spy_vault(dir.path(), "main", &spy, PrivacyBudgets::default()).await;

    for section in Section::ALL {
        if section == Section::Identity {
            // Garde en dur, mesurée par `identity_section_never_calls_the_detector`.
            continue;
        }
        let before = spy.calls();
        vault
            .write_note(fm_for("main", section), format!("corps {section:?}"))
            .await
            .unwrap_or_else(|e| panic!("écriture {section:?} : {e}"));
        assert_eq!(
            spy.calls(),
            before + 1,
            "section {section:?} : une passe, pas zéro ni deux"
        );
    }

    let expected: Vec<Surface> = Section::ALL
        .iter()
        .filter(|section| **section != Section::Identity)
        .map(|section| {
            if *section == Section::Messaging {
                Surface::Sync
            } else {
                Surface::Async
            }
        })
        .collect();
    let observed: Vec<Surface> = spy.seen().into_iter().map(|(surface, _)| surface).collect();
    assert_eq!(
        observed, expected,
        "la surface est dérivée de la section : Messaging en synchrone, le reste en asynchrone"
    );
}

#[tokio::test]
async fn budget_is_read_from_the_vault_never_from_the_caller() {
    let dir = TempDir::new().unwrap();
    let spy = Arc::new(Spy::default());
    let budgets = PrivacyBudgets::new(4_096, 512, 7_000, 2_000, 0, 20_000);
    let vault = spy_vault(dir.path(), "main", &spy, budgets).await;

    // Deux corps de tailles très différentes, même surface : l'allocation doit être
    // identique. Une allocation choisie par l'appelant varierait avec l'appel.
    vault
        .write_note(fm_for("main", Section::Decisions), "court".into())
        .await
        .expect("écriture courte");
    vault
        .write_note(fm_for("main", Section::Decisions), "x".repeat(50_000))
        .await
        .expect("écriture longue");
    vault
        .write_note(fm_for("main", Section::Messaging), "court".into())
        .await
        .expect("écriture synchrone");

    let seen = spy.seen();
    let async_calls: Vec<(Surface, Budget)> = seen
        .iter()
        .filter(|(surface, _)| *surface == Surface::Async)
        .copied()
        .collect();
    let sync_calls: Vec<(Surface, Budget)> = seen
        .iter()
        .filter(|(surface, _)| *surface == Surface::Sync)
        .copied()
        .collect();

    // Le corps de 50 000 octets dépasse le plafond de 4 096 : il est découpé, donc
    // soumis plusieurs fois. Les deux écritures asynchrones font donc plus de deux appels.
    assert!(
        async_calls.len() > 2,
        "un corps au-delà du plafond est découpé en fenêtres, jamais avalé d'un bloc \
         (observé {} appel(s) asynchrone(s))",
        async_calls.len()
    );
    assert!(
        async_calls
            .iter()
            .all(|call| *call == (Surface::Async, Budget::new(7_000, 4_096))),
        "asynchrone : le délai de la configuration et le plafond d'octets du vault, \
         identiques pour un corps court et pour un corps découpé — la taille du corps ne \
         choisit pas l'allocation. Observé : {async_calls:?}"
    );
    assert_eq!(
        sync_calls,
        vec![(Surface::Sync, Budget::new(2_000, 4_096))],
        "synchrone : base 2 000 ms, 0 ms/KiB, plafond d'octets inchangé"
    );
}

// ── (d) l'identité sous no-op ────────────────────────────────────────────────

#[tokio::test]
async fn fresh_note_under_noop_is_byte_identical_to_2_3_1() {
    let fm = fm_for("main", Section::Decisions);
    let body = "corps de la note".to_string();

    // La référence : un vault jamais passé par `with_detector`, donc exactement la
    // configuration que les trois constructeurs écrivent encore en dur.
    let reference_dir = TempDir::new().unwrap();
    let reference_vault = Vault::create(reference_dir.path(), VaultId::new("main"))
        .await
        .expect("vault de référence");
    let reference_note = reference_vault
        .write_note(fm.clone(), body.clone())
        .await
        .expect("écriture de référence");
    assert_eq!(
        reference_vault.vault_id().as_str(),
        "main",
        "la référence est bien le namespace attendu"
    );

    // La même écriture, frontière montée dans sa forme no-op (le défaut des
    // constructeurs, monté explicitement).
    let noop_dir = TempDir::new().unwrap();
    let noop_vault = Vault::create(noop_dir.path(), VaultId::new("main"))
        .await
        .expect("vault no-op")
        .with_detector(Arc::new(NoopDetector), PrivacyBudgets::default());
    let noop_note = noop_vault
        .write_note(fm, body)
        .await
        .expect("écriture no-op");

    // Oracle 1 — le contenu sémantique. `sha256_for_history` écarte `updated`, le seul
    // champ qui bouge à chaque écriture même à contenu identique.
    assert_eq!(
        gradatum_core::history::sha256_for_history(&reference_note),
        gradatum_core::history::sha256_for_history(&noop_note),
        "le no-op n'ajoute rien au contenu sémantique de la note"
    );

    // Oracle 2 — les octets réellement persistés, la ligne `updated:` neutralisée.
    let reference_md = md_without_updated_line(reference_dir.path(), "main", reference_note.id);
    let noop_md = md_without_updated_line(noop_dir.path(), "main", noop_note.id);
    assert_eq!(
        reference_md, noop_md,
        "à la ligne `updated:` près, le `.md` est identique à l'octet"
    );

    // La passe ne laisse aucune trace dans la note elle-même.
    assert!(
        !noop_md.contains("privacy") && !noop_md.contains("detect"),
        "aucun marqueur de la passe ne doit atterrir dans la note écrite"
    );
}

// ── (e) le chemin d'injection ────────────────────────────────────────────────

#[tokio::test]
async fn with_detector_is_the_only_injection_path() {
    let dir = TempDir::new().unwrap();
    let spy = Arc::new(Spy::default());

    // Un vault qui n'est PAS passé par `with_detector` n'est branché sur aucun
    // détecteur, quel que soit le nombre d'écritures : il n'existe pas de détecteur
    // ambiant, seulement un détecteur monté.
    let plain = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("vault par défaut");
    plain
        .write_note(fm_for("main", Section::Decisions), "corps".into())
        .await
        .expect("écriture");
    assert_eq!(
        spy.calls(),
        0,
        "aucun détecteur ambiant n'atteint un vault construit par défaut"
    );

    // Le même détecteur, monté explicitement, est consulté à l'écriture suivante.
    let instrumented = Vault::open(dir.path())
        .await
        .expect("réouverture")
        .with_detector(
            Arc::clone(&spy) as Arc<dyn PiiDetector>,
            PrivacyBudgets::default(),
        );
    instrumented
        .write_note(fm_for("main", Section::Decisions), "corps".into())
        .await
        .expect("écriture instrumentée");
    assert_eq!(
        spy.calls(),
        1,
        "`with_detector` est le seul chemin d'injection, et il est immédiatement actif"
    );
}
