use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[test]
fn init_creates_full_layout_and_files() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Le preset "hierarchical" est embarqué dans le binaire via include_str! —
    // résolution indépendante du CWD depuis le fix T13-bis #6.
    // Le test s'exécute depuis /tmp (TempDir) sans current_dir forcé pour valider ce comportement.
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .arg("init")
        .arg("--preset")
        .arg("hierarchical")
        .arg("--root")
        .arg(root)
        .arg("--non-interactive")
        .current_dir(std::env::temp_dir()) // CWD arbitraire : prouve que le preset est embarqué
        .status()
        .unwrap();
    assert!(status.success(), "gradatum-admin init returned non-zero");

    // Layout
    for sub in ["md", "db", "config"] {
        assert!(root.join(sub).is_dir(), "{sub} directory not created");
    }

    // Bearer file chmod 0600
    let bearer = root.join("config/admin.bearer.txt");
    assert!(bearer.is_file(), "admin.bearer.txt missing");
    let mode = bearer.metadata().unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "admin.bearer.txt chmod = {mode:o}, expected 0600"
    );

    // v1.0.0 — `init` ne génère PLUS de paire PEM JWT.
    //
    // Ces deux fichiers n'étaient lus par aucun composant runtime : le serveur
    // signe et vérifie avec la seed brute `config/jwt-signing-key.secret` qu'il
    // crée lui-même au premier boot. Les scaffolder faisait sauvegarder et
    // tourner à l'opérateur des fichiers sans effet.
    assert!(
        !root.join("config/jwt.private.pem").exists(),
        "init ne doit plus générer jwt.private.pem (clé jamais lue par le runtime)"
    );
    assert!(
        !root.join("config/jwt.public.pem").exists(),
        "init ne doit plus générer jwt.public.pem (clé jamais lue par le runtime)"
    );
    // La vraie clé appartient au serveur : init ne la crée pas non plus.
    assert!(
        !root.join("config/jwt-signing-key.secret").exists(),
        "la seed de signature est créée par gradatum-server au premier boot, pas par init"
    );

    // bearer.toml exists chmod 0640
    let bearer_toml = root.join("config/bearer.toml");
    assert!(bearer_toml.is_file(), "bearer.toml missing");
    let mode = bearer_toml.metadata().unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640, "bearer.toml chmod = {mode:o}, expected 0640");

    // server.toml R-A1 defaults
    let server_toml = std::fs::read_to_string(root.join("config/server.toml"))
        .expect("server.toml missing or unreadable");
    assert!(
        server_toml.contains("jwt_ttl_human_secs = 3600"),
        "server.toml missing jwt_ttl_human_secs = 3600"
    );
    assert!(
        server_toml.contains("jwt_ttl_service_secs = 86400"),
        "server.toml missing jwt_ttl_service_secs = 86400"
    );
    // server.toml chmod 0640
    let mode = root
        .join("config/server.toml")
        .metadata()
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o640, "server.toml chmod = {mode:o}, expected 0640");

    // SQLite databases
    assert!(
        root.join("db/queue.sqlite").is_file(),
        "db/queue.sqlite missing"
    );
    assert!(
        root.join("db/revocation.sqlite").is_file(),
        "db/revocation.sqlite missing"
    );

    // AUTH-T2 : api_keys.sqlite créé avec la table api_keys
    assert!(
        root.join("db/api_keys.sqlite").is_file(),
        "db/api_keys.sqlite missing (AUTH-T2)"
    );

    // Vérifier que la table api_keys existe dans la DB
    let conn = rusqlite::Connection::open(root.join("db/api_keys.sqlite"))
        .expect("ouverture api_keys.sqlite");
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='api_keys'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);
    assert!(
        table_exists,
        "table api_keys non créée dans api_keys.sqlite"
    );

    // server.toml doit contenir api_keys_db_path
    assert!(
        server_toml.contains("api_keys_db_path"),
        "server.toml missing api_keys_db_path (AUTH-T2)"
    );

    // Régression Task 16 G2 : le template doit utiliser le nom canonique vault_index_path
    // et NON l'alias deprecated db_path (qui déclenche un WARN au boot).
    assert!(
        server_toml.contains("vault_index_path"),
        "server.toml doit utiliser vault_index_path (canonique alpha.7) — régression Task 16"
    );
    // Vérifie l'absence de la clé `db_path` seule (ligne "db_path = ...").
    // Note : on utilise "\ndb_path = " pour ne pas matcher "api_keys_db_path" ni "revocation_db_path".
    assert!(
        !server_toml.contains("\ndb_path = "),
        "server.toml ne doit pas contenir la clé db_path (alias deprecated — déclenche WARN au boot)"
    );
}
