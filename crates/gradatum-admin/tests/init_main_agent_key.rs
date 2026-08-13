//! Tâche 6 (v2.0.0, R4) — `init` frappe la clé `main-agent`.
//!
//! Doctrine : la clé `main-agent` est obligatoire à l'installation. Une racine
//! initialisée mais inutilisable faute de clé doit être impossible.
//!
//! Ces tests pilotent le binaire compilé (`CARGO_BIN_EXE_gradatum-admin`),
//! cohérent avec `init_clean.rs` / `init_existing.rs`, et inspectent l'état
//! résultant via `rusqlite` sur `db/api_keys.sqlite`.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Identité d'amorçage exigée par R4.
const BOOTSTRAP_IDENTITY: &str = "main-agent";

/// Lance `gradatum-admin init` sur `root` avec le preset donné, en mode
/// non-interactif par défaut, et renvoie le `Output` (statut + stdout + stderr).
fn run_init(root: &Path, preset: &str, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gradatum-admin"));
    cmd.arg("init")
        .arg("--preset")
        .arg(preset)
        .arg("--root")
        .arg(root)
        .current_dir(std::env::temp_dir());
    for a in extra {
        cmd.arg(a);
    }
    cmd.output()
        .expect("échec du lancement de gradatum-admin init")
}

/// Compte les clés actives (non révoquées) portées par `owner` dans le registre.
fn count_active_keys(root: &Path, owner: &str) -> i64 {
    let conn = rusqlite::Connection::open(root.join("db/api_keys.sqlite"))
        .expect("ouverture de db/api_keys.sqlite");
    conn.query_row(
        "SELECT COUNT(*) FROM api_keys WHERE owner = ?1 AND revoked_at IS NULL",
        [owner],
        |r| r.get::<_, i64>(0),
    )
    .expect("requête de comptage des clés actives")
}

/// Récupère `scopes_json` de l'unique clé active de `owner`.
fn active_key_scopes_json(root: &Path, owner: &str) -> String {
    let conn = rusqlite::Connection::open(root.join("db/api_keys.sqlite"))
        .expect("ouverture de db/api_keys.sqlite");
    conn.query_row(
        "SELECT scopes_json FROM api_keys WHERE owner = ?1 AND revoked_at IS NULL",
        [owner],
        |r| r.get::<_, String>(0),
    )
    .expect("lecture des scopes de la clé active")
}

/// (a) `init` sur une racine neuve → une clé active dont l'owner est `main-agent` existe.
#[test]
fn init_mints_active_main_agent_key() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let out = run_init(root, "hierarchical", &["--non-interactive"]);
    assert!(
        out.status.success(),
        "init a échoué : stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        count_active_keys(root, BOOTSTRAP_IDENTITY),
        1,
        "init doit frapper exactement une clé active pour '{BOOTSTRAP_IDENTITY}'"
    );

    // La clé doit porter le scope d'écriture canonique du porteur main-agent.
    let scopes = active_key_scopes_json(root, BOOTSTRAP_IDENTITY);
    for expected in ["vault_read", "vault_search", "vault_write", "write"] {
        assert!(
            scopes.contains(expected),
            "scopes de la clé main-agent = {scopes}, doit contenir '{expected}'"
        );
    }
}

/// (b) Le secret est écrit en `0600`, affiché **une seule fois**, et **muet** en
/// `--non-interactive` (jamais journalisé, jamais sur stdout/stderr).
#[test]
fn init_secret_file_0600_shown_once_and_silent_when_non_interactive() {
    // --- Volet non-interactif : muet ---
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let out = run_init(root, "hierarchical", &["--non-interactive"]);
    assert!(out.status.success(), "init non-interactif a échoué");

    let secret_path = root.join("config/main-agent.apikey.txt");
    assert!(
        secret_path.is_file(),
        "le secret main-agent doit être écrit dans {}",
        secret_path.display()
    );
    let mode = secret_path.metadata().unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "secret main-agent chmod = {mode:o}, attendu 0600"
    );

    let secret = std::fs::read_to_string(&secret_path).unwrap();
    let secret = secret.trim();
    assert!(
        secret.starts_with("ak_"),
        "le secret enregistré doit être une clé API (préfixe ak_), trouvé : {secret:.6}…"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains(secret) && !stderr.contains(secret),
        "en --non-interactive le secret ne doit JAMAIS apparaître sur stdout/stderr"
    );
    // Filet secondaire : aucun jeton de clé (ak_) ne doit fuiter.
    assert!(
        !stdout.contains("ak_") && !stderr.contains("ak_"),
        "en --non-interactive aucun jeton 'ak_' ne doit apparaître sur stdout/stderr"
    );

    // --- Volet interactif : affiché exactement une fois ---
    let tmp2 = TempDir::new().unwrap();
    let root2 = tmp2.path();
    let out2 = run_init(root2, "hierarchical", &[]);
    assert!(out2.status.success(), "init interactif a échoué");

    let secret2 = std::fs::read_to_string(root2.join("config/main-agent.apikey.txt")).unwrap();
    let secret2 = secret2.trim();
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert_eq!(
        stdout2.matches(secret2).count(),
        1,
        "en interactif le secret main-agent doit être affiché exactement une fois"
    );
}

