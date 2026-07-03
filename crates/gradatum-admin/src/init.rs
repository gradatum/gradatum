//! `gradatum-admin init` — bootstraps a Gradatum root directory.
//!
//! ## Effects
//! - Creates `{root}/{md,db,config}` with mode 0750
//! - Generates an Ed25519 key pair (private 0600, public 0644) for JWT
//! - Generates an admin bearer (32 random bytes, hex-encoded) → `config/admin.bearer.txt` 0600
//! - Materializes the ACL preset TOML into `config/bearer.toml` 0640
//! - Writes `config/server.toml` with default values 0640
//!   (`jwt_ttl_human_secs=3600`, `jwt_ttl_service_secs=86400`, `revocation_store=sqlite`)
//! - Initializes `db/queue.sqlite` and `db/revocation.sqlite` in WAL mode
//!
//! ## Security
//! - Silently refuses if `config/admin.bearer.txt` already exists (pass `--force` to override)
//! - Bearer is printed to stdout **exactly once**, only in interactive mode

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::Args;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
use gradatum_core::paths::queue_db_path;
use pkcs8::LineEnding;
use rand::RngCore;
use rand::rngs::OsRng;
use rusqlite::Connection;

/// Arguments for the `init` sub-command.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// ACL preset to materialize.
    ///
    /// Built-in short names: `hierarchical` (default), `flat`.
    /// For a custom preset, pass a path containing `/`
    /// (e.g. `--preset /etc/gradatum/my-preset.toml`).
    #[arg(long, default_value = "hierarchical")]
    pub preset: String,

    /// Gradatum root directory to initialize.
    #[arg(long)]
    pub root: PathBuf,

    /// Projects to substitute into `${PROJECT}` template placeholders (comma-separated).
    #[arg(long)]
    pub projects: Option<String>,

    /// Server listen address (written to `server.toml`).
    #[arg(long, default_value = "127.0.0.1:19090")]
    pub bind: String,

    /// Non-interactive mode: no prompt, bearer not printed to stdout.
    /// Useful for tests and CI pipelines.
    #[arg(long)]
    pub non_interactive: bool,

    /// Forces re-initialization even if `config/admin.bearer.txt` already exists.
    #[arg(long)]
    pub force: bool,
}

/// Entry point for the `init` sub-command.
///
/// # Errors
/// - Returns an error if `config/admin.bearer.txt` exists and `--force` is not set.
/// - Propagates any I/O or cryptographic generation error.
pub fn run(args: InitArgs) -> Result<()> {
    let bearer_marker = args.root.join("config/admin.bearer.txt");

    if bearer_marker.exists() && !args.force {
        return Err(anyhow!(
            "init déjà effectuée (admin.bearer.txt existe dans {}) ; \
             passer --force pour ré-initialiser",
            args.root.display()
        ));
    }

    create_layout(&args.root)?;
    // Under --force: remove existing secret files before regenerating.
    // Required because generate_jwt_keys and generate_admin_bearer use create_new (O_EXCL),
    // which fails if the file already exists — the intended behaviour in normal mode.
    if args.force {
        for secret_file in [
            "config/jwt.private.pem",
            "config/jwt.public.pem",
            "config/admin.bearer.txt",
        ] {
            let p = args.root.join(secret_file);
            if p.exists() {
                fs::remove_file(&p)
                    .with_context(|| format!("suppression (--force) de {}", p.display()))?;
            }
        }
    }
    generate_jwt_keys(&args.root)?;
    let bearer = generate_admin_bearer(&args.root)?;
    materialize_preset(&args.root, &args.preset, args.projects.as_deref())?;
    write_or_merge_server_toml(&args.root, &args.bind)?;
    init_sqlite_dbs(&args.root)?;

    if !args.non_interactive {
        println!(
            "\nBearer admin (sauvegardé dans {}, affiché UNE SEULE FOIS) :\n  {}",
            bearer_marker.display(),
            bearer
        );
    }

    Ok(())
}

