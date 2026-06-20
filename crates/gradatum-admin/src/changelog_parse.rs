//! CHANGELOG parser — produces project-map card entries.
//!
//! Reads a file in [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format
//! and produces `ChangelogEntry` values ready to be rendered as project-map cards.
//!
//! ## Expected format
//!
//! ```text
//! ## [x.y.z] - YYYY-MM-DD
//! ### Added
//! #### Optional sub-header
//! - **title**: description…
//! - another item
//! ```
//!
//! `####` sub-headers are transparent: their bullets are attributed to the parent
//! `###` section. Versions outside the `[from, to]` range are ignored. Items with
//! an empty title after stripping are silently ignored.
//!
//! ## Source marker
//!
//! Each entry receives a deterministic marker:
//! `changelog/{version}/{section_snake}/{idx}` — where `section_snake` is the
//! section name in lowercase snake_case and `idx` is the 0-based item rank within
//! that section.

use gradatum_core::project_map::KindKind;

/// Longueur maximale d'un titre de carte project-map.
const MAX_TITLE_LEN: usize = 80;

/// Une entrée extraite du CHANGELOG, prête pour la génération d'une carte project-map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
    /// Version SemVer extraite du `## [x.y.z]`.
    pub version: String,
    /// Section CHANGELOG parente (`Added`, `Fixed`, …).
    pub section: String,
    /// Nature de l'unité de travail, dérivée de la section.
    pub kind: KindKind,
    /// Cleaned title, truncated to 80 characters.
    pub title: String,
    /// Marqueur déterministe pour l'idempotence : `changelog/{ver}/{section_snake}/{idx}`.
    pub source_marker: String,
    /// Rang (0-indexé) de l'item dans la section courante.
    pub idx: usize,
}

/// Retourne `true` si la section CHANGELOG est dans l'allowlist Keep-a-Changelog standard.
///
/// Sections exclues (méta) : `Tests`, `Internal`, `Documentation`, `Behavior`,
/// `Infrastructure`, `Design References`, etc. — elles produisent du bruit
/// dans les cartes project-map.
///
/// Les sections standard reconnues sont celles définies par la spec
/// [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) plus les extensions
/// sémantiques courantes (`Performance`, `Privacy`).
fn is_standard_section(section: &str) -> bool {
    matches!(
        section,
        "Added"
            | "Changed"
            | "Fixed"
            | "Security"
            | "Removed"
            | "Deprecated"
            | "Performance"
            | "Privacy"
    )
}

/// Convertit un nom de section CHANGELOG en [`KindKind`].
///
/// Les sections inconnues produisent un `Task` avec un log d'avertissement.
fn section_to_kind(section: &str) -> KindKind {
    match section {
        "Added" => KindKind::Feature,
        "Changed" | "Performance" => KindKind::Enhancement,
        "Fixed" | "Security" | "Privacy" => KindKind::Fix,
        "Removed" | "Deprecated" => KindKind::Chore,
        other => {
            tracing::warn!(
                section = other,
                "section CHANGELOG inconnue → KindKind::Task"
            );
            KindKind::Task
        }
    }
}

/// Convertit un nom de section en snake_case minuscule pour le marqueur source.
fn section_to_snake(section: &str) -> String {
    let mut out = String::with_capacity(section.len());
    for (i, c) in section.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// Compare deux versions SemVer (`x.y.z`) numériquement.
///
/// Retourne `Ok(())` si `from ≤ ver ≤ to`, `Err(())` sinon.
/// Versions malformées (non parsable) sont traitées comme inférieures à tout.
fn semver_in_range(ver: &str, from: &str, to: &str) -> bool {
    fn parse(s: &str) -> (u64, u64, u64) {
        let parts: Vec<&str> = s.split('.').collect();
        let n = |i: usize| {
            parts
                .get(i)
                .and_then(|p| {
                    // Enlève les suffixes éventuels `-alpha`, `-rc.1`, etc.
                    let p = p.split('-').next().unwrap_or(p);
                    p.parse::<u64>().ok()
                })
                .unwrap_or(0)
        };
        (n(0), n(1), n(2))
    }
    let ver_t = parse(ver);
    let from_t = parse(from);
    let to_t = parse(to);
    from_t <= ver_t && ver_t <= to_t
}

/// Supprime le préfixe de bullet `- ` et les marqueurs markdown bold `**...**`.
///
/// Retourne le texte nettoyé, tronqué à [`MAX_TITLE_LEN`] caractères.
fn strip_and_truncate(raw: &str) -> String {
    let s = raw.trim();
    // Retire le préfixe de bullet
    let s = s.strip_prefix("- ").unwrap_or(s).trim();
    // Retire les **...** markdown bold
    let s = remove_bold(s);
    // Tronque à MAX_TITLE_LEN (char-safe)
    if s.chars().count() > MAX_TITLE_LEN {
        s.chars().take(MAX_TITLE_LEN).collect()
    } else {
        s
    }
}

/// Supprime les séquences `**...**` d'une chaîne.
fn remove_bold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        // Cherche `**`
        if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            // Cherche la fermeture `**`
            let start = i + 2;
            if let Some(end_offset) = s[start..].find("**") {
                let inner = &s[start..start + end_offset];
                out.push_str(inner);
                i = start + end_offset + 2;
                continue;
            }
        }
        // Caractère normal — avancer char par char (UTF-8 safe)
        let c_len = s[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&s[i..i + c_len]);
        i += c_len;
    }
    out
}