/// (c) `init --force` sur une racine déjà pourvue → ne crée PAS une seconde clé
/// `main-agent` (croisement R1 : une identité = une seule clé active).
#[test]
fn init_force_does_not_create_second_main_agent_key() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let first = run_init(root, "hierarchical", &["--non-interactive"]);
    assert!(first.status.success(), "première init a échoué");
    assert_eq!(count_active_keys(root, BOOTSTRAP_IDENTITY), 1);

    let forced = run_init(root, "hierarchical", &["--non-interactive", "--force"]);
    assert!(
        forced.status.success(),
        "init --force a échoué : stderr={}",
        String::from_utf8_lossy(&forced.stderr)
    );

    assert_eq!(
        count_active_keys(root, BOOTSTRAP_IDENTITY),
        1,
        "init --force ne doit PAS frapper une seconde clé active main-agent (R1)"
    );
}

/// (d) Si la frappe échoue (identité non déclarée au preset), `init` échoue et ne
/// laisse pas de racine à moitié initialisée (aucun marqueur `admin.bearer.txt`,
/// donc l'installation reste réessayable sans `--force`).
#[test]
fn init_fails_when_bootstrap_identity_absent_and_leaves_no_half_root() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Preset custom valide mais SANS `main-agent`.
    let preset_dir = TempDir::new().unwrap();
    let preset_path = preset_dir.path().join("no-main-agent.toml");
    std::fs::write(
        &preset_path,
        r#"[[consumer]]
identity = "some-other-agent"
read_patterns = ["foo/**"]
write_patterns = ["foo/**"]
sees_personal_classified = false
"#,
    )
    .unwrap();

    let out = run_init(root, preset_path.to_str().unwrap(), &["--non-interactive"]);
    assert!(
        !out.status.success(),
        "init aurait dû échouer : le preset ne déclare pas '{BOOTSTRAP_IDENTITY}'"
    );

    // Aucune clé main-agent frappée.
    if root.join("db/api_keys.sqlite").exists() {
        assert_eq!(
            count_active_keys(root, BOOTSTRAP_IDENTITY),
            0,
            "aucune clé main-agent ne doit exister après un échec de frappe"
        );
    }

    // Pas de racine à moitié initialisée : le marqueur admin.bearer.txt est absent.
    assert!(
        !root.join("config/admin.bearer.txt").exists(),
        "aucun marqueur admin.bearer.txt ne doit subsister après échec (racine non initialisée)"
    );

    // Réessayable sans --force avec un preset correct.
    let retry = run_init(root, "hierarchical", &["--non-interactive"]);
    assert!(
        retry.status.success(),
        "après un échec, une init correcte doit réussir sans --force : stderr={}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(count_active_keys(root, BOOTSTRAP_IDENTITY), 1);
}

/// (e) `init --preset flat` aboutit et frappe la clé `main-agent`.
///
/// Régression : le preset `flat` est `include_str!`'é dans le binaire public et se
/// présente comme l'onboarding le plus rapide pour l'évaluation OSS — le premier
/// contact d'un installateur tiers. Il doit satisfaire la même règle R4 que
/// `hierarchical` : déclarer l'identité d'amorçage `main-agent`, sinon
/// `init --preset flat` échoue sur sa propre garde et la racine reste non initialisée.
#[test]
fn init_flat_preset_mints_main_agent_key() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let out = run_init(root, "flat", &["--non-interactive"]);
    assert!(
        out.status.success(),
        "init --preset flat a échoué : stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        count_active_keys(root, BOOTSTRAP_IDENTITY),
        1,
        "init --preset flat doit frapper exactement une clé active pour '{BOOTSTRAP_IDENTITY}'"
    );
}
