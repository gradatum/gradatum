//! `gradatum-admin backfill-authors` sub-command.
//!
//! Attributes an author to notes that have none — writing **both** the on-disk `.md`
//! frontmatter **and** the SQLite index column, coherently.
//!
//! ## Why this exists
//!
//! Since v2.0.0 the public write path refuses any client-supplied `author` by design, so a
//! batch of legacy-imported notes whose author is empty can no longer be re-attributed
//! through the API. This operator command closes that gap. It is the durable answer named in
//! the migration plan: a dedicated administration capability rather than a temporary
//! roll-back of the binary.
//!
//! ## Usage
//! ```text
//! gradatum-admin backfill-authors --root /var/lib/gradatum --tenant main --author main-agent --dry-run
//! gradatum-admin backfill-authors --root /var/lib/gradatum --tenant main --author main-agent
//! ```
//!
//! ## Coherence contract (the critical property)
//!
//! For every candidate note the command rewrites the `.md` frontmatter author **and** the
//! `notes.author_*` columns, and re-links the two through `content_hash`: the index
//! `content_hash` is recomputed from the rewritten `.md`, so the drift scanner and the
//! server's effective-note cache both stay coherent (a cache entry is invalidated the moment
//! its stored hash no longer matches the index).
//!
//! It writes the author field and **nothing else**. It deliberately does *not* reuse
//! `Vault::write_note`, whose full-row upsert re-applies `status = excluded.status`
//! unconditionally and would resurrect notes downgraded index-only (the D1.3 hazard that
//! `Vault::move_locus` has to neutralise) and could clobber an index-only `locus`. A targeted
//! author write touches the two representations of the author and leaves status, locus,
//! trust, forgotten flags and the body untouched.
//!
//! ## Idempotence and resumption
//!
//! Candidates are selected from the index (`author_id IS NULL OR author_id = ''`). Each note
//! is processed `.md`-first, index-last: an interruption leaves the index still reporting the
//! note as author-less, so the next run re-selects it and the identical `.md` rewrite is a
//! no-op. The selection query is therefore its own resumption point — a note leaves the set
//! only once its index row carries the author.
//!
//! A backup before running against a production vault is strongly recommended.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gradatum_acl_policy::AclEngine;
use gradatum_core::author::{AuthorKind, AuthorRef};
use gradatum_core::identity::ContentHash;
use gradatum_core::paths::vault_index_path;
use gradatum_core::scope::AgentId;

