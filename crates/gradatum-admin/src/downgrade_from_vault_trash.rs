//! Phase 2.1.2 alpha.9-patch.1 — Migration tool : porte les notes du
//! legacy vault `.vault-trash/**/*.md` vers gradatum via UPDATE direct.
//!
//! ## Structure vault-trash supportée
//!
//! L'outil parcourt récursivement `.vault-trash/` avec `walkdir` pour supporter
//! toutes les profondeurs observées en production :
//!
//! - Structure legacy 2 niveaux : `.vault-trash/<date>/<file>.md`
//! - Structure dédup 4 niveaux  : `.vault-trash/<date>/dedup/<section>/<file>.md`
//!
//! Le nom du répertoire daté (premier composant relatif à `.vault-trash/`) est
//! extrait depuis le chemin relatif pour construire le `status_reason`.
//!
//! ## Comportement
//!
//! Idempotent : skip si la note correspondante est déjà `status='downgraded'`.
//! Heuristique match : `substr(body_text, 1, 200)` du fichier vs DB (UTF-8 safe).
//! Mode dry-run + `--limit` pour usage progressif.
//!
//! ## Usage
//! ```text
//! gradatum-admin downgrade-from-legacy-vault-trash --legacy-vault-path ~/.memory-vault --root /var/lib/gradatum
//! gradatum-admin downgrade-from-legacy-vault-trash --dry-run --limit 50
//! ```

use anyhow::{Context, Result};
use gradatum_storage::{FileStorage, Storage as _};
use std::path::PathBuf;
use walkdir::WalkDir;

/// Arguments du sub-commande `downgrade-from-legacy-vault-trash`.
#[derive(Debug, Clone)]
pub struct DowngradeFromTrashArgs {
    /// Répertoire racine du legacy vault (contenant `.vault-trash/`).
    pub legacy_vault_path: PathBuf,
    /// Répertoire racine Gradatum (ex. `/var/lib/gradatum`).
    pub gradatum_root: PathBuf,
    /// Dry-run : affiche les actions sans écrire en base.
    pub dry_run: bool,
    /// Max nombre de notes à downgrader (réelles ou dry-run). `None` = illimité.
    pub limit: Option<usize>,
}

/// Statistiques retournées par [`run`].
#[derive(Debug, Default)]
pub struct DowngradeStats {
    /// Nombre de fichiers `.md` parcourus dans `.vault-trash/`.
    pub trash_files_scanned: usize,
    /// Nombre de fichiers matchés dans `notes` (statut quelconque).
    pub matched_in_gradatum: usize,
    /// Nombre de notes déjà `status='downgraded'` (skip idempotent).
    pub already_downgraded: usize,
    /// Nombre de notes effectivement downgraded (ou dry-run comptés).
    pub downgraded: usize,
    /// Nombre de fichiers sans correspondance dans gradatum.
    pub not_matched: usize,
}

/// Strip le frontmatter YAML `---\n...\n---\n` si présent.
///
/// Retourne le corps sans frontmatter. Si le format n'est pas reconnu,
/// retourne le texte d'entrée intact.
fn strip_frontmatter(body: &str) -> &str {
    if !body.starts_with("---\n") {
        return body;
    }
    // Cherche le délimiteur de fermeture "\n---\n" après l'opening "---\n"
    if let Some(end) = body[4..].find("\n---\n") {
        // end est relatif à body[4..], donc on skip "---\n" (4) + contenu (end) + "\n---\n" (5)
        return &body[4 + end + 5..];
    }
    body
}

