//! `gradatum-admin init` — bootstrap d'un répertoire racine Gradatum.
//!
//! ## Effets
//! - Crée `{root}/{md,db,config}` avec mode 0750
//! - Génère une paire de clés Ed25519 (privée 0600, publique 0644) pour JWT
//! - Génère un bearer admin (32 octets aléatoires hex) → `config/admin.bearer.txt` 0600
//! - Matérialise le preset TOML dans `config/bearer.toml` 0640
//! - Écrit `config/server.toml` avec les défauts R-A1 0640
//!   (`jwt_ttl_human_secs=3600`, `jwt_ttl_service_secs=86400`, `revocation_store=sqlite`)
//! - Initialise `db/queue.sqlite` et `db/revocation.sqlite` en mode WAL
//!
//! ## Sécurité
//! - Refus silencieux si `config/admin.bearer.txt` existe déjà (passe `--force` pour override)
//! - Bearer affiché sur stdout **une seule fois**, uniquement en mode interactif

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::Args;
use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::SigningKey;
use pkcs8::LineEnding;
use rand::rngs::OsRng;
use rand::RngCore;
use rusqlite::Connection;

/// Arguments du sous-commande `init`.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Preset ACL à matérialiser.
    ///
    /// Noms courts embarqués : `hierarchical` (défaut), `flat`.
    /// Pour un preset custom : passer un chemin contenant `/`
    /// (ex. `--preset /etc/gradatum/mon-preset.toml`).
    #[arg(long, default_value = "hierarchical")]
    pub preset: String,

    /// Répertoire racine Gradatum à initialiser.
    #[arg(long)]
    pub root: PathBuf,

    /// Projets à substituer dans les templates `${PROJECT}` (séparés par virgule).
    #[arg(long)]
    pub projects: Option<String>,

    /// Adresse d'écoute du serveur (utilisée dans server.toml).
    #[arg(long, default_value = "127.0.0.1:19090")]
    pub bind: String,

    /// Mode non-interactif : pas de prompt, bearer non affiché sur stdout.
    /// Utile pour les tests et les pipelines CI.
    #[arg(long)]
    pub non_interactive: bool,

    /// Ré-init forcée même si `config/admin.bearer.txt` existe déjà.
    #[arg(long)]
    pub force: bool,
}

/// Point d'entrée du sous-commande `init`.
///
/// # Erreurs
/// - Retourne une erreur si `config/admin.bearer.txt` existe et que `--force` n'est pas passé.
/// - Propage toute erreur d'I/O ou de génération cryptographique.
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

/// Crée les sous-répertoires `{md,db,config}` avec le mode 0750.
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

