//! Renders valid project-map cards from CHANGELOG entries.
//!
//! Transforms a `ChangelogEntry` into a `VaultWriteCard` conforming to the
//! project-map schema: forced typed wikilinks `[[project:gradatum]]` +
//! `[[status:OPEN]]` + `[[kind:{KIND}]]` + `[[version:gradatum/{ver}]]`,
//! an HTML comment source marker, and a canonical title.
//!
//! ## Body format
//!
//! ```text
//! [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[version:gradatum/0.5.2]]
//!
//! vault_write in-place update
//!
//! <!-- pm-source: changelog/0.5.2/added/0 -->
//! ```
//!
//! ## Title format
//!
//! `[PROJECT-MAP][gradatum] {title ≤80c} — v{ver}`
//!
//! ## Schema conformance
//!
//! The body contains exactly 1 `[[project:…]]`, 1 `[[status:…]]`, 1 `[[kind:…]]`
//! and 1 `[[version:…]]` — satisfying the required triple plus the optional
//! version link as validated by [`gradatum_core::project_map::validate_links_from_targets`].

use std::collections::HashMap;

use serde::Serialize;

use crate::changelog_parse::ChangelogEntry;

/// Normal form of a card title, used to detect duplicates.
///
/// Two titles that differ only by letter case or by runs of whitespace denote the same
/// card. The normal form lowercases (Unicode-aware, so accented titles fold correctly)
/// and collapses every whitespace run into a single space, trimming both ends.
///
/// The guard must never be stricter than the measurement that motivated it: the registry
/// clean-up counted duplicates under exactly this normal form, so a stricter comparison
/// would let through what the measurement had counted.
#[must_use]
pub fn normalize_title(title: &str) -> String {
    title
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Indexes existing cards by the normal form of their title.
///
/// Takes the `(locus, title)` pairs reported by
/// [`crate::changelog_backfill::VaultWriteClient::existing_titles`] and keys them by
/// [`normalize_title`], so a back-fill can tell whether a card bearing the same title is
/// already present — and name it.
///
/// When several existing cards collapse onto the same normal form, the first one wins;
/// naming any one of them is enough for an operator to find the collision.
#[must_use]
pub fn build_title_index(entries: Vec<(String, String)>) -> HashMap<String, String> {
    let mut index = HashMap::with_capacity(entries.len());
    for (locus, title) in entries {
        index.entry(normalize_title(&title)).or_insert(locus);
    }
    index
}

/// Payload ready to be sent to `POST /api/v1/vault_write`.
#[derive(Debug, Clone, Serialize)]
pub struct VaultWriteCard {
    /// Note title, shaped as `[PROJECT-MAP][gradatum] {title} — v{version}`.
    pub title: String,
    /// Markdown body carrying the typed wikilinks and the source marker.
    pub body: String,
    /// Tags, used to make the card searchable.
    pub tags: Vec<String>,
    /// Destination section, always `"project-map"`.
    pub section_hint: String,
}

/// Generates a project-map card from a CHANGELOG entry.
///
/// Body format:
/// ```text
/// [[project:gradatum]] [[status:OPEN]] [[kind:{KIND}]] [[version:gradatum/{ver}]]
///
/// {cleaned full title}
///
/// <!-- pm-source: changelog/{ver}/{section_snake}/{idx} -->
/// ```
///
/// Title format: `[PROJECT-MAP][gradatum] {title ≤80c} — v{ver}`
///
/// # Schema conformance
///
/// The body contains all 4 required typed wikilinks (project + status + kind +
/// version) and passes [`gradatum_core::project_map::validate_links_from_targets`].
#[must_use]
pub fn render_card(entry: &ChangelogEntry) -> VaultWriteCard {
    let kind_wire = entry.kind.as_wire();
    // Escape les `[[` dans le texte du titre pour éviter que le validateur compte
    // des liens supplémentaires (cardinality rejection, spec §5).
    // Les 4 wikilinks typés forcés sont écrits APRÈS ce titre escapé — ils restent intacts.
    let escaped_title = entry.title.replace("[[", "[ [");
    let body = format!(
        "[[project:gradatum]] [[status:OPEN]] [[kind:{kind_wire}]] [[version:gradatum/{ver}]]\n\n{title}\n\n<!-- pm-source: {marker} -->",
        ver = entry.version,
        title = escaped_title,
        marker = entry.source_marker,
    );
    let title = format!(
        "[PROJECT-MAP][gradatum] {} — v{}",
        entry.title, entry.version
    );
    VaultWriteCard {
        title,
        body,
        tags: vec![
            "project-map".to_string(),
            format!("v{}", entry.version),
            entry.kind.as_wire().to_ascii_lowercase(),
        ],
        section_hint: "project-map".to_string(),
    }
}

/// Extrait les cibles de wikilinks `[[…]]` d'un texte markdown (usage test).
///
/// Ne capture que ce qui est entre `[[` et `]]`. Fonction locale aux tests —
/// l'extraction production est faite par `gradatum_curator::wikilinks`.
#[cfg(test)]
pub(crate) fn extract_wikilink_targets(body: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after_open = &rest[start + 2..];
        if let Some(end) = after_open.find("]]") {
            targets.push(after_open[..end].to_string());
            rest = &after_open[end + 2..];
        } else {
            break;
        }
    }
    targets
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use gradatum_core::project_map::{KindKind, validate_links_from_targets};

    use crate::changelog_parse::{ChangelogEntry, parse_changelog};

    use super::*;

    fn make_entry(version: &str, kind: KindKind, title: &str, idx: usize) -> ChangelogEntry {
        ChangelogEntry {
            version: version.to_string(),
            section: "Added".to_string(),
            kind,
            title: title.to_string(),
            source_marker: format!("changelog/{version}/added/{idx}"),
            idx,
        }
    }

    #[test]
    fn card_body_contains_all_four_required_links() {
        let entry = make_entry("0.5.2", KindKind::Feature, "vault_write in-place update", 0);
        let card = render_card(&entry);
        let targets = extract_wikilink_targets(&card.body);
        // Appel obligatoire : validate_links_from_targets
        assert_eq!(
            validate_links_from_targets(&targets),
            Ok(()),
            "body non conforme au validateur : {:?}\ntargets = {:?}",
            card.body,
            targets
        );
    }

    #[test]
    fn card_body_contains_source_marker() {
        let entry = make_entry("0.5.2", KindKind::Fix, "Optimistic-lock fix", 0);
        let card = render_card(&entry);
        assert!(
            card.body.contains("<!-- pm-source: changelog/"),
            "marqueur source absent du body : {}",
            card.body
        );
        assert!(
            card.body.contains(&entry.source_marker),
            "marqueur source incorrect : attendu '{}' dans '{}'",
            entry.source_marker,
            card.body
        );
    }

    #[test]
    fn card_title_format_is_correct() {
        let entry = make_entry("0.5.2", KindKind::Feature, "vault_write in-place update", 0);
        let card = render_card(&entry);
        assert!(
            card.title.starts_with("[PROJECT-MAP][gradatum] "),
            "title ne commence pas par [PROJECT-MAP][gradatum] : {}",
            card.title
        );
        assert!(
            card.title.contains("— v"),
            "title ne contient pas '— v' : {}",
            card.title
        );
        assert!(
            card.title.contains("0.5.2"),
            "title ne contient pas la version : {}",
            card.title
        );
    }

    #[test]
    fn card_section_hint_is_project_map() {
        let entry = make_entry("0.5.2", KindKind::Task, "Dependency cleanup", 0);
        let card = render_card(&entry);
        assert_eq!(card.section_hint, "project-map");
    }

    #[test]
    fn card_validator_conformant_for_each_kind() {
        // Pour chaque KindKind : générer une carte, valider les wikilinks
        let kinds = [
            KindKind::Feature,
            KindKind::Enhancement,
            KindKind::Fix,
            KindKind::Task,
        ];
        for kind in kinds {
            let entry = make_entry("0.6.0", kind, "Test item", 0);
            let card = render_card(&entry);
            let targets = extract_wikilink_targets(&card.body);
            assert_eq!(
                validate_links_from_targets(&targets),
                Ok(()),
                "body non conforme pour kind={:?} : {:?}\ntargets = {:?}",
                kind,
                card.body,
                targets
            );
        }
    }

    #[test]
    fn card_body_contains_kind_wire() {
        let entry = make_entry(
            "0.5.2",
            KindKind::Enhancement,
            "Studio session persistence",
            0,
        );
        let card = render_card(&entry);
        assert!(
            card.body.contains("[[kind:ENHANCEMENT]]"),
            "kind wire absent du body : {}",
            card.body
        );
    }

    #[test]
    fn card_body_contains_version_link() {
        let entry = make_entry("0.5.2", KindKind::Feature, "vault_timeline", 0);
        let card = render_card(&entry);
        assert!(
            card.body.contains("[[version:gradatum/0.5.2]]"),
            "lien version absent du body : {}",
            card.body
        );
    }

    #[test]
    fn wikilink_in_bullet_text_is_escaped_before_card_render() {
        // Un titre contenant [[status:DONE]] ajouterait un 2e lien status → rejet validateur.
        let entry = make_entry(
            "0.5.2",
            KindKind::Feature,
            "See [[status:DONE]] for details",
            0,
        );
        let card = render_card(&entry);
        // Le body doit contenir "[ [status:DONE]]" (double crochet escapé dans le titre)
        assert!(
            card.body.contains("[ [status:DONE]]"),
            "le [[ du titre doit être escapé dans le body, got: {}",
            card.body
        );
        // Et ne doit contenir qu'un seul lien status: (le forcé [[status:OPEN]])
        let count = card.body.matches("[[status:").count();
        assert_eq!(
            count, 1,
            "exactement 1 lien status forcé attendu, got {} — body: {}",
            count, card.body
        );
        // Le validateur doit passer
        let targets = extract_wikilink_targets(&card.body);
        assert_eq!(
            validate_links_from_targets(&targets),
            Ok(()),
            "body non conforme au validateur après escape : targets = {:?}\nbody = {}",
            targets,
            card.body
        );
    }

    #[test]
    fn card_produced_from_parse_changelog_is_validator_conformant() {
        // Test d'intégration : parse → render → validate
        let changelog = "## [0.5.2] - 2026-06-15\n### Added\n- **vault_write**: in-place update.\n### Fixed\n- **Conflict**: fixed.\n";
        let entries = parse_changelog(changelog, "0.5.2", "0.5.2", false);
        assert!(
            !entries.is_empty(),
            "parse_changelog doit retourner des entrées"
        );
        for entry in &entries {
            let card = render_card(entry);
            let targets = extract_wikilink_targets(&card.body);
            assert_eq!(
                validate_links_from_targets(&targets),
                Ok(()),
                "intégration parse→render non conforme pour entry {:?}",
                entry
            );
        }
    }

    // ── Forme normale du titre ─────────────────────────────────────────────

    #[test]
    fn normalize_title_folds_case_and_whitespace_runs() {
        assert_eq!(
            normalize_title("  [PROJECT-MAP][Gradatum]   Vault   Write  "),
            normalize_title("[project-map][gradatum] vault write")
        );
    }

    #[test]
    fn normalize_title_folds_accented_letters() {
        // `to_lowercase` est Unicode : un titre français crié se replie correctement.
        assert_eq!(
            normalize_title("RÉSOLUTION Différée"),
            "résolution différée"
        );
    }

    #[test]
    fn normalize_title_keeps_distinct_titles_distinct() {
        assert_ne!(
            normalize_title("[PROJECT-MAP][gradatum] Vault write — v0.5.2"),
            normalize_title("[PROJECT-MAP][gradatum] Vault write — v0.5.3"),
            "le suffixe de version distingue deux cartes légitimes"
        );
    }

    #[test]
    fn build_title_index_keys_by_normal_form_and_keeps_the_first() {
        let index = build_title_index(vec![
            ("project-map/aaa".to_string(), "Vault   Write".to_string()),
            ("project-map/bbb".to_string(), "vault write".to_string()),
        ]);
        assert_eq!(index.len(), 1, "les deux titres se replient sur une clé");
        assert_eq!(
            index.get("vault write").map(String::as_str),
            Some("project-map/aaa"),
            "la première carte rencontrée est celle qui est nommée"
        );
    }
}
