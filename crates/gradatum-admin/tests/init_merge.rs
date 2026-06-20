//! Tests merge structurel `server.toml`.
//!
//! Valide les scénarios du pattern de merge (sémantique BACKUP autoritaire) :
//! 1. Préservation des customisations user (ex. `[curator.llm]`)
//! 2. Préservation des sections backup-only absentes du template (bug reproducer patch.4)
//! 3. Migration rename via `KEY_MIGRATIONS` (`storage.db_path` → `storage.vault_index_path`)
//! 4. Nouvelles clés du template avec leurs valeurs défaut

use gradatum_admin::{generate_server_toml_template, merge_user_config};
use std::path::Path;

/// Vérifie que les customisations `[curator.llm]` présentes dans un server.toml
/// existant sont préservées après merge avec le nouveau template.
///
/// Cas terrain réel : config LIVE contient `base_url`, `api_key_env`,
/// `timeout_ms` que `install-gradatum-services.sh --force` ne doit PAS écraser.
#[test]
fn merge_preserves_user_curator_customizations() {
    let existing = r#"# Config LIVE
[server]
bind = "127.0.0.1:19090"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "/var/lib/gradatum"
vault_index_path = "/var/lib/gradatum/db/index.sqlite"

[auth]
jwt_public_key_path = "/var/lib/gradatum/config/jwt.public.pem"
jwt_private_key_path = "/var/lib/gradatum/config/jwt.private.pem"
jwt_ttl_human_secs = 3600
jwt_ttl_service_secs = 86400
revocation_store = "sqlite"
revocation_db_path = "/var/lib/gradatum/db/revocation.sqlite"
api_keys_db_path = "/var/lib/gradatum/db/api_keys.sqlite"

[acl]
preset_path = "/var/lib/gradatum/config/bearer.toml"

[log]
format = "json"

[curator]
backend = "openai_compat"
heuristic_admit_threshold = 0.85

[curator.llm]
backend = "openai_compat"
base_url = "http://localhost:8080"
model = "extract"
api_key_env = "GRADATUM_LLM_BEARER"
timeout_ms = 60000
"#;

    let new_template =
        generate_server_toml_template(Path::new("/var/lib/gradatum"), "127.0.0.1:19090");

    // Patch.4 : sémantique BACKUP autoritaire — [curator] et [curator.llm] absents du template
    // standard DOIVENT être préservés (sections d'extension user).
    let merged = merge_user_config(existing, &new_template).expect("merge ne doit pas échouer");

    // Clés standard préservées depuis le backup
    assert!(
        merged.contains("bind = \"127.0.0.1:19090\""),
        "bind préservé depuis backup"
    );
    assert!(
        merged.contains("jwt_ttl_human_secs = 3600"),
        "jwt_ttl_human_secs préservé"
    );
    assert!(
        merged.contains("jwt_ttl_service_secs = 86400"),
        "jwt_ttl_service_secs préservé"
    );
    assert!(
        merged.contains("vault_index_path = \"/var/lib/gradatum/db/index.sqlite\""),
        "vault_index_path préservé depuis backup"
    );

    // Sections d'extension user préservées (patch.4 — sémantique inversée)
    assert!(
        merged.contains("heuristic_admit_threshold = 0.85"),
        "curator.heuristic_admit_threshold doit être préservé"
    );
    assert!(
        merged.contains("base_url = \"http://localhost:8080\""),
        "curator.llm.base_url doit être préservé"
    );
    assert!(
        merged.contains("api_key_env = \"GRADATUM_LLM_BEARER\""),
        "curator.llm.api_key_env doit être préservé"
    );
    assert!(
        merged.contains("timeout_ms = 60000"),
        "curator.llm.timeout_ms doit être préservé"
    );

    // Le résultat est du TOML valide (parseable sans erreur)
    assert!(
        merged.parse::<toml_edit::DocumentMut>().is_ok(),
        "résultat merge doit être du TOML valide"
    );
}