/// Creates the `{md,db,config}` subdirectories with mode 0750.
fn create_layout(root: &Path) -> Result<()> {
    for sub in ["md", "db", "config"] {
        let p = root.join(sub);
        fs::create_dir_all(&p)
            .with_context(|| format!("création du répertoire {}", p.display()))?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o750))
            .with_context(|| format!("chmod 0750 sur {}", p.display()))?;
    }
    Ok(())
}

/// Generates an Ed25519 key pair in PKCS8/SPKI PEM format and writes it to `config/`.
///
/// - `jwt.private.pem` → mode 0600
/// - `jwt.public.pem`  → mode 0644
fn generate_jwt_keys(root: &Path) -> Result<()> {
    let mut csprng = OsRng;
    let signing = SigningKey::generate(&mut csprng);
    let verifying = signing.verifying_key();

    // Private key PKCS8 PEM
    let priv_pem = signing
        .to_pkcs8_pem(LineEnding::LF)
        .context("encodage de la clé privée JWT en PKCS8 PEM")?;
    let priv_path = root.join("config/jwt.private.pem");
    // Atomic write: O_EXCL + mode 0o600 at creation — no world-readable window.
    // If the file already exists, `create_new` fails → file already initialized.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&priv_path)
        .with_context(|| {
            format!(
                "ouverture en création exclusive de {} (déjà initialisé ?)",
                priv_path.display()
            )
        })?
        .write_all(priv_pem.as_bytes())
        .with_context(|| format!("écriture de {}", priv_path.display()))?;

    // Public key SPKI PEM
    let pub_pem = verifying
        .to_public_key_pem(LineEnding::LF)
        .context("encodage de la clé publique JWT en SPKI PEM")?;
    let pub_path = root.join("config/jwt.public.pem");
    fs::write(&pub_path, pub_pem.as_bytes())
        .with_context(|| format!("écriture de {}", pub_path.display()))?;
    fs::set_permissions(&pub_path, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("chmod 0644 sur {}", pub_path.display()))?;

    Ok(())
}

/// Generates an admin bearer (32 CSPRNG bytes, hex-encoded) and writes it to
/// `config/admin.bearer.txt` with mode 0600.
///
/// Returns the cleartext bearer (to be printed once in interactive mode).
fn generate_admin_bearer(root: &Path) -> Result<String> {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let bearer = hex::encode(bytes);

    let path = root.join("config/admin.bearer.txt");
    // Atomic write: O_EXCL + mode 0o600 at creation — no world-readable window.
    // If the file already exists, `create_new` fails → already initialized.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| {
            format!(
                "ouverture en création exclusive de {} (déjà initialisé ?)",
                path.display()
            )
        })?
        .write_all(bearer.as_bytes())
        .with_context(|| format!("écriture de {}", path.display()))?;

    Ok(bearer)
}

/// Presets embedded in the binary (compiled via `include_str!`).
/// Allows calling `gradatum-admin init --preset <name>` from any working directory.
const PRESET_HIERARCHICAL: &str = include_str!("../presets/hierarchical.toml");
const PRESET_FLAT: &str = include_str!("../presets/flat.toml");

/// Resolves a preset by its short name or by an absolute / relative filesystem path containing `/`.
///
/// Detection rule:
/// - Contains `/` → read from the filesystem (explicit absolute or relative path).
/// - Otherwise → lookup in the embedded map (`hierarchical`, `flat`).
///
/// Returns an error if the short name is unknown or if the file is unreadable.
fn resolve_preset(preset: &str) -> Result<String> {
    if preset.contains('/') {
        // Explicit filesystem path (absolute or relative with directory component).
        fs::read_to_string(preset)
            .with_context(|| format!("lecture du preset depuis le fichier '{preset}'"))
    } else {
        match preset {
            "hierarchical" => Ok(PRESET_HIERARCHICAL.to_owned()),
            "flat" => Ok(PRESET_FLAT.to_owned()),
            other => Err(anyhow!(
                "preset inconnu : '{other}'. \
                 Presets embarqués disponibles : hierarchical, flat. \
                 Pour un preset custom, passer un chemin contenant '/' \
                 (ex. --preset /etc/gradatum/mon-preset.toml)"
            )),
        }
    }
}