/// Arguments for the `backfill-authors` sub-command.
#[derive(Debug, Clone)]
pub struct BackfillAuthorsArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`) — holds `vault/` and `config/`.
    pub root: PathBuf,
    /// Target tenant / `vault_id`.
    pub tenant: String,
    /// Target author identity, validated against the ACL preset (like `api-key --owner`).
    pub author: String,
    /// Preview actions without writing anything.
    pub dry_run: bool,
    /// Maximum number of notes to process; unlimited when absent.
    pub limit: Option<usize>,
}

/// Report of a `backfill-authors` run.
#[derive(Debug, Default, Clone)]
#[must_use]
pub struct BackfillAuthorsReport {
    /// Candidate notes scanned (index author is NULL or empty).
    pub notes_scanned: usize,
    /// Notes attributed. In dry-run this is the count of notes that **would** be attributed
    /// (a pre-flight preview) — nothing is written to disk or the index.
    pub authors_assigned: usize,
    /// Notes skipped because their `.md` already carries a **different** author — never
    /// clobbered.
    pub skipped_drift: usize,
    /// Notes skipped because no `.md` was found on disk (index/disk drift — surfaced, not
    /// silenced).
    pub skipped_missing_md: usize,
    /// `true` when the run was a dry-run.
    pub dry_run: bool,
}

/// Validates the target identity against the ACL preset and builds the [`AuthorRef`].
///
/// Two barriers, identical in shape to `api-key create --owner`:
/// 1. **Parse-don't-validate** — `--author` crosses into [`AgentId`] here or not at all.
/// 2. **Referential integrity** — the identity must be declared by a `[[consumer]]` block of
///    the preset (`{root}/config/bearer.toml`); an undeclared identity is refused, never
///    written. An unreadable or unparsable preset is a refusal too (the server itself falls
///    back to DENY-ALL in that situation).
///
/// # Errors
/// Returns an error when `--author` is not a well-formed [`AgentId`], when the preset cannot
/// be read or parsed, or when the identity is absent from it.
fn validate_target_author(root: &Path, raw_author: &str) -> Result<AuthorRef> {
    let agent = AgentId::parse(raw_author).map_err(|e| {
        anyhow::anyhow!(
            "invalid --author: {e}\n\
             \n\
             An agent identity is lowercase `[a-z0-9-]`, non-empty, at most 64 bytes, and \
             carries no leading or trailing hyphen. It must match a `[[consumer]] identity` \
             of the ACL preset byte for byte."
        )
    })?;

    let preset_path = root.join("config").join("bearer.toml");
    let preset = std::fs::read_to_string(&preset_path).map_err(|e| {
        anyhow::anyhow!(
            "cannot read the ACL preset {}: {e}\n\
             \n\
             Without it the identity '{agent}' cannot be checked, and the server itself falls \
             back to DENY-ALL. Run `gradatum-admin init` to materialise the preset.",
            preset_path.display()
        )
    })?;
    let engine = AclEngine::from_preset_str(&preset).map_err(|e| {
        anyhow::anyhow!(
            "the ACL preset {} does not parse: {e}",
            preset_path.display()
        )
    })?;

    if !engine.has_identity(&agent) {
        bail!(
            "refusing to attribute notes to an undeclared identity: '{agent}' has no \
             `[[consumer]]` block in {}\n\
             \n\
             Declare `identity = \"{agent}\"` in the preset first.",
            preset_path.display()
        );
    }

    // ECON: `kind` figé à `MainAgent` — c'est le défaut du chemin d'écriture NOMINAL du
    // serveur pour une identité nue (persist.rs `parse_author`), et le `kind` est une
    // métadonnée d'audit DESCRIPTIVE qui ne gouverne aucune décision d'autorisation.
    // Upgrade -> ajouter `--author-kind` le jour où un backfill vers un `kind` autre que
    // main-agent devient un besoin réel.
    Ok(AuthorRef {
        kind: AuthorKind::MainAgent,
        id: agent.as_str().to_string(),
        display_name: None,
    })
}

