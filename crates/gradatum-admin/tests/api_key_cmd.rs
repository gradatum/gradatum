//! Tests d'intégration AUTH-T3 — sous-commande `gradatum-admin api-key`.
//!
//! Vérifie que :
//! - `api-key create` génère un secret valide et persisté
//! - `api-key list` retourne les clés créées
//! - `api-key revoke` révoque une clé (AlreadyRevoked sur second appel)
//! - `api-key rotate` révoque l'ancienne et génère une nouvelle atomiquement
//! - `api-key create` REFUSE (exit ≠ 0) un jeu de scopes sans droit d'écriture,
//!   sauf `--read-only` explicite
//!
//! Les tests de refus invoquent le VRAI binaire (`CARGO_BIN_EXE_gradatum-admin`)
//! et mesurent son code de sortie : la garde vit dans la commande, pas dans le
//! store — la tester au niveau du store ne prouverait rien sur le CLI livré.

use gradatum_acl_auth::{ApiKeyStore, SqliteApiKeyStore};
use gradatum_core::scope::AgentId;
use tempfile::TempDir;

/// Ouvre ou crée un store SQLite dans un TempDir.
async fn open_store(dir: &TempDir) -> SqliteApiKeyStore {
    let db_path = dir.path().join("api_keys.sqlite");
    SqliteApiKeyStore::init(&db_path)
        .await
        .expect("init store doit réussir")
}

/// Création d'une clé et vérification du secret retourné.
#[tokio::test]
async fn create_returns_valid_secret() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let material = store
        .create(
            &AgentId::new("mcp-stub"),
            vec!["vault_read".into()],
            "main".into(),
            Some("test key".into()),
        )
        .await
        .expect("create doit réussir");

    // Le secret doit commencer par le préfixe `ak_`.
    assert!(
        material.secret.starts_with("ak_"),
        "le secret doit commencer par 'ak_'"
    );
    // Le préfixe doit être cohérent avec le secret.
    assert_eq!(
        &material.secret[..material.prefix.len()],
        &material.prefix[..],
        "le préfixe doit être le début du secret"
    );
}