/// Parse `text` et retourne les entrées dans la plage `[from_version, to_version]` inclus.
///
/// Les `####` sous-headers sont ignorés côté ventilation kind — ils appartiennent à la
/// section `###` parente. Entrées avec titre vide après stripping sont ignorées
/// silencieusement.
///
/// Les sections méta (hors allowlist Keep-a-Changelog standard : `Tests`, `Internal`,
/// `Documentation`, `Behavior`, `Infrastructure`, etc.) sont filtrées par défaut
/// (`include_meta = false`). Avec `include_meta = true`, elles sont incluses comme
/// [`KindKind::Task`].
///
/// # Arguments
///
/// - `text` : contenu complet du fichier CHANGELOG.md.
/// - `from_version` : version SemVer minimale incluse (ex. `"0.4.0"`).
/// - `to_version` : version SemVer maximale incluse (ex. `"0.5.2"`).
/// - `include_meta` : si `true`, inclut les sections méta (Tests, Internal, …) comme
///   cartes [`KindKind::Task`]. Par défaut `false`.
pub fn parse_changelog(
    text: &str,
    from_version: &str,
    to_version: &str,
    include_meta: bool,
) -> Vec<ChangelogEntry> {
    let mut entries: Vec<ChangelogEntry> = Vec::new();

    // État de la machine à états ligne par ligne.
    let mut current_version: Option<String> = None;
    let mut in_range = false;
    let mut current_section: Option<String> = None;
    // Compteur par section (réinitialisé à chaque nouvelle `###`).
    let mut section_item_idx: usize = 0;

    for line in text.lines() {
        // Détection `## [x.y.z]` — début d'un bloc de version.
        if let Some(rest) = line.strip_prefix("## [") {
            // Extrait la version entre `[` et `]`.
            if let Some(ver_end) = rest.find(']') {
                let ver = &rest[..ver_end];
                if ver.eq_ignore_ascii_case("Unreleased") {
                    current_version = None;
                    in_range = false;
                    current_section = None;
                } else {
                    let ver = ver.to_string();
                    in_range = semver_in_range(&ver, from_version, to_version);
                    current_version = Some(ver);
                    current_section = None;
                    section_item_idx = 0;
                }
            }
            continue;
        }

        // Détection `### Section` — début d'une section (only si dans une version en range).
        if let Some(rest) = line.strip_prefix("### ") {
            let section = rest.trim().to_string();
            current_section = Some(section);
            section_item_idx = 0;
            continue;
        }

        // `#### Sous-titre` — ignoré, les bullets suivants restent dans la section parente.
        if line.starts_with("#### ") {
            continue;
        }

        // Bullets `- ...` — capturer uniquement si en range et section connue.
        if !in_range {
            continue;
        }
        let Some(ref ver) = current_version else {
            continue;
        };
        let Some(ref section) = current_section else {
            continue;
        };
        if !line.starts_with("- ") && !line.starts_with("  - ") {
            continue;
        }
        // Normalise le préfixe (nested bullets `  - ` traités comme `- `).
        let raw = if line.starts_with("  - ") {
            &line[2..]
        } else {
            line
        };

        let title = strip_and_truncate(raw);
        if title.is_empty() {
            continue;
        }

        // Filtre les sections méta si include_meta=false — ADN 4 : zéro bruit dans les cartes.
        if !include_meta && !is_standard_section(section) {
            continue;
        }

        let kind = section_to_kind(section);
        let section_snake = section_to_snake(section);
        let source_marker = format!("changelog/{ver}/{section_snake}/{section_item_idx}");

        entries.push(ChangelogEntry {
            version: ver.clone(),
            section: section.clone(),
            kind,
            title,
            source_marker,
            idx: section_item_idx,
        });

        section_item_idx += 1;
    }

    entries
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGELOG_MINIMAL: &str = r#"
## [0.5.2] - 2026-06-15

### Added

- **vault_write in-place update**: supports note_id + expected_sha256.
- Another added item.

### Fixed

- **Optimistic-lock Conflict not surfaced**: fixed by anti-clobber guard.

### Changed

- **Studio session persistence**: JWT in localStorage.
"#;

    const CHANGELOG_WITH_H4: &str = r#"
## [0.5.2] - 2026-06-15

### Added

#### Code index (`gradatum-admin code ingest` / `gradatum-admin code update`)

- **`gradatum-admin code ingest`**: initial full ingest from a Git repository root.
- **`gradatum-admin code update`**: O(diff) incremental update.

### Fixed

- **Optimistic-lock**: fixed.
"#;

    #[test]
    fn parses_added_entry_as_feature() {
        let entries = parse_changelog(CHANGELOG_MINIMAL, "0.5.2", "0.5.2", false);
        let added: Vec<_> = entries.iter().filter(|e| e.section == "Added").collect();
        assert!(!added.is_empty(), "doit avoir au moins 1 entrée Added");
        for e in &added {
            assert_eq!(e.kind, KindKind::Feature, "Added → Feature");
        }
    }

    #[test]
    fn parses_fixed_entry_as_fix() {
        let entries = parse_changelog(CHANGELOG_MINIMAL, "0.5.2", "0.5.2", false);
        let fixed: Vec<_> = entries.iter().filter(|e| e.section == "Fixed").collect();
        assert!(!fixed.is_empty(), "doit avoir au moins 1 entrée Fixed");
        for e in &fixed {
            assert_eq!(e.kind, KindKind::Fix, "Fixed → Fix");
        }
    }

    #[test]
    fn filters_versions_outside_range() {
        let changelog = r#"
## [0.6.0] - 2026-07-01

### Added

- Out of range item.

## [0.5.2] - 2026-06-15

### Added

- In range item.

## [0.4.0] - 2026-01-01

### Fixed

- Also out of range.
"#;
        let entries = parse_changelog(changelog, "0.5.0", "0.5.9", false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].version, "0.5.2");
    }

    #[test]
    fn fourth_header_bullets_attributed_to_parent_section() {
        // Input: "## [0.5.2]\n### Added\n#### Code index\n- **foo**: bar\n- baz\n"
        // → 2 entries kind=Feature version="0.5.2"
        let entries = parse_changelog(CHANGELOG_WITH_H4, "0.5.2", "0.5.2", false);
        let added: Vec<_> = entries.iter().filter(|e| e.section == "Added").collect();
        assert_eq!(added.len(), 2, "2 bullets sous #### rattachés à ### Added");
        for e in &added {
            assert_eq!(e.kind, KindKind::Feature);
            assert_eq!(e.version, "0.5.2");
        }
    }

    #[test]
    fn source_marker_is_deterministic() {
        let entries1 = parse_changelog(CHANGELOG_MINIMAL, "0.5.2", "0.5.2", false);
        let entries2 = parse_changelog(CHANGELOG_MINIMAL, "0.5.2", "0.5.2", false);
        assert_eq!(entries1.len(), entries2.len());
        for (a, b) in entries1.iter().zip(entries2.iter()) {
            assert_eq!(
                a.source_marker, b.source_marker,
                "marqueur non déterministe"
            );
        }
        // Vérifie le format
        let first = entries1
            .iter()
            .find(|e| e.section == "Added")
            .expect("entrée Added attendue");
        assert!(
            first.source_marker.starts_with("changelog/0.5.2/added/"),
            "marqueur mal formé : {}",
            first.source_marker
        );
    }

    #[test]
    fn strips_markdown_bold_from_title() {
        let changelog = "## [1.0.0] - 2026-01-01\n### Added\n- **vault_write in-place update**: supports note_id.\n";
        let entries = parse_changelog(changelog, "1.0.0", "1.0.0", false);
        assert_eq!(entries.len(), 1);
        // Le titre NE doit PAS contenir `**`
        assert!(
            !entries[0].title.contains("**"),
            "markdown bold non supprimé : {}",
            entries[0].title
        );
        assert!(
            entries[0].title.contains("vault_write in-place update"),
            "contenu attendu absent : {}",
            entries[0].title
        );
    }

    #[test]
    fn title_truncated_at_80_chars() {
        let long_title = "a".repeat(100);
        let changelog = format!("## [1.0.0] - 2026-01-01\n### Added\n- {long_title}\n");
        let entries = parse_changelog(&changelog, "1.0.0", "1.0.0", false);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].title.chars().count(),
            80,
            "titre non tronqué à 80"
        );
    }

    #[test]
    fn unknown_section_maps_to_task() {
        // include_meta=true pour que la section inconnue soit incluse
        let changelog = "## [1.0.0] - 2026-01-01\n### Internal\n- Some internal item.\n";
        let entries = parse_changelog(changelog, "1.0.0", "1.0.0", true);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, KindKind::Task, "section inconnue → Task");
    }

    #[test]
    fn unreleased_section_excluded() {
        let changelog = "## [Unreleased]\n### Added\n- Future item.\n\n## [1.0.0] - 2026-01-01\n### Fixed\n- Real fix.\n";
        let entries = parse_changelog(changelog, "0.0.1", "2.0.0", false);
        // Aucune entrée de [Unreleased]
        let unreleased: Vec<_> = entries
            .iter()
            .filter(|e| e.version.contains("nreleased"))
            .collect();
        assert!(
            unreleased.is_empty(),
            "[Unreleased] ne doit pas produire d'entrée"
        );
        // L'entrée de 1.0.0 doit être présente
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].version, "1.0.0");
    }

    #[test]
    fn changed_section_maps_to_enhancement() {
        let changelog = "## [1.0.0] - 2026-01-01\n### Changed\n- Breaking change description.\n";
        let entries = parse_changelog(changelog, "1.0.0", "1.0.0", false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, KindKind::Enhancement);
    }

    #[test]
    fn security_section_maps_to_fix() {
        let changelog = "## [1.0.0] - 2026-01-01\n### Security\n- CVE-2026-0001 patched.\n";
        let entries = parse_changelog(changelog, "1.0.0", "1.0.0", false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, KindKind::Fix);
    }

    #[test]
    fn idx_increments_per_section() {
        let entries = parse_changelog(CHANGELOG_MINIMAL, "0.5.2", "0.5.2", false);
        let added: Vec<_> = entries.iter().filter(|e| e.section == "Added").collect();
        assert_eq!(added.len(), 2);
        assert_eq!(added[0].idx, 0);
        assert_eq!(added[1].idx, 1);
    }

    #[test]
    fn tests_section_skipped_by_default() {
        let text = "## [0.4.0] - 2025-01-01\n### Tests\n- Workspace: 1407 tests pass\n";
        let entries = parse_changelog(text, "0.4.0", "0.4.0", false);
        assert!(
            entries.is_empty(),
            "section Tests doit être skippée avec include_meta=false, got {} entries",
            entries.len()
        );
    }

    #[test]
    fn tests_section_included_with_include_meta() {
        let text = "## [0.4.0] - 2025-01-01\n### Tests\n- Workspace: 1407 tests pass\n";
        let entries = parse_changelog(text, "0.4.0", "0.4.0", true);
        assert_eq!(
            entries.len(),
            1,
            "section Tests doit être incluse avec include_meta=true"
        );
        assert_eq!(entries[0].kind, KindKind::Task);
    }

    #[test]
    fn standard_sections_always_included() {
        // 8 sections standard + 1 bullet chacune
        let text = "## [0.4.0] - 2025-01-01\n\
            ### Added\n- feat A\n\
            ### Changed\n- change B\n\
            ### Fixed\n- fix C\n\
            ### Security\n- sec D\n\
            ### Removed\n- rem E\n\
            ### Deprecated\n- dep F\n\
            ### Performance\n- perf G\n\
            ### Privacy\n- priv H\n";
        let entries = parse_changelog(text, "0.4.0", "0.4.0", false);
        assert_eq!(
            entries.len(),
            8,
            "les 8 sections standard doivent toutes être incluses avec include_meta=false, got {}",
            entries.len()
        );
    }
}