/// Vérifie que `storage.db_path` legacy (présent dans le backup) est migré vers
/// `storage.vault_index_path` dans le résultat, via la table `KEY_MIGRATIONS`.
///
/// Cas terrain : upgrade depuis une version antérieure où la clé
/// s'appelait encore `db_path`.
#[test]
fn merge_drops_legacy_db_path_via_rename_migration() {
    let existing = r#"[storage]
root = "/var/lib/gradatum"
db_path = "/custom/legacy/index.sqlite"
"#;

    let new_template =
        generate_server_toml_template(Path::new("/var/lib/gradatum"), "127.0.0.1:19090");

    let merged = merge_user_config(existing, &new_template).expect("merge");

    // Valeur user préservée via la migration rename
    assert!(
        merged.contains("vault_index_path = \"/custom/legacy/index.sqlite\""),
        "db_path legacy doit être migré vers vault_index_path avec la valeur user"
    );
    // `db_path` seul ne doit pas apparaître dans le résultat
    // (on utilise "\ndb_path =" pour ne pas matcher api_keys_db_path ni revocation_db_path)
    assert!(
        !merged.contains("\ndb_path ="),
        "db_path (clé legacy) ne doit pas apparaître dans le résultat"
    );
}

/// Vérifie que les nouvelles clés du template (absentes du backup) apparaissent
/// avec leurs valeurs défaut, et que les clés user existantes sont préservées.
#[test]
fn merge_keeps_new_keys_with_defaults() {
    // Backup minimal : seulement [server].bind customisé, reste absent
    let existing = r#"[server]
bind = "0.0.0.0:9090"
"#;

    let new_template =
        generate_server_toml_template(Path::new("/var/lib/gradatum"), "127.0.0.1:19090");

    let merged = merge_user_config(existing, &new_template).expect("merge");

    // Clé user préservée (bind customisé)
    assert!(
        merged.contains("bind = \"0.0.0.0:9090\""),
        "bind user (0.0.0.0:9090) doit être préservé"
    );
    // Nouvelles sections du template avec leurs défauts
    assert!(
        merged.contains("[auth]"),
        "section [auth] doit être présente"
    );
    assert!(
        merged.contains("vault_index_path"),
        "vault_index_path doit être présent avec défaut"
    );
    assert!(merged.contains("[log]"), "section [log] doit être présente");
    assert!(
        merged.contains("format = \"json\""),
        "log.format défaut doit être présent"
    );
    assert!(
        merged.contains("jwt_ttl_human_secs = 3600"),
        "jwt_ttl_human_secs défaut doit être présent"
    );
}

/// Reproducer du bug LIVE patch.4 : la section `[curator]` et `[curator.llm]` du backup
/// doivent être préservées intégralement même si absentes du NEW template.
///
/// Ce test capture exactement le scénario qui causait gradatum-worker `inactive (dead)` :
/// `walk_and_merge` (patch.2) itérait sur les keys du NEW template uniquement → curator
/// jamais visité → absent du résultat → worker ne démarrait pas.
#[test]
fn merge_preserves_backup_only_sections_curator() {
    let existing = r#"
[server]
bind = "127.0.0.1:19090"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "/var/lib/gradatum"
vault_index_path = "/var/lib/gradatum/db/index.sqlite"

[curator]
backend = "openai_compat"
llm_review_enabled = true
heuristic_admit_threshold = 0.8

[curator.llm]
backend = "openai_compat"
base_url = "http://localhost:8080"
model = "extract"
api_key_env = "GRADATUM_LLM_BEARER"
timeout_ms = 60000
"#;

    let new_template =
        generate_server_toml_template(Path::new("/var/lib/gradatum"), "127.0.0.1:19090");

    let merged = merge_user_config(existing, &new_template).expect("merge ne doit pas échouer");

    // [curator] complet préservé
    assert!(
        merged.contains("llm_review_enabled = true"),
        "curator.llm_review_enabled doit être préservé"
    );
    assert!(
        merged.contains("heuristic_admit_threshold = 0.8"),
        "curator.heuristic_admit_threshold doit être préservé"
    );

    // [curator.llm] complet préservé
    assert!(
        merged.contains("base_url = \"http://localhost:8080\""),
        "curator.llm.base_url doit être préservé"
    );
    assert!(
        merged.contains("model = \"extract\""),
        "curator.llm.model doit être préservé"
    );
    assert!(
        merged.contains("api_key_env = \"GRADATUM_LLM_BEARER\""),
        "curator.llm.api_key_env doit être préservé"
    );
    assert!(
        merged.contains("timeout_ms = 60000"),
        "curator.llm.timeout_ms doit être préservé"
    );

    // Les sections standard du template sont toujours présentes
    assert!(
        merged.contains("[auth]"),
        "section [auth] du template doit être présente"
    );
    assert!(
        merged.contains("[log]"),
        "section [log] du template doit être présente"
    );

    // TOML valide
    assert!(
        merged.parse::<toml_edit::DocumentMut>().is_ok(),
        "résultat merge doit être du TOML valide"
    );
}