/// Vérification du secret après création — roundtrip create → verify.
#[tokio::test]
async fn create_verify_roundtrip() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let material = store
        .create(
            &AgentId::new("owner-1"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create");

    let key = store
        .verify(&material.secret)
        .await
        .expect("verify avec le bon secret doit réussir");

    assert_eq!(key.owner, "owner-1");
    assert_eq!(key.tenant_id, "main");
    assert!(!key.is_revoked());
}

/// Vérification avec mauvais secret → NotFound.
#[tokio::test]
async fn verify_wrong_secret_returns_not_found() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    store
        .create(
            &AgentId::new("owner-1"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create");

    let err = store
        .verify("ak_0000000000000000000000000000000000") // 32 hex = 34 chars avec préfixe
        .await
        .expect_err("verify avec mauvais secret doit échouer");

    assert!(
        matches!(err, gradatum_acl_auth::ApiKeyError::NotFound),
        "mauvais secret → NotFound, obtenu : {err}"
    );
}

/// Lister les clés actives.
#[tokio::test]
async fn list_active_keys() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    store
        .create(
            &AgentId::new("owner-a"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create a");
    store
        .create(
            &AgentId::new("owner-b"),
            vec!["vault_write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create b");

    let keys = store.list(false, None).await.expect("list");
    assert_eq!(keys.len(), 2, "2 clés actives attendues");
    assert!(keys.iter().any(|k| k.owner == "owner-a"));
    assert!(keys.iter().any(|k| k.owner == "owner-b"));
}

/// Lister toutes les clés (y compris révoquées).
#[tokio::test]
async fn list_all_includes_revoked() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let mat_a = store
        .create(
            &AgentId::new("owner-a"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create a");
    store
        .create(
            &AgentId::new("owner-b"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create b");

    // Révoquer owner-a.
    store.revoke(&mat_a.prefix).await.expect("revoke a");

    // list(false) = seulement actives.
    let active = store.list(false, None).await.expect("list active");
    assert_eq!(active.len(), 1, "1 clé active après révocation");

    // list(true) = toutes.
    let all = store.list(true, None).await.expect("list all");
    assert_eq!(all.len(), 2, "2 clés au total");
}

/// Révoquer une clé → AlreadyRevoked sur second appel.
#[tokio::test]
async fn revoke_twice_returns_already_revoked() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let mat = store
        .create(
            &AgentId::new("owner-x"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create");

    store
        .revoke(&mat.prefix)
        .await
        .expect("première révocation");

    let err = store
        .revoke(&mat.prefix)
        .await
        .expect_err("deuxième révocation doit échouer");

    assert!(
        matches!(err, gradatum_acl_auth::ApiKeyError::AlreadyRevoked),
        "deuxième révocation → AlreadyRevoked, obtenu : {err}"
    );
}

/// Révoquer une clé inexistante → NotFound.
#[tokio::test]
async fn revoke_nonexistent_returns_not_found() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let err = store
        .revoke("ak_inexistant")
        .await
        .expect_err("révocation clé inexistante doit échouer");

    assert!(
        matches!(err, gradatum_acl_auth::ApiKeyError::NotFound),
        "clé inexistante → NotFound, obtenu : {err}"
    );
}

/// Rotation : nouvelle clé valide, ancienne révoquée.
#[tokio::test]
async fn rotate_produces_new_valid_key() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let mat_old = store
        .create(
            &AgentId::new("owner-r"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create");

    let mat_new = store
        .rotate(&mat_old.prefix)
        .await
        .expect("rotation doit réussir");

    // Le nouveau secret doit être différent de l'ancien.
    assert_ne!(
        mat_old.secret, mat_new.secret,
        "le nouveau secret doit être différent de l'ancien"
    );

    // L'ancienne clé doit être révoquée.
    let err = store
        .verify(&mat_old.secret)
        .await
        .expect_err("l'ancienne clé doit être révoquée après rotation");
    assert!(
        matches!(
            err,
            gradatum_acl_auth::ApiKeyError::AlreadyRevoked
                | gradatum_acl_auth::ApiKeyError::NotFound
        ),
        "ancienne clé après rotation → AlreadyRevoked ou NotFound, obtenu : {err}"
    );

    // La nouvelle clé doit être vérifiable.
    let key = store
        .verify(&mat_new.secret)
        .await
        .expect("nouvelle clé après rotation doit être vérifiable");
    assert_eq!(key.owner, "owner-r");
}

// ── Garde de scopes sur `api-key create` (niveau CLI) ─────────────────────────

/// Lance `gradatum-admin api-key create` sur un root jetable et rend sa sortie.
///
/// `extra` reçoit les arguments testés (`--scopes …`, `--read-only`). Le
/// répertoire `<root>/db` est créé car `SqliteApiKeyStore::init` crée le fichier,
/// pas son parent.
fn run_create_cli(root: &std::path::Path, extra: &[&str]) -> std::process::Output {
    // B6′b : `create` vérifie désormais que `--owner` est déclaré dans le preset ACL.
    // Les tests de la garde de SCOPES doivent donc partir d'un preset qui déclare leur
    // owner, sinon ils mesureraient la garde d'identité en croyant mesurer la leur.
    write_preset(root, &["scope-guard-owner"]);
    run_create_cli_owner(root, "scope-guard-owner", extra)
}

/// Variante de [`run_create_cli`] avec un `--owner` explicite (garde d'identité).
///
/// N'écrit AUCUN preset : c'est l'appelant qui décide de son contenu, ou de son absence.
fn run_create_cli_owner(
    root: &std::path::Path,
    owner: &str,
    extra: &[&str],
) -> std::process::Output {
    std::fs::create_dir_all(root.join("db")).expect("creer <root>/db");
    std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .args(["api-key", "create"])
        .arg("--root")
        .arg(root)
        .args(["--owner", owner])
        .args(["--tenant", "main"]) // A1 : plus de defaut, toujours explicite
        .args(extra)
        .output()
        .expect("lancer le binaire gradatum-admin")
}

/// Matérialise `<root>/config/bearer.toml` déclarant exactement `identities`.
fn write_preset(root: &std::path::Path, identities: &[&str]) {
    std::fs::create_dir_all(root.join("config")).expect("créer <root>/config");
    let body: String = identities
        .iter()
        .map(|id| {
            format!(
                "[[consumer]]\nidentity = \"{id}\"\nread_patterns = [\"main/**\"]\nwrite_patterns = [\"main/**\"]\n\n"
            )
        })
        .collect();
    std::fs::write(root.join("config/bearer.toml"), body).expect("écrire bearer.toml");
}

/// `create` sans `--scopes` → refus (exit ≠ 0), au niveau clap.
///
/// `--scopes` n'a AUCUN défaut : le seul défaut envisageable serait un scope de
/// lecture, que la garde refuse de toute façon hors `--read-only`. L'absence de
/// défaut est donc ce qui empêche `--help` d'annoncer une valeur inapte à émettre
/// une clé — ce test le verrouille.
#[test]
fn create_without_scopes_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let out = run_create_cli(dir.path(), &[]);

    assert!(
        !out.status.success(),
        "create sans --scopes doit sortir en erreur, code obtenu : {:?}",
        out.status.code()
    );
}

/// Le refus est expliqué : l'opérateur doit savoir quoi faire.
///
/// Les scopes sont fournis explicitement pour exercer la garde métier, et non le
/// message « argument requis » de clap : c'est le texte de NOTRE refus qui est ici
/// sous test.
#[test]
fn refusal_names_the_write_scopes_and_the_read_only_flag() {
    let dir = TempDir::new().expect("tempdir");
    let out = run_create_cli(dir.path(), &["--scopes", "vault_read"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("write, admin, service") && stderr.contains("--read-only"),
        "le message doit nommer les scopes d'écriture ET --read-only, obtenu : {stderr}"
    );
}

/// Un refus ne doit laisser aucune trace : pas de base créée en effet de bord.
///
/// Scopes explicites là encore : le refus doit venir de la garde, qui s'exécute
/// AVANT l'ouverture du store — un rejet clap ne prouverait rien sur cet ordre.
#[test]
fn refused_create_does_not_create_the_database() {
    let dir = TempDir::new().expect("tempdir");
    run_create_cli(dir.path(), &["--scopes", "vault_read"]);

    assert!(
        !dir.path().join("db/api_keys.sqlite").exists(),
        "un create refusé ne doit pas créer api_keys.sqlite"
    );
}

/// `--read-only` assume le choix : la création réussit.
#[test]
fn create_read_only_succeeds() {
    let dir = TempDir::new().expect("tempdir");
    let out = run_create_cli(dir.path(), &["--scopes", "vault_read", "--read-only"]);

    assert!(
        out.status.success(),
        "create --scopes vault_read --read-only doit réussir, code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `--read-only` émet bien un secret exploitable sur stdout.
#[test]
fn create_read_only_prints_the_secret() {
    let dir = TempDir::new().expect("tempdir");
    let out = run_create_cli(dir.path(), &["--scopes", "vault_read", "--read-only"]);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.trim().starts_with("ak_"),
        "le secret doit être imprimé sur stdout, obtenu : {stdout:?}"
    );
}

/// Un scope d'écriture réel passe sans `--read-only`.
#[test]
fn create_with_write_scope_succeeds() {
    let dir = TempDir::new().expect("tempdir");
    let out = run_create_cli(dir.path(), &["--scopes", "write"]);

    assert!(
        out.status.success(),
        "create --scopes write doit réussir, code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `vault_write` n'accorde RIEN : proche du bon nom, hors du jeu autorisé.
#[test]
fn create_with_lookalike_scope_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let out = run_create_cli(dir.path(), &["--scopes", "vault_write"]);

    assert!(
        !out.status.success(),
        "create --scopes vault_write doit être refusé, code obtenu : {:?}",
        out.status.code()
    );
}

/// `--read-only` combiné à un scope d'écriture est contradictoire → refus.
#[test]
fn create_read_only_with_write_scope_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let out = run_create_cli(dir.path(), &["--read-only", "--scopes", "admin"]);

    assert!(
        !out.status.success(),
        "--read-only + --scopes admin doit être refusé, code obtenu : {:?}",
        out.status.code()
    );
}

// ── Garde d'identité sur `api-key create` (B6′b, niveau CLI) ──────────────────
//
// Le défaut fermé : `api_keys.owner` et `bearer.toml`.`identity` ne sont reliés par
// AUCUNE intégrité référentielle. Une clé émise pour un owner absent du preset
// s'authentifie (200 sur `/auth/exchange`) puis se fait refuser sur tous les locus,
// **en silence** — indistinguable d'une panne. C'est la forme exacte de l'incident
// `engine` du 2026-07-27, une journée d'instruction pour un refus nominal.
//
// Les tests invoquent le VRAI binaire : la garde vit dans la commande. La tester au
// niveau du store ne prouverait rien sur le CLI livré (même raison qu'en garde de
// scopes ci-dessus).

/// Un `--owner` absent du preset est refusé.
///
/// Discriminant : `gemini-agent` est écrit correctement, porte des scopes valides et
/// un tenant valide. Rien d'autre que la relation à `bearer.toml` ne peut le refuser.
#[test]
fn create_with_undeclared_owner_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    write_preset(dir.path(), &["engine", "main-agent"]);
    let out = run_create_cli_owner(dir.path(), "gemini-agent", &["--scopes", "write"]);

    assert!(
        !out.status.success(),
        "un owner non déclaré doit être refusé, code obtenu : {:?}",
        out.status.code()
    );
}

/// Le refus est actionnable : il nomme l'identité, le fichier, et l'échappatoire.
#[test]
fn undeclared_owner_refusal_names_the_preset_and_the_escape_hatch() {
    let dir = TempDir::new().expect("tempdir");
    write_preset(dir.path(), &["engine"]);
    let out = run_create_cli_owner(dir.path(), "gemini-agent", &["--scopes", "write"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("gemini-agent")
            && stderr.contains("bearer.toml")
            && stderr.contains("--allow-unknown-identity"),
        "le message doit nommer l'identité, le preset ET l'échappatoire, obtenu : {stderr}"
    );
}

/// Un `--owner` déclaré passe — la garde ne bloque pas le cas nominal.
#[test]
fn create_with_declared_owner_succeeds() {
    let dir = TempDir::new().expect("tempdir");
    write_preset(dir.path(), &["engine"]);
    let out = run_create_cli_owner(dir.path(), "engine", &["--scopes", "write"]);

    assert!(
        out.status.success(),
        "un owner déclaré doit passer, code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Des identités déclarées SANS clé ne gênent pas la création d'une autre clé.
///
/// Discriminant : `admin`, `expert-prompt`, `expert-llm` et `expert-infra` sont des identités
/// du preset livré pour lesquelles ce test ne frappe AUCUNE clé. La garde porte sur le sens
/// clé → identité (l'owner d'une clé est-il déclaré ?), jamais sur identité → clé ; une
/// implémentation qui aurait joint les deux sens échouerait ici.
#[test]
fn declared_identities_without_keys_do_not_block_creation() {
    let dir = TempDir::new().expect("tempdir");
    write_preset(
        dir.path(),
        &[
            "admin",
            "expert-prompt",
            "expert-llm",
            "expert-infra",
            "engine",
        ],
    );
    let out = run_create_cli_owner(dir.path(), "engine", &["--scopes", "write"]);

    assert!(
        out.status.success(),
        "4 identités sans clé ne doivent pas gêner la 5e, code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `--allow-unknown-identity` lève la garde, et le dit.
#[test]
fn allow_unknown_identity_mints_the_key_and_warns() {
    let dir = TempDir::new().expect("tempdir");
    write_preset(dir.path(), &["engine"]);
    let out = run_create_cli_owner(
        dir.path(),
        "gemini-agent",
        &["--scopes", "write", "--allow-unknown-identity"],
    );

    assert!(
        out.status.success(),
        "--allow-unknown-identity doit émettre la clé, code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("WARNING"),
        "l'échappatoire doit rester bruyante"
    );
}

/// Un preset absent est un refus, pas un laissez-passer.
///
/// Discriminant : c'est le cas où le serveur retombe en DENY-ALL. Une garde qui
/// passerait ici émettrait précisément la clé qui ne peut pas fonctionner.
#[test]
fn missing_preset_is_a_refusal_not_a_pass() {
    let dir = TempDir::new().expect("tempdir");
    // Aucun `write_preset` : `<root>/config/bearer.toml` n'existe pas.
    let out = run_create_cli_owner(dir.path(), "engine", &["--scopes", "write"]);

    assert!(
        !out.status.success(),
        "preset absent doit refuser, code obtenu : {:?}",
        out.status.code()
    );
}

/// Un refus d'identité ne crée pas la base : la garde s'exécute avant l'ouverture du store.
#[test]
fn refused_owner_does_not_create_the_database() {
    let dir = TempDir::new().expect("tempdir");
    write_preset(dir.path(), &["engine"]);
    run_create_cli_owner(dir.path(), "gemini-agent", &["--scopes", "write"]);

    assert!(
        !dir.path().join("db/api_keys.sqlite").exists(),
        "un create refusé sur l'identité ne doit pas créer api_keys.sqlite"
    );
}

// ── Point d'entrée typé : `--owner` est PARSÉ (B6′b, livrable 1) ──────────────

/// Un `--owner` non conforme à `AgentId` est refusé avant tout accès au preset.
///
/// Discriminant : ce test échoue sur B6′a, où `--owner` était un `String` transmis
/// tel quel jusqu'à la colonne SQLite. `AgentId::parse` n'avait alors aucun appelant.
/// Chaque forme ci-dessous produirait un `owner` qu'aucune `identity` ne peut égaler.
#[test]
fn malformed_owner_is_rejected_at_the_cli_boundary() {
    for bad in [
        "Engine",
        "main_agent",
        "main agent",
        "main/agent",
        "",
        "-engine",
        "engine-",
    ] {
        let dir = TempDir::new().expect("tempdir");
        // Preset volontairement permissif : si le refus venait de la garde d'identité
        // plutôt que du parse, il resterait indétectable. On déclare donc l'owner brut.
        write_preset(dir.path(), &[bad, "engine"]);
        let out = run_create_cli_owner(dir.path(), bad, &["--scopes", "write"]);

        assert!(
            !out.status.success(),
            "--owner {bad:?} n'est pas un AgentId — doit être refusé, code : {:?}",
            out.status.code()
        );
    }
}

/// Le refus de parse est distinct de celui d'identité : l'opérateur doit savoir lequel.
#[test]
fn malformed_owner_refusal_is_distinct_from_the_identity_refusal() {
    let dir = TempDir::new().expect("tempdir");
    write_preset(dir.path(), &["engine"]);
    let out = run_create_cli_owner(dir.path(), "Engine", &["--scopes", "write"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("invalid --owner"),
        "un owner mal formé doit être annoncé comme tel, obtenu : {stderr}"
    );
}

// ── Une seule clé active par identité (R1, tâche 7, niveau CLI) ────────────────
//
// R1 : une identité = une seule clé active. `create` refuse de frapper une seconde
// clé active pour un owner qui en porte déjà une — deux secrets qui mappent un même
// `sub` (`api_keys.owner`) est précisément l'état que le modèle d'identité v2.0.0
// interdit. La garde vit dans la commande (même raison que les gardes de scopes et
// d'identité ci-dessus : la tester au niveau du store ne prouverait rien sur le CLI
// livré). L'état de départ est arrangé directement dans le store, au chemin exact que
// le CLI dérive (`<root>/db/api_keys.sqlite`), puis le binaire est invoqué.

/// Ouvre (ou crée) le store à l'emplacement exact dérivé par le CLI depuis `--root`.
///
/// Le helper `open_store` en tête de fichier vise `<dir>/api_keys.sqlite` (tests de
/// store pur) ; le CLI, lui, dérive `<root>/db/api_keys.sqlite`. Arranger l'état de
/// ces tests CLI exige donc CE chemin, sinon binaire et arrangement viseraient deux
/// fichiers distincts.
async fn open_store_at_cli_path(root: &std::path::Path) -> SqliteApiKeyStore {
    std::fs::create_dir_all(root.join("db")).expect("créer <root>/db");
    SqliteApiKeyStore::init(&root.join("db/api_keys.sqlite"))
        .await
        .expect("init store au chemin CLI doit réussir")
}

/// Lance `gradatum-admin api-key rotate --root <root> --prefix <prefix>`.
fn run_rotate_cli(root: &std::path::Path, prefix: &str) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .args(["api-key", "rotate"])
        .arg("--root")
        .arg(root)
        .args(["--prefix", prefix])
        .output()
        .expect("lancer le binaire gradatum-admin")
}

/// (a) `create` pour un owner qui porte déjà une clé active → refus (exit ≠ 0),
/// et le message oriente vers `rotate` en nommant le préfixe de la clé existante (R3).
#[tokio::test]
async fn create_is_refused_when_owner_already_has_an_active_key() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    write_preset(root, &["engine"]);

    // Arrange : une clé active pour `engine`.
    let store = open_store_at_cli_path(root).await;
    let existing = store
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create clé active");
    drop(store);

    // Act : create pour le même owner.
    let out = run_create_cli_owner(root, "engine", &["--scopes", "write"]);

    assert!(
        !out.status.success(),
        "une 2e clé active pour le même owner doit être refusée (R1), code : {:?}",
        out.status.code()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&existing.prefix) && stderr.contains("rotate"),
        "le refus doit nommer le préfixe existant ({}) ET la commande de rotation, obtenu : {stderr}",
        existing.prefix
    );
}

/// (b) `create` pour un owner dont la clé est révoquée → succès : R1 compte les clés
/// ACTIVES, une clé révoquée ne bloque pas la frappe d'une remplaçante.
#[tokio::test]
async fn create_succeeds_when_the_owners_key_is_revoked() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    write_preset(root, &["engine"]);

    // Arrange : une clé pour `engine`, puis révoquée.
    let store = open_store_at_cli_path(root).await;
    let existing = store
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create");
    store
        .revoke(&existing.prefix)
        .await
        .expect("arrange : revoke");
    drop(store);

    // Act : create pour le même owner, dont la seule clé est désormais révoquée.
    let out = run_create_cli_owner(root, "engine", &["--scopes", "write"]);

    assert!(
        out.status.success(),
        "une clé révoquée ne doit pas bloquer une nouvelle clé (R1), code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// (c) `rotate` sur un owner → exactement une clé active subsiste.
///
/// L'invariant est déjà préservé nativement (rotate révoque + remplace atomiquement) ;
/// ce test verrouille l'acquis contre une régression future.
#[tokio::test]
async fn rotate_leaves_exactly_one_active_key_for_the_owner() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    write_preset(root, &["engine"]);

    // Arrange : une clé active pour `engine`.
    let store = open_store_at_cli_path(root).await;
    let existing = store
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create");
    drop(store);

    // Act : rotation de cette clé.
    let out = run_rotate_cli(root, &existing.prefix);
    assert!(
        out.status.success(),
        "rotate doit réussir, code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    // Assert : exactement une clé active pour `engine`.
    let store = open_store_at_cli_path(root).await;
    let active = store.list(false, None).await.expect("list active");
    let n = active.iter().filter(|k| k.owner == "engine").count();
    assert_eq!(
        n, 1,
        "après rotation, exactement une clé active doit subsister pour 'engine'"
    );
}

// ── Remise à zéro du registre (R6/R7, tâche 8, niveau CLI) ─────────────────────
//
// R6 : une remise à zéro du registre de clés doit être possible.
// R7 : son périmètre est le registre de clés SEUL, JAMAIS le vault. Le reset opère
// par RÉVOCATION (jamais par suppression de ligne), donc la trace d'audit survit ;
// après reset, plus aucune clé active ⇒ le service tombe en état R5 (503 registre
// vierge) jusqu'à re-provisionnement.
//
// La confirmation suit l'idiome de l'oubli de notes : aperçu (dry-run) puis écho de
// la liste prévisualisée via `--confirm-prefixes`. Il n'existe volontairement AUCUN
// drapeau de contournement (`--yes`/`--force`) — le booléen aveugle qu'un alias
// porterait sans qu'un humain lise jamais la liste est précisément ce que l'écho
// interdit. La garde vit dans la commande : les tests invoquent le VRAI binaire.

/// Lance `gradatum-admin api-key reset --root <root> [extra…]`.
fn run_reset_cli(root: &std::path::Path, extra: &[&str]) -> std::process::Output {
    std::fs::create_dir_all(root.join("db")).expect("créer <root>/db");
    std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .args(["api-key", "reset"])
        .arg("--root")
        .arg(root)
        .args(extra)
        .output()
        .expect("lancer le binaire gradatum-admin")
}

/// Instantané logique d'un `index.db` de vault : (nombre de notes, attributions triées).
///
/// C'est exactement le couple que R7 exige de voir inchangé — « nombre de notes et
/// attributions identiques avant/après ». Le reset n'ouvre jamais ce fichier ; ce
/// helper le lit dans une connexion neuve, indépendante du binaire testé.
fn vault_notes_snapshot(index_db: &std::path::Path) -> (i64, Option<String>) {
    let conn = rusqlite::Connection::open(index_db).expect("ouvrir index.db pour l'instantané");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
        .expect("compter les notes");
    let authors: Option<String> = conn
        .query_row(
            "SELECT group_concat(author_id) FROM (SELECT author_id FROM notes ORDER BY id)",
            [],
            |r| r.get(0),
        )
        .expect("concaténer les attributions");
    (count, authors)
}

/// (1) Aperçu (dry-run) : liste les clés qui seront révoquées, ne mute rien.
#[tokio::test]
async fn reset_dry_run_lists_active_keys_and_mutates_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    // Arrange : 2 clés actives.
    let store = open_store_at_cli_path(root).await;
    let a = store
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create a");
    let b = store
        .create(
            &AgentId::new("main-agent"),
            vec!["admin".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create b");
    drop(store);

    // Act : dry-run (pas de --execute).
    let out = run_reset_cli(root, &[]);
    assert!(
        out.status.success(),
        "un aperçu doit réussir, code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    // L'aperçu nomme les 2 préfixes actifs.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&a.prefix) && stdout.contains(&b.prefix),
        "l'aperçu doit nommer les préfixes actifs, obtenu : {stdout}"
    );

    // Aucune mutation : les 2 clés restent actives.
    let store = open_store_at_cli_path(root).await;
    let active = store.list(false, None).await.expect("list active");
    assert_eq!(active.len(), 2, "un aperçu ne doit rien révoquer");
}

/// (2) Exécution confirmée : toutes les clés révoquées, AUCUNE ligne supprimée.
///
/// La trace d'audit survit — `list --all` continue de montrer les clés retirées.
#[tokio::test]
async fn reset_execute_revokes_all_keys_without_deleting_rows() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    let store = open_store_at_cli_path(root).await;
    let a = store
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create a");
    let b = store
        .create(
            &AgentId::new("main-agent"),
            vec!["admin".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create b");
    drop(store);

    // Act : reset confirmé par l'écho exact des préfixes actifs.
    let confirm = format!("{},{}", a.prefix, b.prefix);
    let out = run_reset_cli(root, &["--execute", "--confirm-prefixes", &confirm]);
    assert!(
        out.status.success(),
        "un reset confirmé doit réussir, code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let store = open_store_at_cli_path(root).await;
    // Plus aucune clé active.
    let active = store.list(false, None).await.expect("list active");
    assert!(active.is_empty(), "toutes les clés doivent être révoquées");
    // Aucune ligne supprimée : les 2 clés existent toujours, révoquées (audit).
    let all = store.list(true, None).await.expect("list all");
    assert_eq!(
        all.len(),
        2,
        "aucune ligne ne doit être supprimée — la trace d'audit survit"
    );
    assert!(
        all.iter().all(|k| k.is_revoked()),
        "les 2 clés doivent être révoquées, pas absentes"
    );
}

/// (3) Après reset, `has_any_active` rend `false` → le service tombe en état R5.
#[tokio::test]
async fn reset_leaves_store_with_no_active_key() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    let store = open_store_at_cli_path(root).await;
    let a = store
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create");
    drop(store);

    let out = run_reset_cli(root, &["--execute", "--confirm-prefixes", &a.prefix]);
    assert!(
        out.status.success(),
        "reset doit réussir, code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let store = open_store_at_cli_path(root).await;
    assert!(
        !store.has_any_active().await.expect("has_any_active"),
        "après reset, has_any_active doit être false (état R5)"
    );
}

/// (4) R7 — le vault n'est PAS touché : notes et attributions identiques avant/après.
///
/// Discriminant : un `index.db` de vault peuplé est planté au chemin canonique. Le
/// reset n'en tient aucune référence — il n'ouvre que `<root>/db/api_keys.sqlite`.
/// Une implémentation qui déborderait sur le vault ferait diverger l'instantané.
#[tokio::test]
async fn reset_leaves_the_vault_untouched() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    // Arrange : registre avec 1 clé active.
    let store = open_store_at_cli_path(root).await;
    let a = store
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create");
    drop(store);

    // Arrange : un index.db de vault avec des notes attribuées, au chemin canonique.
    let vault_dir = root.join("vault/.gradatum");
    std::fs::create_dir_all(&vault_dir).expect("créer le répertoire de vault");
    let index_db = vault_dir.join("index.db");
    {
        let conn = rusqlite::Connection::open(&index_db).expect("ouvrir index.db");
        conn.execute_batch(
            "CREATE TABLE notes (id TEXT PRIMARY KEY, author_id TEXT, vault_id TEXT);
             INSERT INTO notes VALUES ('n1', 'main-agent', 'main');
             INSERT INTO notes VALUES ('n2', 'acp-claude', 'main');
             INSERT INTO notes VALUES ('n3', 'main-agent', 'main');",
        )
        .expect("semer les notes");
    }
    let before = vault_notes_snapshot(&index_db);

    // Act : reset confirmé.
    let out = run_reset_cli(root, &["--execute", "--confirm-prefixes", &a.prefix]);
    assert!(
        out.status.success(),
        "reset doit réussir, code : {:?}, stderr : {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    // Assert : registre muté (clé révoquée) MAIS vault intact.
    let store = open_store_at_cli_path(root).await;
    assert!(
        store
            .list(false, None)
            .await
            .expect("list active")
            .is_empty(),
        "précondition : le reset a bien muté le registre"
    );
    let after = vault_notes_snapshot(&index_db);
    assert_eq!(
        before, after,
        "R7 : le reset ne doit toucher que le registre, jamais le vault (notes/attributions)"
    );
}

/// (5) Sans confirmation explicite (`--execute` seul) → refus, aucune mutation.
#[tokio::test]
async fn reset_execute_without_confirmation_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    let store = open_store_at_cli_path(root).await;
    store
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create");
    drop(store);

    // Act : --execute sans --confirm-prefixes.
    let out = run_reset_cli(root, &["--execute"]);
    assert!(
        !out.status.success(),
        "un reset sans confirmation doit être refusé, code : {:?}",
        out.status.code()
    );

    // Aucune mutation.
    let store = open_store_at_cli_path(root).await;
    let active = store.list(false, None).await.expect("list active");
    assert_eq!(active.len(), 1, "un refus ne doit rien révoquer");
}

/// (5b) Une confirmation qui ne correspond pas à la liste prévisualisée → refus.
///
/// C'est le cœur de la sûreté : l'écho doit être EXACT. Un préfixe bidon ne passe pas.
#[tokio::test]
async fn reset_execute_with_mismatched_confirmation_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    let store = open_store_at_cli_path(root).await;
    store
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create");
    drop(store);

    // Act : confirmation erronée.
    let out = run_reset_cli(root, &["--execute", "--confirm-prefixes", "ak_deadbeef"]);
    assert!(
        !out.status.success(),
        "une confirmation qui ne matche pas doit être refusée, code : {:?}",
        out.status.code()
    );

    let store = open_store_at_cli_path(root).await;
    let active = store.list(false, None).await.expect("list active");
    assert_eq!(active.len(), 1, "un refus ne doit rien révoquer");
}

/// (6) Aucun drapeau de contournement n'existe — le booléen aveugle est interdit.
///
/// Un `--yes`/`--force`/`--non-interactive` réintroduirait exactement ce que l'écho
/// de liste vise à empêcher. clap doit rejeter chacun (argument inconnu), et aucune
/// clé ne doit être révoquée au passage.
#[tokio::test]
async fn reset_has_no_blind_bypass_flag() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    let store = open_store_at_cli_path(root).await;
    store
        .create(
            &AgentId::new("engine"),
            vec!["write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("arrange : create");
    drop(store);

    for flag in ["--yes", "--force", "--non-interactive", "--confirm-all"] {
        let out = run_reset_cli(root, &["--execute", flag]);
        assert!(
            !out.status.success(),
            "aucun drapeau de contournement ne doit exister : {flag} devrait être rejeté"
        );
    }

    // Aucun bypass n'a pu révoquer.
    let store = open_store_at_cli_path(root).await;
    let active = store.list(false, None).await.expect("list active");
    assert_eq!(active.len(), 1, "aucun bypass ne doit révoquer la clé");
}