/// Loads the preset (embedded or filesystem), substitutes `${PROJECTS}`, `${AGENT}`,
/// and `${THEME}` template variables, then writes the result to `config/bearer.toml` 0640.
///
/// The embedded preset is resolved independently of the working directory, so
/// `gradatum-admin init --preset hierarchical` works from any directory.
///
/// If `bearer.toml` already exists, an atomic backup `.bak.<ISO-TS>` is created
/// before writing. Manual customizations can be recovered from the backup file.
pub fn materialize_preset(root: &Path, preset: &str, projects: Option<&str>) -> Result<()> {
    let template = resolve_preset(preset)?;

    let projects_list = projects.unwrap_or("main");
    // Substitute template variables.
    // ${PROJECTS} → project list, ${AGENT}/${THEME} → wildcard defaults.
    let materialized = template
        .replace("${PROJECTS}", projects_list)
        .replace("${AGENT}", "*")
        .replace("${THEME}", "*");

    let bearer_toml = root.join("config/bearer.toml");

    // Atomic backup if the file exists — consistent pattern with server.toml handling.
    if bearer_toml.exists() {
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let backup = bearer_toml.with_extension(format!("toml.bak.{ts}"));
        fs::copy(&bearer_toml, &backup)
            .with_context(|| format!("backup {} → {}", bearer_toml.display(), backup.display()))?;
        tracing::info!(backup = %backup.display(), "bearer.toml sauvegardé avant écrasement");
    }

    fs::write(&bearer_toml, materialized.as_bytes())
        .with_context(|| format!("écriture de {}", bearer_toml.display()))?;
    fs::set_permissions(&bearer_toml, fs::Permissions::from_mode(0o640))
        .with_context(|| format!("chmod 0640 sur {}", bearer_toml.display()))?;

    Ok(())
}

/// Generates the `server.toml` template content with default values.
///
/// Returns the raw `String` without writing — allows reuse in
/// `write_or_merge_server_toml` and in integration tests.
///
/// - `[storage].vault_index_path` — chemin canonique de l'index SQLite FTS5 ;
///   **respecté par le server et le worker** via `gradatum-core::paths::vault_index_path`
///   (canonical path, respected by both the server and the worker via `gradatum-core::paths::vault_index_path`).
///   Deprecated alias `db_path` is still accepted for reading (backward compatibility).
/// - `jwt_ttl_human_secs = 3600`  (1 hour)
/// - `jwt_ttl_service_secs = 86400` (24 hours)
/// - `revocation_store = "sqlite"`
/// - `api_keys_db_path = "{root}/db/api_keys.sqlite"`
pub fn generate_server_toml_template(root: &Path, bind: &str) -> String {
    let root_str = root.display();
    format!(
        r#"# Généré par `gradatum-admin init` — modifier avec précaution.

[server]
bind = "{bind}"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "{root_str}"
vault_index_path = "{root_str}/vault/.gradatum/index.db"

[auth]
jwt_public_key_path = "{root_str}/config/jwt.public.pem"
jwt_private_key_path = "{root_str}/config/jwt.private.pem"
jwt_ttl_human_secs = 3600
jwt_ttl_service_secs = 86400
revocation_store = "sqlite"
revocation_db_path = "{root_str}/db/revocation.sqlite"
api_keys_db_path = "{root_str}/db/api_keys.sqlite"

[acl]
preset_path = "{root_str}/config/bearer.toml"

[log]
format = "json"

[embed]
# Embedder HTTP.
# Active la génération d'embeddings async via gradatum-worker → POST endpoint HTTP.
# Sans cette section, le worker démarre embedder=None et skip silencieusement les jobs embed_note.
# Default values — override in server.toml for your deployment.
enabled = true
endpoint = "http://localhost:8436/v1/embeddings"
model = "bge-m3-Q8_0"
dim = 1024
timeout_ms = 5000
"#
    )
}