/// Serialises an [`AuthorKind`] to its kebab-case index representation (`main-agent`, …),
/// byte-identical to what `SqliteIndex::write_note` stores in `notes.author_kind`.
fn author_kind_str(kind: AuthorKind) -> String {
    serde_json::to_string(&kind)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

/// Ordered on-disk path candidates for a note, mirroring `Vault::read_note_in`
/// (locus-aware → tenant root → legacy section-as-locus). The note is read from — and
/// rewritten to — the **first candidate that exists**, so no orphan `.md` is ever created.
fn md_path_candidates(vault_id: &str, locus: Option<&str>, section: &str, id: &str) -> Vec<String> {
    let mut v = Vec::with_capacity(3);
    if let Some(loc) = locus.filter(|l| !l.is_empty()) {
        v.push(format!("{vault_id}/{loc}/{id}.md"));
    }
    v.push(format!("{vault_id}/{id}.md"));
    v.push(format!("{vault_id}/{section}/{id}.md"));
    v
}

/// Per-note outcome, folded into the report by the caller.
enum Outcome {
    Assigned,
    SkippedDrift,
    SkippedMissingMd,
}

/// Entry point. Validates the author, then runs the (blocking) SQLite + filesystem pass.
///
/// # Errors
/// - the identity is invalid or undeclared (see `validate_target_author`);
/// - `index.db` is absent, or a `.md` is malformed on disk.
#[must_use = "the report states how many notes were attributed and how many were skipped"]
pub async fn run(args: BackfillAuthorsArgs) -> Result<BackfillAuthorsReport> {
    // Validated first: a refused invocation must not open the database or the vault.
    let author = validate_target_author(&args.root, &args.author)?;

    // SSOT : chemin de l'index via helper canonique.
    let db_path = vault_index_path(&args.root);
    if !db_path.exists() {
        bail!(
            "index.db not found: {} — the server must have started at least once",
            db_path.display()
        );
    }

    let vault_dir = args.root.join("vault");

    tokio::task::spawn_blocking(move || {
        run_sync(
            &db_path,
            &vault_dir,
            &args.tenant,
            &author,
            args.dry_run,
            args.limit,
        )
    })
    .await
    .context("spawn_blocking backfill_authors")?
}

/// Synchronous pass — one SQLite connection, `.md`-first / index-last per note.
fn run_sync(
    db_path: &Path,
    vault_dir: &Path,
    tenant: &str,
    author: &AuthorRef,
    dry_run: bool,
    limit: Option<usize>,
) -> Result<BackfillAuthorsReport> {
    let conn =
        rusqlite::Connection::open(db_path).context("opening index.db for backfill-authors")?;
    // WAL : lecture/écriture concurrente avec gradatum-server (cohérent backfill-titles).
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .context("PRAGMA journal_mode=WAL")?;

    let candidates = select_candidates(&conn, tenant, limit)?;

    let mut report = BackfillAuthorsReport {
        notes_scanned: candidates.len(),
        dry_run,
        ..Default::default()
    };

    for cand in &candidates {
        match process_one(&conn, vault_dir, tenant, author, dry_run, cand)? {
            Outcome::Assigned => report.authors_assigned += 1,
            Outcome::SkippedDrift => report.skipped_drift += 1,
            Outcome::SkippedMissingMd => report.skipped_missing_md += 1,
        }
    }

    tracing::info!(
        notes_scanned = report.notes_scanned,
        authors_assigned = report.authors_assigned,
        skipped_drift = report.skipped_drift,
        skipped_missing_md = report.skipped_missing_md,
        dry_run,
        "backfill-authors complete"
    );

    Ok(report)
}

/// One index-level candidate: `(id, locus, section)`.
struct Candidate {
    id: String,
    locus: Option<String>,
    section: String,
}

/// Selects notes whose index author is NULL or empty, oldest first.
///
/// This query is the resumption point: a note leaves the set only once its `author_id`
/// column is set, which the pass does last.
fn select_candidates(
    conn: &rusqlite::Connection,
    tenant: &str,
    limit: Option<usize>,
) -> Result<Vec<Candidate>> {
    let limit_clause = limit.map(|n| format!("LIMIT {n}")).unwrap_or_default();
    let query = format!(
        "SELECT id, locus, section FROM notes \
         WHERE vault_id = ?1 AND (author_id IS NULL OR author_id = '') \
         ORDER BY created ASC \
         {limit_clause}"
    );
    let mut stmt = conn
        .prepare(&query)
        .context("preparing SELECT notes without author")?;
    let rows = stmt
        .query_map(rusqlite::params![tenant], |row| {
            Ok(Candidate {
                id: row.get::<_, String>(0)?,
                locus: row.get::<_, Option<String>>(1)?,
                section: row.get::<_, String>(2)?,
            })
        })
        .context("executing SELECT notes without author")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("collecting notes without author")?;
    drop(stmt);
    Ok(rows)
}

/// Attributes one note: rewrite the `.md` frontmatter author, then the index columns.
fn process_one(
    conn: &rusqlite::Connection,
    vault_dir: &Path,
    tenant: &str,
    author: &AuthorRef,
    dry_run: bool,
    cand: &Candidate,
) -> Result<Outcome> {
    // Résoudre le chemin physique réel : premier candidat existant l'emporte.
    let candidates = md_path_candidates(tenant, cand.locus.as_deref(), &cand.section, &cand.id);
    let Some(rel) = candidates
        .into_iter()
        .find(|rel| vault_dir.join(rel).exists())
    else {
        tracing::warn!(
            id = %cand.id,
            "backfill-authors: no .md on disk for an indexed note — skipped (index/disk drift)"
        );
        return Ok(Outcome::SkippedMissingMd);
    };
    let md_path = vault_dir.join(&rel);

    let raw = std::fs::read_to_string(&md_path)
        .with_context(|| format!("reading .md {}", md_path.display()))?;
    let mut parsed = gradatum_markdown::parse(&raw)
        .map_err(|e| anyhow::anyhow!("parsing .md {}: {e}", md_path.display()))?;

    // Garde : ne JAMAIS écraser un auteur déjà présent qui diffère de la cible.
    // Un auteur == cible (kind+id) est un reliquat d'un run interrompu → on continue pour
    // solder l'index (réécriture .md idempotente).
    if let Some(existing) = &parsed.frontmatter.author
        && !existing.id.is_empty()
        && (existing.kind != author.kind || existing.id != author.id)
    {
        tracing::warn!(
            id = %cand.id,
            existing_author = %existing.id,
            "backfill-authors: .md already carries a different author — skipped"
        );
        return Ok(Outcome::SkippedDrift);
    }

    // Muter le SEUL champ auteur (+ updated), recalculer le content_hash canonique.
    // Un seul instant `now` pour le frontmatter ET la colonne `updated` de l'index (même
    // acte d'écriture logique). Seul le frontmatter alimente `content_hash`.
    let now = chrono::Utc::now();
    parsed.frontmatter.author = Some(author.clone());
    parsed.frontmatter.updated = Some(now);
    let new_hash = ContentHash::compute(&parsed.frontmatter, &parsed.body.markdown);
    parsed.content_hash = new_hash;

    if dry_run {
        return Ok(Outcome::Assigned);
    }

    // 1) `.md` d'abord — écriture atomique (tmp + rename même fs) pour ne jamais laisser un
    //    fichier tronqué en cas d'interruption.
    let md_out = gradatum_markdown::write_parsed(&parsed)
        .map_err(|e| anyhow::anyhow!("serialising .md {}: {e}", md_path.display()))?;
    write_atomic(&md_path, md_out.as_bytes())
        .with_context(|| format!("writing .md {}", md_path.display()))?;

    // 2) Index ensuite (point de reprise). La garde `author_id IS NULL OR = ''` rend
    //    l'UPDATE idempotent et sûr en cas de course : 0 ligne si déjà soldé ailleurs.
    let updated_ms = now.timestamp_millis();
    conn.execute(
        "UPDATE notes \
         SET author_kind = ?1, author_id = ?2, author_display_name = ?3, \
             content_hash = ?4, updated = ?5 \
         WHERE vault_id = ?6 AND id = ?7 AND (author_id IS NULL OR author_id = '')",
        rusqlite::params![
            author_kind_str(author.kind),
            author.id.as_str(),
            author.display_name.as_deref(),
            &new_hash.0[..],
            updated_ms,
            tenant,
            cand.id.as_str(),
        ],
    )
    .with_context(|| format!("UPDATE author for note {}", cand.id))?;

    Ok(Outcome::Assigned)
}

/// Atomically replaces `path` with `content` (temp file in the same directory + rename).
///
/// A rename on the same filesystem is atomic: the `.md` is either the old content or the new
/// one, never a torn write. The temp name carries the PID so two concurrent runs cannot
/// collide on it.
fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.gradatum-tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, content).with_context(|| format!("writing temp {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_kind_str_is_kebab_case() {
        assert_eq!(author_kind_str(AuthorKind::MainAgent), "main-agent");
        assert_eq!(author_kind_str(AuthorKind::SubAgent), "sub-agent");
        assert_eq!(author_kind_str(AuthorKind::Human), "human");
        assert_eq!(author_kind_str(AuthorKind::System), "system");
    }

    #[test]
    fn md_path_candidates_are_locus_then_root_then_section() {
        let with_locus = md_path_candidates("main", Some("knowledge/rust"), "decisions", "01ID");
        assert_eq!(
            with_locus,
            vec![
                "main/knowledge/rust/01ID.md",
                "main/01ID.md",
                "main/decisions/01ID.md",
            ]
        );
        // Locus vide == pas de locus : la 1re candidate est la racine tenant.
        let no_locus = md_path_candidates("main", Some(""), "decisions", "01ID");
        assert_eq!(no_locus, vec!["main/01ID.md", "main/decisions/01ID.md"]);
    }
}