/// Génère une paire de clés Ed25519 en PKCS8/SPKI PEM et les écrit dans `config/`.
///
/// - `jwt.private.pem` → mode 0600
/// - `jwt.public.pem`  → mode 0644
fn generate_jwt_keys(root: &Path) -> Result<()> {
    let mut csprng = OsRng;
    let signing = SigningKey::generate(&mut csprng);
    let verifying = signing.verifying_key();

    // Clé privée PKCS8 PEM
    let priv_pem = signing
        .to_pkcs8_pem(LineEnding::LF)
        .context("encodage de la clé privée JWT en PKCS8 PEM")?;
    let priv_path = root.join("config/jwt.private.pem");
    fs::write(&priv_path, priv_pem.as_bytes())
        .with_context(|| format!("écriture de {}", priv_path.display()))?;
    fs::set_permissions(&priv_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 sur {}", priv_path.display()))?;

    // Clé publique SPKI PEM
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

/// Génère un bearer admin (32 octets CSPRNG encodés en hex) et l'écrit dans
/// `config/admin.bearer.txt` avec le mode 0600.
///
/// Retourne le bearer en clair (à afficher une seule fois en mode interactif).
fn generate_admin_bearer(root: &Path) -> Result<String> {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let bearer = hex::encode(bytes);

    let path = root.join("config/admin.bearer.txt");
    fs::write(&path, bearer.as_bytes())
        .with_context(|| format!("écriture de {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 sur {}", path.display()))?;

    Ok(bearer)
}

/// Presets embarqués dans le binaire (compilés via `include_str!`).
/// Permet d'appeler `gradatum-admin init --preset <name>` depuis n'importe quel CWD.
const PRESET_HIERARCHICAL: &str = include_str!("../../../examples/presets/hierarchical.toml");
const PRESET_FLAT: &str = include_str!("../../../examples/presets/flat.toml");

/// Résout un preset par son nom court ou par chemin filesystem absolu / relatif contenant `/`.
///
/// Règle de détection :
/// - Contient `/` → lecture depuis le filesystem (chemin absolu ou relatif explicite).
/// - Sinon → lookup dans la map embarquée (`hierarchical`, `flat`).
///
/// Retourne une erreur si le nom court est inconnu ou si le fichier est illisible.
fn resolve_preset(preset: &str) -> Result<String> {
    if preset.contains('/') {
        // Chemin filesystem explicite (absolu ou relatif avec répertoire).
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

/// Charge le preset (embarqué ou filesystem), substitue les variables
/// `${PROJECTS}`, `${AGENT}`, `${THEME}` et écrit le résultat dans `config/bearer.toml` 0640.
///
/// Le preset embarqué est résolu indépendamment du CWD — permet d'appeler
/// `gradatum-admin init --preset hierarchical` depuis n'importe quel répertoire.
///
/// Si `bearer.toml` existe déjà : backup atomique `.bak.<ISO-TS>` avant écriture
/// (le merge consumer-aware n'est pas implémenté — patch.3 minimaliste). L'utilisateur
/// peut récupérer manuellement ses customisations via le fichier backup.
pub fn materialize_preset(root: &Path, preset: &str, projects: Option<&str>) -> Result<()> {
    let template = resolve_preset(preset)?;

    let projects_list = projects.unwrap_or("main");
    // Substitution des variables de template.
    // ${PROJECTS} → liste des projets, ${AGENT}/${THEME} → wildcards par défaut.
    let materialized = template
        .replace("${PROJECTS}", projects_list)
        .replace("${AGENT}", "*")
        .replace("${THEME}", "*");

    let bearer_toml = root.join("config/bearer.toml");

    // Backup atomique si fichier existe — pattern cohérent avec server.toml (patch.2).
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

/// Génère le contenu du template `server.toml` avec les défauts R-A1.
///
/// Retourne le `String` brut sans l'écrire — permet la réutilisation dans
/// `write_or_merge_server_toml` et dans les tests d'intégration.
///
/// - `[storage].vault_index_path` (renommé alpha.7 — alias `db_path` deprecated)
/// - `jwt_ttl_human_secs = 3600`  (1 heure)
/// - `jwt_ttl_service_secs = 86400` (24 heures)
/// - `revocation_store = "sqlite"`
/// - `api_keys_db_path = "{root}/db/api_keys.sqlite"` (AUTH-T2)
pub fn generate_server_toml_template(root: &Path, bind: &str) -> String {
    let root_str = root.display();
    format!(
        r#"# Généré par `gradatum-admin init` — modifier avec précaution.

[server]
bind = "{bind}"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "{root_str}"
vault_index_path = "{root_str}/db/index.sqlite"

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
# Embedder HTTP (Phase 2.1.1 alpha.8 — fix patch.1).
# Active la génération d'embeddings async via gradatum-worker → POST endpoint HTTP.
# Sans cette section, le worker démarre embedder=None et skip silencieusement les jobs embed_note.
# Default values — override in server.toml for your deployment.
enabled = true
endpoint = "http://localhost:8431/v1/embeddings"
model = "bge-m3-Q8_0"
dim = 1024
timeout_ms = 5000
"#
    )
}

/// Table de migrations renames : `(chemin_ancien, chemin_nouveau)`.
///
/// Utilisée dans `merge_user_config` : si une key du nouveau template est absente
/// du backup, on cherche si elle correspond à un ancien nom présent dans le backup.
/// Ajout par release ; ordre sans importance (indépendants).
const KEY_MIGRATIONS: &[(&str, &str)] = &[
    // alpha.7 RT11 : `storage.db_path` → `storage.vault_index_path`
    ("storage.db_path", "storage.vault_index_path"),
];

/// Si `config/server.toml` existe → backup atomique `.bak.<ISO-TS>` + merge dirigé
/// par schéma du nouveau template.
/// Sinon → écrire le template tel quel.
///
/// Pattern merge :
/// - Clé présente dans backup **ET** dans nouveau template → valeur backup préservée.
/// - Clé uniquement dans nouveau template → valeur défaut du template.
/// - Clé uniquement dans backup → écartée (suppression intentionnelle dans le nouveau schéma).
/// - Renames via `KEY_MIGRATIONS` : valeur ancienne injectée dans nouveau chemin.
fn write_or_merge_server_toml(root: &Path, bind: &str) -> Result<()> {
    let p = root.join("config/server.toml");
    let new_content = generate_server_toml_template(root, bind);

    let final_content = if p.exists() {
        // Backup atomique
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let backup = p.with_extension(format!("toml.bak.{ts}"));
        fs::copy(&p, &backup)
            .with_context(|| format!("backup {} → {}", p.display(), backup.display()))?;
        tracing::info!(backup = %backup.display(), "server.toml sauvegardé avant re-init");

        // Merge des valeurs user
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

/// Merge structurel : le BACKUP est autoritaire pour tout contenu user.
///
/// Sémantique patch.4 (inversion par rapport à patch.2) :
/// - Le BACKUP est autoritaire pour toutes ses keys (sections custom, sections d'extension,
///   keys customisées). Chaque key backup est appliquée sur le résultat.
/// - Le NEW template augmente avec les nouvelles keys/sections absentes du backup
///   (ces keys gardent leurs valeurs défaut du template).
/// - KEY_MIGRATIONS appliqués en pré-traitement sur une copie du backup (renommage
///   d'anciennes keys avant le walk, pour éviter leur insertion telle quelle).
///
/// Retourne le contenu `String` prêt à être écrit, en préservant les commentaires
/// TOML via `toml_edit::DocumentMut`.
pub fn merge_user_config(existing: &str, new_template: &str) -> Result<String> {
    use toml_edit::DocumentMut;

    let existing_doc: DocumentMut = existing
        .parse()
        .context("parse server.toml existant (backup)")?;
    let mut result: DocumentMut = new_template
        .parse()
        .context("parse nouveau template server.toml")?;

    // 1. Appliquer les migrations renames sur une copie du backup (pré-walk).
    //    Renommer old_path → new_path dans le backup pour que le walk voie
    //    directement la key canonique et ne réinsère pas l'ancienne.
    let mut backup_migrated = existing_doc.clone();
    for (old_path, new_path) in KEY_MIGRATIONS {
        if let Some(old_item) = lookup_item(backup_migrated.as_table(), old_path) {
            let val = old_item.clone();
            // Injecter dans new_path (toujours présent dans le backup migré si la section existe,
            // ou sera inséré par le walk dans la table résultat).
            // On insère directement dans backup_migrated pour que le walk le traite.
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

    // 2. Walk avec backup migré : BACKUP autoritaire.
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

/// Parcourt récursivement les keys de `source` (BACKUP) et les applique sur `target` (NEW).
///
/// Pour chaque key du backup :
/// - Si présente dans target ET les deux sont des Tables → récursion
/// - Si présente dans target (scalaire ou array) → écraser target avec valeur backup
/// - Si absente de target → AJOUTER (préserve sections/keys présentes uniquement dans le backup)
///
/// Les keys de target absentes du backup conservent leurs valeurs défaut NEW.
fn walk_and_merge(
    target: &mut toml_edit::Table,
    source: &toml_edit::Table,
    path_prefix: &str,
    preserved: &mut usize,
    new_keys: &mut usize,
    user_added: &mut usize,
) {
    // Phase 1 : itérer sur les keys du BACKUP (source) — sémantique BACKUP autoritaire
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
                // Scalaire ou array dans target → écraser avec valeur backup
                *target_item = source_item;
                *preserved += 1;
            }
            None => {
                // Key absente du NEW template → insérer depuis backup
                // (section/key d'extension user)
                target.insert(key.as_str(), source_item);
                *user_added += 1;
                tracing::info!(path = %full_path, "merge: section/key préservée (user extension)");
            }
        }
    }

    // Phase 2 : compter les keys de NEW absentes du backup (déjà dans target avec valeur défaut)
    for (key, _) in target.iter() {
        if !source.contains_key(key) {
            *new_keys += 1;
        }
    }
}

/// Lookup immutable d'un item par chemin `"section.key.subkey"`.
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

/// Injecte `value` à `path` dans `table` (chemin `"section.key"`).
///
/// Contrairement à `set_item`, insère le nœud intermédiaire (table de section)
/// s'il est absent. Utilisé dans KEY_MIGRATIONS pré-walk pour injecter la valeur
/// migrée même quand la section `[storage]` n'a pas encore le nouveau nom de clé.
///
/// Note : ne supporte que des chemins à 2 niveaux (`section.key`) — suffisant pour
/// les migrations actuelles dans `KEY_MIGRATIONS`.
fn set_item_or_insert(table: &mut toml_edit::Table, path: &str, value: toml_edit::Item) {
    let mut parts = path.splitn(2, '.');
    let head = match parts.next() {
        Some(h) => h,
        None => return,
    };
    let tail = parts.next();

    match tail {
        None => {
            // Clé top-level
            table.insert(head, value);
        }
        Some(key) => {
            // S'assurer que la sous-table existe
            if table.get(head).is_none() {
                table.insert(head, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            if let Some(toml_edit::Item::Table(sub)) = table.get_mut(head) {
                sub.insert(key, value);
            }
        }
    }
}

/// Supprime la key à `path` (`"section.key"`) dans `table`.
///
/// Silencieux si le chemin n'existe pas.
/// Note : ne supporte que des chemins à 2 niveaux — suffisant pour `KEY_MIGRATIONS`.
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

/// Vérifie que le contenu `server.toml` est parseable en TOML valide.
///
/// Exposé `pub(crate)` pour les tests de fumée uniquement.
#[allow(dead_code)]
pub(crate) fn validate_server_toml(content: &str) -> Result<()> {
    content
        .parse::<toml_edit::DocumentMut>()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("server.toml invalide : {e}"))
}

/// Initialise `db/queue.sqlite`, `db/revocation.sqlite` et `db/api_keys.sqlite` en mode WAL.
///
/// - `queue.sqlite` : table `jobs` + index (schéma `gradatum-queue`)
/// - `revocation.sqlite` : table `revoked` (schéma `gradatum-auth::SqliteRevocationStore`)
/// - `api_keys.sqlite` : table `api_keys` + index (schéma `gradatum-acl-auth`, AUTH-T2)
///
/// Toutes les opérations sont idempotentes (`CREATE TABLE IF NOT EXISTS`).
fn init_sqlite_dbs(root: &Path) -> Result<()> {
    // --- queue.sqlite ---
    let queue_path = root.join("db/queue.sqlite");
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

    // --- api_keys.sqlite (AUTH-T2 — gradatum-acl-auth) ---
    //
    // Initialisé via rusqlite directement (cohérent avec les autres DBs init dans cette
    // fonction sync). La DB sera réouverte par SqliteApiKeyStore via sqlx en runtime.
    // Migration identique au fichier `gradatum-acl-auth/migrations/V0001__create_api_keys.sql`.
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

    // Warn log si la table a des rows préexistants (A3 — re-init non-destructive).
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