/// Migre les notes du `.vault-trash` du legacy vault vers gradatum via `status='downgraded'`.
///
/// ## Comportement
/// - Parcourt récursivement `.vault-trash/**/*.md` via `walkdir` (profondeur arbitraire).
///   Supporte les structures legacy 2 niveaux ET la structure dédup 4 niveaux réelle.
/// - Pour chaque `.md`, strip le frontmatter YAML, prend les 200 premiers chars (UTF-8 safe).
/// - Cherche la correspondance dans `notes.body_text` via `substr(body_text, 1, 200)`.
/// - Si match et `status != 'downgraded'` → UPDATE atomique (status + status_reason + timestamps).
/// - Skip si déjà downgraded (idempotence).
/// - `--limit N` arrête après N downgrades comptabilisés (réels ou dry-run).
///
/// ## Effets de bord
/// - Écrit directement dans `vault/.gradatum/index.db` (SQL UPDATE).
/// - En dry-run : log les actions prévues sur stderr, aucune écriture.
/// - Log le résumé final sur stderr.
pub async fn run(args: DowngradeFromTrashArgs) -> Result<DowngradeStats> {
    let trash_dir = args.legacy_vault_path.join(".vault-trash");
    if !trash_dir.exists() {
        // Idempotent : pas de trash dir = rien à migrer = OK.
        // Cas typique post-cleanup (ex. suppression doublons .bck 2026-05-09).
        // Contrairement à l'absence de index.db (erreur fatale infra), l'absence
        // de .vault-trash est un état valide et attendu sur un vault propre.
        let stats = DowngradeStats::default();
        eprintln!(
            "info: .vault-trash absent ({}) — rien à migrer (idempotent)",
            trash_dir.display()
        );
        eprintln!(
            "downgrade-from-legacy-vault-trash: scanned={} matched={} already_downgraded={} downgraded={} not_matched={}",
            stats.trash_files_scanned,
            stats.matched_in_gradatum,
            stats.already_downgraded,
            stats.downgraded,
            stats.not_matched
        );
        return Ok(stats);
    }

    let index_path = args.gradatum_root.join("vault/.gradatum/index.db");
    if !index_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le worker doit avoir démarré au moins une fois",
            index_path.display()
        );
    }

    // Storage OpenDAL enraciné sur legacy_vault_path — pour lire les .md trash
    // (convergence v81 §6 : tout fichier Gradatum data path passe par OpenDAL).
    // Caveat : legacy_vault_path est une source externe (legacy vault), pas le vault
    // gradatum. Le FileStorage ici est éphémère, scoped à cette opération de migration.
    let trash_storage = FileStorage::new(&args.legacy_vault_path).with_context(|| {
        format!(
            "FileStorage init legacy_vault_path {}",
            args.legacy_vault_path.display()
        )
    })?;

    let mut stats = DowngradeStats::default();
    let conn = rusqlite::Connection::open(&index_path).context("ouverture index.db")?;

    // R1/M1 — prepare() AVANT la boucle (Phase 4 alpha.15 council backlog).
    //
    // La préparation de la requête SQL est coûteuse (parsing + compilation).
    // Exécuter `conn.prepare()` à l'intérieur de la boucle recompile la requête
    // à chaque itération — O(N) préparations inutiles pour N fichiers.
    // Déplacé hors de la boucle : 1 seule préparation, le Statement est réutilisé.
    let mut stmt = conn
        .prepare("SELECT id, status FROM notes WHERE substr(body_text, 1, 200) = ?1 LIMIT 1")
        .context("préparation requête match")?;

    // Parcours récursif .vault-trash/**/*.md — supporte toutes les profondeurs :
    //   - legacy 2 niveaux : .vault-trash/<date>/<file>.md      (min_depth=2)
    //   - dédup  4 niveaux : .vault-trash/<date>/dedup/<section>/<file>.md
    //
    // min_depth=2 : évite de retourner .vault-trash/ lui-même (depth=0) et les
    // répertoires datés directs (depth=1 = pas de fichiers .md attendus à ce niveau).
    // max_depth=10 : garde-fou anti-runaway, largement supérieur à la profondeur réelle.
    for entry in WalkDir::new(&trash_dir)
        .min_depth(2)
        .max_depth(10)
        .into_iter()
        .filter_map(|e| {
            // Ignorer les entrées illisibles (permissions) sans interrompre le parcours
            e.map_err(|err| {
                eprintln!("[WARN] entrée inaccessible dans .vault-trash : {err}");
            })
            .ok()
        })
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        // Guard limit : si on a déjà atteint la limite de downgrades, stop global
        if let Some(limit) = args.limit {
            if stats.downgraded >= limit {
                break;
            }
        }

        // Extraire le nom du répertoire daté = premier composant relatif à trash_dir.
        // Exemples :
        //   .vault-trash/2026-05-09/note.md             → "2026-05-09"
        //   .vault-trash/2026-05-09/dedup/ref/note.md   → "2026-05-09"
        let date_dir_name = path
            .strip_prefix(&trash_dir)
            .unwrap_or(path)
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());

        // Chemin relatif à legacy_vault_path — requis par Storage trait (chemins relatifs).
        // path est absolu (walkdir) ; strip_prefix récupère la partie relative.
        let rel_path = match path.strip_prefix(&args.legacy_vault_path) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => {
                eprintln!("[WARN] chemin hors legacy_vault_path : {}", path.display());
                continue;
            }
        };

        let body_bytes = match trash_storage.read(&rel_path).await {
            Ok(b) => b,
            Err(e) => {
                // R4 — decrement safe (Phase 4 alpha.15 council backlog).
                //
                // L'incrément `trash_files_scanned` était effectué AVANT la tentative
                // de lecture → un fichier illisible incrémentait le compteur puis le
                // décrémentait, créant un risque de underflow (usize = 0 - 1 = overflow
                // en mode release Rust si le compteur était à 0, ex. premier fichier
                // du parcours illisible). Fix : ne pas incrémenter avant lecture réussie.
                eprintln!("[WARN] lecture impossible {} : {e}", path.display());
                continue; // compteur PAS incrémenté pour les fichiers illisibles
            }
        };
        // Conversion bytes → String UTF-8 (les notes Markdown sont toujours UTF-8).
        // lossy_conversion pour resilience sur bytes éventuellement corrompus.
        let body = String::from_utf8_lossy(&body_bytes).into_owned();

        // R4 — incrément APRÈS lecture réussie (safe, pas de risk underflow)
        stats.trash_files_scanned += 1;

        let body_clean = strip_frontmatter(&body);
        // 200 premiers chars (UTF-8 safe via chars().take())
        let needle: String = body_clean.chars().take(200).collect();

        let row: Option<(String, String)> = stmt
            .query_row(rusqlite::params![needle], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .ok();

        match row {
            Some((id, status)) => {
                stats.matched_in_gradatum += 1;

                if status == "downgraded" {
                    stats.already_downgraded += 1;
                    continue;
                }

                if args.dry_run {
                    eprintln!(
                        "[DRY-RUN] would downgrade note_id={id} (file={})",
                        path.display()
                    );
                    stats.downgraded += 1;
                    continue;
                }

                // UPDATE atomique : status + status_reason + timestamps
                let now = chrono::Utc::now().timestamp_millis();
                let reason = format!("migrated from legacy-vault .vault-trash/{date_dir_name}/");

                conn.execute(
                    "UPDATE notes \
                     SET status = 'downgraded', \
                         status_reason = ?2, \
                         status_changed = ?3, \
                         updated = ?3 \
                     WHERE id = ?1",
                    rusqlite::params![id, reason, now],
                )
                .context("UPDATE downgrade note")?;

                stats.downgraded += 1;
            }
            None => {
                stats.not_matched += 1;
            }
        }
    }

    eprintln!(
        "downgrade-from-legacy-vault-trash: scanned={} matched={} already_downgraded={} downgraded={} not_matched={}",
        stats.trash_files_scanned,
        stats.matched_in_gradatum,
        stats.already_downgraded,
        stats.downgraded,
        stats.not_matched
    );

    Ok(stats)
}