/// Rename migration table: `(old_path, new_path)`.
///
/// Used in `merge_user_config`: if a key from the new template is absent from
/// the backup, the table is checked for a matching old name present in the backup.
/// New entries are added per release; order is irrelevant (entries are independent).
const KEY_MIGRATIONS: &[(&str, &str)] = &[
    // `storage.db_path` → `storage.vault_index_path`
    ("storage.db_path", "storage.vault_index_path"),
];

/// If `config/server.toml` exists, creates an atomic backup `.bak.<ISO-TS>` and
/// merges user values according to the new template schema.
/// Otherwise, writes the template as-is.
///
/// Merge semantics:
/// - Key in backup **and** in new template → backup value is preserved.
/// - Key only in new template → template default value is used.
/// - Key only in backup → discarded (intentionally removed from the new schema).
/// - Renames via `KEY_MIGRATIONS`: old value is injected into the new path.
fn write_or_merge_server_toml(root: &Path, bind: &str) -> Result<()> {
    let p = root.join("config/server.toml");
    let new_content = generate_server_toml_template(root, bind);

    let final_content = if p.exists() {
        // Atomic backup
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let backup = p.with_extension(format!("toml.bak.{ts}"));
        fs::copy(&p, &backup)
            .with_context(|| format!("backup {} → {}", p.display(), backup.display()))?;
        tracing::info!(backup = %backup.display(), "server.toml sauvegardé avant re-init");

        // Merge user values
        let existing =
            fs::read_to_string(&p).with_context(|| format!("lecture de {}", p.display()))?;
        merge_user_config(&existing, &new_content)?
    } else {
        new_content
    };

    fs::write(&p, final_content.as_bytes())
        .with_context(|| format!("écriture de {}", p.display()))?;
    fs::set_permissions(&p, fs::Permissions::from_mode(0o640))
        .with_context(|| format!("chmod 0640 sur {}", p.display()))?;

    Ok(())
}

/// Structural merge: the BACKUP is authoritative for all user content.
///
/// Semantics:
/// - The BACKUP is authoritative for all its keys (custom sections, extension sections,
///   customized keys). Each backup key is applied onto the result.
/// - The NEW template contributes keys/sections absent from the backup
///   (these keys keep their template default values).
/// - `KEY_MIGRATIONS` are applied as a pre-processing step on a copy of the backup
///   (renaming old keys before the walk, to avoid inserting them verbatim).
///
/// Returns the `String` content ready to write, preserving TOML comments
/// via `toml_edit::DocumentMut`.
pub fn merge_user_config(existing: &str, new_template: &str) -> Result<String> {
    use toml_edit::DocumentMut;

    let existing_doc: DocumentMut = existing
        .parse()
        .context("parse server.toml existant (backup)")?;
    let mut result: DocumentMut = new_template
        .parse()
        .context("parse nouveau template server.toml")?;

    // 1. Apply rename migrations on a copy of the backup (pre-walk).
    //    Rename old_path → new_path in the backup so the walk sees
    //    the canonical key directly and does not reinsert the old one.
    let mut backup_migrated = existing_doc.clone();
    for (old_path, new_path) in KEY_MIGRATIONS {
        if let Some(old_item) = lookup_item(backup_migrated.as_table(), old_path) {
            let val = old_item.clone();
            // Inject into new_path (always present in the migrated backup if the section exists,
            // or will be inserted by the walk into the result table).
            // Inserted directly into backup_migrated so the walk processes it.
            set_item_or_insert(backup_migrated.as_table_mut(), new_path, val);
            remove_path(backup_migrated.as_table_mut(), old_path);
            tracing::info!(
                old = %old_path,
                new = %new_path,
                "merge server.toml — rename migration appliquée pré-walk"
            );
        }
    }

    let mut preserved = 0usize;
    let mut new_keys = 0usize;
    let mut user_added = 0usize;

    // 2. Walk with migrated backup: BACKUP is authoritative.
    walk_and_merge(
        result.as_table_mut(),
        backup_migrated.as_table(),
        "",
        &mut preserved,
        &mut new_keys,
        &mut user_added,
    );

    tracing::info!(
        preserved,
        new_keys,
        user_added,
        "merge server.toml — valeurs préservées + nouvelles clés avec défauts + extensions user"
    );

    Ok(result.to_string())
}