/// Vérifie que la section `[embed]` est bien
/// propagée par le merge structurel lorsque le backup user ne la contient pas.
///
/// Scénario : upgrade depuis une config sans `[embed]` vers une config avec `[embed]`.
/// La sémantique patch.4 (NEW augmente avec nouvelles keys absentes du backup) doit
/// ajouter `[embed]` avec les valeurs défaut du template.
///
/// Sans ce test, le bug pourrait régresser silencieusement (worker embedder=None).
#[test]
fn merge_adds_embed_section_when_backup_lacks_it() {
    // Backup alpha.7-patch.6 : sections standard sans [embed].
    let backup = r#"
[server]
bind = "127.0.0.1:19090"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "/var/lib/gradatum"
vault_index_path = "/var/lib/gradatum/db/index.sqlite"

[auth]
jwt_public_key_path = "/var/lib/gradatum/config/jwt.public.pem"
jwt_private_key_path = "/var/lib/gradatum/config/jwt.private.pem"
jwt_ttl_human_secs = 3600
jwt_ttl_service_secs = 86400
revocation_store = "sqlite"
revocation_db_path = "/var/lib/gradatum/db/revocation.sqlite"
api_keys_db_path = "/var/lib/gradatum/db/api_keys.sqlite"

[acl]
preset_path = "/var/lib/gradatum/config/bearer.toml"

[log]
format = "json"

[curator]
backend = "openai_compat"

[curator.llm]
base_url = "http://localhost:8080"
model = "extract"
"#;

    // Le template alpha.8-patch.1 inclut maintenant [embed].
    let new_template =
        generate_server_toml_template(Path::new("/var/lib/gradatum"), "127.0.0.1:19090");

    // Le template doit contenir [embed] (vérification préalable que le fix est appliqué).
    assert!(
        new_template.contains("[embed]"),
        "le template generate_server_toml_template doit contenir [embed] après patch.1"
    );

    let merged = merge_user_config(backup, &new_template).expect("merge ne doit pas échouer");

    // Section [embed] doit être présente avec les valeurs défaut du template.
    assert!(
        merged.contains("[embed]"),
        "section [embed] doit être présente dans le résultat (NEW template l'apporte)"
    );
    assert!(
        merged.contains("enabled = true"),
        "embed.enabled défaut (true) doit être présent"
    );
    assert!(
        merged.contains("endpoint = \"http://localhost:8436/v1/embeddings\""),
        "embed.endpoint défaut doit être présent"
    );
    // Default model — see docs/DEPLOYMENT.md for configuration.
    assert!(
        merged.contains("model = \"bge-m3-Q8_0\""),
        "embed.model default must be bge-m3-Q8_0"
    );
    assert!(
        merged.contains("dim = 1024"),
        "embed.dim défaut doit être 1024 (bge-m3-Q8_0)"
    );
    assert!(
        merged.contains("timeout_ms = 5000"),
        "embed.timeout_ms défaut doit être présent"
    );

    // Les customisations user existantes (curator) sont préservées.
    assert!(
        merged.contains("base_url = \"http://localhost:8080\""),
        "curator.llm.base_url user doit être préservé"
    );
    assert!(
        merged.contains("bind = \"127.0.0.1:19090\""),
        "server.bind user doit être préservé"
    );

    // TOML valide.
    assert!(
        merged.parse::<toml_edit::DocumentMut>().is_ok(),
        "résultat merge doit être du TOML valide"
    );
}

/// Vérifie qu'une section custom user au top-level sans équivalent dans le NEW template
/// est insérée dans le résultat.
#[test]
fn merge_adds_user_only_top_level_section() {
    let existing = r#"
[server]
bind = "127.0.0.1:19090"
metrics_bind = "127.0.0.1:19091"

[storage]
root = "/var/lib/gradatum"
vault_index_path = "/var/lib/gradatum/db/index.sqlite"

[my_custom_section]
foo = "bar"
number = 42
"#;

    let new_template =
        generate_server_toml_template(Path::new("/var/lib/gradatum"), "127.0.0.1:19090");

    let merged = merge_user_config(existing, &new_template).expect("merge ne doit pas échouer");

    assert!(
        merged.contains("foo = \"bar\""),
        "my_custom_section.foo doit être préservé"
    );
    assert!(
        merged.contains("number = 42"),
        "my_custom_section.number doit être préservé"
    );

    // TOML valide
    assert!(
        merged.parse::<toml_edit::DocumentMut>().is_ok(),
        "résultat merge doit être du TOML valide"
    );
}