/// Recursively walks the keys of `source` (BACKUP) and applies them onto `target` (NEW).
///
/// For each backup key:
/// - Present in target AND both are Tables → recurse
/// - Present in target (scalar or array) → overwrite target with backup value
/// - Absent from target → INSERT (preserves sections/keys present only in the backup)
///
/// Keys in target absent from the backup retain their NEW default values.
fn walk_and_merge(
    target: &mut toml_edit::Table,
    source: &toml_edit::Table,
    path_prefix: &str,
    preserved: &mut usize,
    new_keys: &mut usize,
    user_added: &mut usize,
) {
    // Iterate over BACKUP (source) keys — BACKUP-authoritative semantics
    let source_keys: Vec<String> = source.iter().map(|(k, _)| k.to_string()).collect();

    for key in &source_keys {
        let full_path = if path_prefix.is_empty() {
            key.clone()
        } else {
            format!("{path_prefix}.{key}")
        };

        let source_item = match source.get(key.as_str()) {
            Some(it) => it.clone(),
            None => continue,
        };

        match target.get_mut(key.as_str()) {
            Some(target_item)
                if matches!(target_item, toml_edit::Item::Table(_))
                    && matches!(source_item, toml_edit::Item::Table(_)) =>
            {
                // Les deux sont des Tables → récursion
                if let (toml_edit::Item::Table(t_target), toml_edit::Item::Table(t_source)) =
                    (target_item, &source_item)
                {
                    walk_and_merge(
                        t_target, t_source, &full_path, preserved, new_keys, user_added,
                    );
                }
            }
            Some(target_item) => {
                // Scalar or array in target → overwrite with backup value
                *target_item = source_item;
                *preserved += 1;
            }
            None => {
                // Key absent from the NEW template → insert from backup
                // (user extension section/key)
                target.insert(key.as_str(), source_item);
                *user_added += 1;
                tracing::info!(path = %full_path, "merge: section/key préservée (user extension)");
            }
        }
    }

    // Count NEW keys absent from the backup (already in target with default value)
    for (key, _) in target.iter() {
        if !source.contains_key(key) {
            *new_keys += 1;
        }
    }
}

/// Immutable lookup of an item by path `"section.key.subkey"`.
fn lookup_item<'a>(table: &'a toml_edit::Table, path: &str) -> Option<&'a toml_edit::Item> {
    let mut parts = path.splitn(2, '.');
    let head = parts.next()?;
    let rest = parts.next();

    match (table.get(head), rest) {
        (Some(item), None) => Some(item),
        (Some(toml_edit::Item::Table(sub)), Some(tail)) => lookup_item(sub, tail),
        _ => None,
    }
}

/// Injects `value` at `path` into `table` (path format: `"section.key"`).
///
/// Unlike `set_item`, inserts the intermediate node (section table) if absent.
/// Used in `KEY_MIGRATIONS` pre-walk to inject the migrated value even when
/// the `[storage]` section does not yet contain the new key name.
///
/// Note: supports only two-level paths (`section.key`) — sufficient for
/// current entries in `KEY_MIGRATIONS`.
fn set_item_or_insert(table: &mut toml_edit::Table, path: &str, value: toml_edit::Item) {
    let mut parts = path.splitn(2, '.');
    let head = match parts.next() {
        Some(h) => h,
        None => return,
    };
    let tail = parts.next();

    match tail {
        None => {
            // Top-level key
            table.insert(head, value);
        }
        Some(key) => {
            // Ensure the sub-table exists
            if table.get(head).is_none() {
                table.insert(head, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            if let Some(toml_edit::Item::Table(sub)) = table.get_mut(head) {
                sub.insert(key, value);
            }
        }
    }
}

/// Removes the key at `path` (`"section.key"`) from `table`.
///
/// Silent if the path does not exist.
/// Note: supports only two-level paths — sufficient for `KEY_MIGRATIONS`.
fn remove_path(table: &mut toml_edit::Table, path: &str) {
    let mut parts = path.splitn(2, '.');
    let head = match parts.next() {
        Some(h) => h,
        None => return,
    };
    let tail = parts.next();

    match tail {
        None => {
            table.remove(head);
        }
        Some(key) => {
            if let Some(toml_edit::Item::Table(sub)) = table.get_mut(head) {
                sub.remove(key);
            }
        }
    }
}

/// Validates that `server.toml` content is parseable as valid TOML.
///
/// Exposed as `pub(crate)` for smoke tests only.
#[allow(dead_code)]
pub(crate) fn validate_server_toml(content: &str) -> Result<()> {
    content
        .parse::<toml_edit::DocumentMut>()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("server.toml invalide : {e}"))
}

/// Initializes `db/queue.sqlite`, `db/revocation.sqlite`, and `db/api_keys.sqlite` in WAL mode.
///
/// - `queue.sqlite`      : `jobs` table + index (schema `gradatum-queue`)
/// - `revocation.sqlite` : `revoked` table (schema `gradatum-auth::SqliteRevocationStore`)
/// - `api_keys.sqlite`   : `api_keys` table + index (schema `gradatum-acl-auth`)
///
/// All operations are idempotent (`CREATE TABLE IF NOT EXISTS`).
fn init_sqlite_dbs(root: &Path) -> Result<()> {
    // --- queue.sqlite ---
    // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
    let queue_path = queue_db_path(root);
    let conn = Connection::open(&queue_path)
        .with_context(|| format!("ouverture de {}", queue_path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS jobs (
             id           TEXT    PRIMARY KEY,
             kind         TEXT    NOT NULL,
             payload_json TEXT    NOT NULL,
             status       TEXT    NOT NULL,
             lease_until  INTEGER,
             created_at   INTEGER NOT NULL,
             updated_at   INTEGER NOT NULL,
             attempts     INTEGER NOT NULL DEFAULT 0,
             last_error   TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_jobs_status_lease ON jobs(status, lease_until);",
    )
    .with_context(|| format!("initialisation de {}", queue_path.display()))?;

    // --- revocation.sqlite ---
    let revoc_path = root.join("db/revocation.sqlite");
    let conn = Connection::open(&revoc_path)
        .with_context(|| format!("ouverture de {}", revoc_path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS revoked (
             jti        TEXT    PRIMARY KEY,
             exp        INTEGER NOT NULL,
             revoked_at INTEGER NOT NULL
         );",
    )
    .with_context(|| format!("initialisation de {}", revoc_path.display()))?;

    // --- api_keys.sqlite ---
    //
    // Initialized via rusqlite directly (consistent with the other DBs initialized in this
    // sync function). The DB will be reopened by SqliteApiKeyStore via sqlx at runtime.
    // Schema identical to `gradatum-acl-auth/migrations/V0001__create_api_keys.sql`.
    let api_keys_path = root.join("db/api_keys.sqlite");
    let conn = Connection::open(&api_keys_path)
        .with_context(|| format!("ouverture de {}", api_keys_path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS api_keys (
             id              TEXT    PRIMARY KEY,
             prefix          TEXT    NOT NULL UNIQUE,
             hash            TEXT    NOT NULL,
             owner           TEXT    NOT NULL,
             scopes_json     TEXT    NOT NULL,
             tenant_id       TEXT    NOT NULL,
             created_at      INTEGER NOT NULL,
             last_used_at    INTEGER,
             revoked_at      INTEGER,
             description     TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_api_keys_owner ON api_keys(owner) WHERE revoked_at IS NULL;
         CREATE INDEX IF NOT EXISTS idx_api_keys_prefix ON api_keys(prefix);",
    )
    .with_context(|| format!("initialisation de {}", api_keys_path.display()))?;

    // Warn if the table already has rows (re-init is non-destructive).
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))
        .unwrap_or(0);
    if count > 0 {
        tracing::warn!(
            rows = count,
            "api_keys table existe avec {} rows — re-init non-destructive",
            count
        );
    }

    Ok(())
}
