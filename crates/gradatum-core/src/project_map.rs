//! project-map — typed-wikilink schema for traceable work units.
//!
//! A note in section [`Section::ProjectMap`](crate::section::Section::ProjectMap)
//! is a **traceable work unit** (feature/fix/task…) whose lifecycle state is
//! carried by a **typed wikilink schema** instead of duplicated frontmatter fields.
//! This module provides **pure parsing** (`[[role:value]]` → [`ProjectMapLink`])
//! and **cardinality validation** ([`validate_links`]), with no I/O.
//!
//! ## Link convention
//!
//! The "type" of a link is its **reserved prefix** before `:`. Reserved prefixes
//! are: `project` · `status` · `kind` · `version` · `spec` · `plan` · `context`.
//! Any other prefix (e.g. `decisions:`) is a **content dependency**
//! ([`ProjectMapLink::Dep`]) that reuses the existing `[[section:ULID]]` format
//! without regression.
//!
//! | Role | Form | Variant |
//! |---|---|---|
//! | project | `[[project:gradatum]]` | [`ProjectMapLink::Project`] |
//! | status | `[[status:DONE]]` | [`ProjectMapLink::Status`] |
//! | kind | `[[kind:FIX]]` | [`ProjectMapLink::Kind`] |
//! | version | `[[version:gradatum/0.6.1]]` | [`ProjectMapLink::Version`] |
//! | annex | `[[spec:…]]` `[[plan:…]]` `[[context:…]]` | [`ProjectMapLink::Annex`] |
//! | dependency | `[[decisions:01K…]]` | [`ProjectMapLink::Dep`] |
//!
//! ## Case sensitivity
//!
//! `status` and `kind` values are normalised to **SCREAMING_SNAKE** on the wire
//! to prevent case-sensitive bugs. `[[status:done]]` is rejected; only
//! `[[status:DONE]]` is accepted.
//!
//! ## Validation design
//!
//! - The project-map validator ([`validate_links`]) is **dedicated** to this schema
//!   and does not invoke a generic schema-registry subsystem.

use thiserror::Error;

/// Lifecycle status of a project-map work unit.
///
/// Wire values are SCREAMING_SNAKE-cased (e.g. `"IN_PROGRESS"`).
/// `BRAINSTORMING` is the upstream ideation state; `DONE` marks completion;
/// `OBSOLETE` marks abandonment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StatusKind {
    /// Réflexion amont, pas encore engagée.
    Brainstorming,
    /// Engagée, pas démarrée.
    Open,
    /// En cours.
    InProgress,
    /// Bloquée par une dépendance.
    Blocked,
    /// Terminée (≡ RESOLVED gov-todo).
    Done,
    /// Abandonnée / périmée.
    Obsolete,
}

impl StatusKind {
    /// Parse the SCREAMING_SNAKE wire value of a `[[status:…]]` wikilink.
    ///
    /// Matching is case-sensitive: `"DONE"` is accepted; `"done"` and `"Done"` are rejected.
    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "BRAINSTORMING" => Some(Self::Brainstorming),
            "OPEN" => Some(Self::Open),
            "IN_PROGRESS" => Some(Self::InProgress),
            "BLOCKED" => Some(Self::Blocked),
            "DONE" => Some(Self::Done),
            "OBSOLETE" => Some(Self::Obsolete),
            _ => None,
        }
    }

    /// Returns the SCREAMING_SNAKE wire representation (inverse of `from_wire`).
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::Brainstorming => "BRAINSTORMING",
            Self::Open => "OPEN",
            Self::InProgress => "IN_PROGRESS",
            Self::Blocked => "BLOCKED",
            Self::Done => "DONE",
            Self::Obsolete => "OBSOLETE",
        }
    }
}

/// Nature of a project-map work unit — drives CHANGELOG categorisation.
///
/// Wire values are SCREAMING_SNAKE-cased. `CHORE` and `SPIKE` are provided as
/// distinct kinds so that `TASK` does not become a catch-all that degrades
/// Keep-a-Changelog grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KindKind {
    /// Nouvelle capacité (→ CHANGELOG « Added »).
    Feature,
    /// Amélioration d'existant (→ « Changed »).
    Enhancement,
    /// Correction de bug (→ « Fixed »).
    Fix,
    /// Maintenance / outillage (→ hors changelog public usuel).
    Chore,
    /// Exploration / prototype borné.
    Spike,
    /// Tâche générique (fourre-tout assumé).
    Task,
}

impl KindKind {
    /// Parse la valeur wire SCREAMING_SNAKE d'un `[[kind:…]]`.
    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "FEATURE" => Some(Self::Feature),
            "ENHANCEMENT" => Some(Self::Enhancement),
            "FIX" => Some(Self::Fix),
            "CHORE" => Some(Self::Chore),
            "SPIKE" => Some(Self::Spike),
            "TASK" => Some(Self::Task),
            _ => None,
        }
    }

    /// Représentation wire SCREAMING_SNAKE.
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::Feature => "FEATURE",
            Self::Enhancement => "ENHANCEMENT",
            Self::Fix => "FIX",
            Self::Chore => "CHORE",
            Self::Spike => "SPIKE",
            Self::Task => "TASK",
        }
    }
}

/// Role of an annex link — detailed material **referenced, never duplicated**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AnnexRole {
    /// Spécification de design (`[[spec:…]]`).
    Spec,
    /// Plan d'implémentation (`[[plan:…]]`).
    Plan,
    /// Contexte / discussion (`[[context:…]]`).
    Context,
}

impl AnnexRole {
    /// Préfixe réservé associé au rôle annexe.
    #[must_use]
    pub const fn prefix(&self) -> &'static str {
        match self {
            Self::Spec => "spec",
            Self::Plan => "plan",
            Self::Context => "context",
        }
    }
}

/// Un lien typé d'une carte project-map, issu d'un wikilink `[[role:valeur]]`.
///
/// Produit par [`parse_link`]. La cardinalité (1 `Project`, 1 `Status`, 1 `Kind`,
/// ≤1 `Version`, N `Annex`, N `Dep`) est vérifiée par [`validate_links`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProjectMapLink {
    /// `[[project:<nom>]]` — projet rattaché (1 obligatoire).
    Project(String),
    /// `[[status:<STATUS>]]` — statut de cycle de vie (1 obligatoire).
    Status(StatusKind),
    /// `[[kind:<KIND>]]` — nature de l'unité (1 obligatoire).
    Kind(KindKind),
    /// `[[version:<projet>/<x.y.z>]]` — version cible (0 ou 1).
    Version {
        /// Projet namespacé (partie avant le `/`).
        project: String,
        /// Numéro de version (partie après le `/`).
        version: String,
    },
    /// `[[spec|plan|context:<cible>]]` — annexe référencée (0..N).
    Annex {
        /// Rôle de l'annexe.
        role: AnnexRole,
        /// Cible brute (ULID ou chemin).
        target: String,
    },
    /// `[[<section>:<ULID>]]` — dépendance de contenu existante (0..N).
    Dep {
        /// Section de la note cible.
        section: String,
        /// ULID de la note cible.
        ulid: String,
    },
}

/// Schema error for project-map parsing or cardinality validation.
///
/// This error type is **dedicated** to the project-map schema and is distinct
/// from any generic schema-registry validation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SchemaError {
    /// Lien sans préfixe `role:` et non reconnu comme dépendance `section:ULID`.
    #[error("lien sans préfixe typé ni format section:ULID : {0:?}")]
    MissingPrefix(String),

    /// Valeur de statut hors taxonomie (`[[status:…]]`).
    #[error(
        "statut invalide {0:?} (attendu SCREAMING_SNAKE ∈ BRAINSTORMING/OPEN/IN_PROGRESS/BLOCKED/DONE/OBSOLETE)"
    )]
    InvalidStatus(String),

    /// Valeur de kind hors taxonomie (`[[kind:…]]`).
    #[error(
        "kind invalide {0:?} (attendu SCREAMING_SNAKE ∈ FEATURE/ENHANCEMENT/FIX/CHORE/SPIKE/TASK)"
    )]
    InvalidKind(String),

    /// `[[version:…]]` mal formé (attendu `projet/x.y.z`).
    #[error("version mal formée {0:?} (attendu projet/x.y.z)")]
    MalformedVersion(String),

    /// Valeur vide après le préfixe (ex. `[[project:]]`).
    #[error("valeur vide pour le préfixe {0:?}")]
    EmptyValue(String),

    /// Valeur project/version dépasse 64 caractères.
    #[error("valeur {1:?} trop longue pour le préfixe {0:?} (max 64 chars, reçu {2})")]
    ValueTooLong(String, String, usize),

    /// Valeur project/version contient des caractères non autorisés (`[a-z0-9._-]`).
    #[error(
        "valeur {1:?} contient des caractères interdits pour le préfixe {0:?} (autorisé : a-z 0-9 . _ -)"
    )]
    InvalidChars(String, String),

    /// `project:` manquant ou multiple (exactement 1 requis).
    #[error("exactement 1 lien project: requis, trouvé {0}")]
    ProjectCardinality(usize),

    /// `status:` manquant ou multiple (exactement 1 requis).
    #[error("exactement 1 lien status: requis, trouvé {0}")]
    StatusCardinality(usize),

    /// `kind:` manquant ou multiple (exactement 1 requis).
    #[error("exactement 1 lien kind: requis, trouvé {0}")]
    KindCardinality(usize),

    /// `version:` multiple (au plus 1).
    #[error("au plus 1 lien version: autorisé, trouvé {0}")]
    VersionCardinality(usize),

    /// Incohérence : `version:<projet>/…` ≠ `project:<projet>`.
    #[error("version namespacée {version_project:?} ≠ project {project:?}")]
    VersionProjectMismatch {
        /// Projet déclaré par `[[project:…]]`.
        project: String,
        /// Projet namespacé dans `[[version:…]]`.
        version_project: String,
    },
}

/// Longueur maximale pour un identifiant `project:` ou composant de `version:`.
///
/// Cohérent avec `Tag::normalize` (64 chars). Safety cap DoS (ADN 5).
const MAX_IDENT_LEN: usize = 64;

/// Valide qu'un identifiant project-map respecte le charset `[a-z0-9._-]` et
/// la longueur maximale [`MAX_IDENT_LEN`].
///
/// # Errors
///
/// - [`SchemaError::ValueTooLong`] si `value.len() > MAX_IDENT_LEN`.
/// - [`SchemaError::InvalidChars`] si un caractère n'est pas dans `[a-z0-9._-]`.
fn validate_ident(prefix: &str, value: &str) -> Result<(), SchemaError> {
    if value.len() > MAX_IDENT_LEN {
        return Err(SchemaError::ValueTooLong(
            prefix.to_string(),
            value.to_string(),
            value.len(),
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(SchemaError::InvalidChars(
            prefix.to_string(),
            value.to_string(),
        ));
    }
    Ok(())
}

/// Découpe une cible brute `role:valeur` au **premier** `:`.
///
/// Retourne `(prefix, value)`. Si aucun `:`, `prefix` vaut la chaîne entière et
/// `value` est vide — le caller décide.
fn split_prefix(target: &str) -> (&str, &str) {
    match target.split_once(':') {
        Some((p, v)) => (p, v),
        None => (target, ""),
    }
}

/// Parse a wikilink target `[[role:value]]` (without the surrounding `[[ ]]`) into a [`ProjectMapLink`].
///
/// `raw` is the **already-extracted** target (e.g. `"status:DONE"`), as returned
/// by `gradatum_curator::wikilinks::extract_wikilinks`.
///
/// ## Disambiguation rule
///
/// - A **reserved prefix** (`project`/`status`/`kind`/`version`/`spec`/`plan`/`context`)
///   → typed structural or annex link.
/// - Any other prefix in `<section>:<ULID>` form → [`ProjectMapLink::Dep`].
///
/// # Errors
///
/// - [`SchemaError::EmptyValue`] si la valeur après un préfixe réservé est vide.
/// - [`SchemaError::InvalidStatus`] / [`SchemaError::InvalidKind`] hors taxonomie.
/// - [`SchemaError::MalformedVersion`] si `version:` n'est pas `projet/x.y.z`.
/// - [`SchemaError::MissingPrefix`] si la cible n'a pas de préfixe `role:` et
///   n'est pas un `section:ULID` valide.
#[must_use = "le résultat de parsing doit être inspecté"]
pub fn parse_link(raw: &str) -> Result<ProjectMapLink, SchemaError> {
    let raw = raw.trim();
    let (prefix, value) = split_prefix(raw);

    // Préfixes réservés : valeur obligatoire et non vide.
    let reserved = matches!(
        prefix,
        "project" | "status" | "kind" | "version" | "spec" | "plan" | "context"
    );
    if reserved && value.is_empty() {
        return Err(SchemaError::EmptyValue(prefix.to_string()));
    }

    match prefix {
        "project" => {
            validate_ident("project", value)?;
            Ok(ProjectMapLink::Project(value.to_string()))
        }
        "status" => StatusKind::from_wire(value)
            .map(ProjectMapLink::Status)
            .ok_or_else(|| SchemaError::InvalidStatus(value.to_string())),
        "kind" => KindKind::from_wire(value)
            .map(ProjectMapLink::Kind)
            .ok_or_else(|| SchemaError::InvalidKind(value.to_string())),
        "version" => parse_version(value),
        "spec" => Ok(ProjectMapLink::Annex {
            role: AnnexRole::Spec,
            target: value.to_string(),
        }),
        "plan" => Ok(ProjectMapLink::Annex {
            role: AnnexRole::Plan,
            target: value.to_string(),
        }),
        "context" => Ok(ProjectMapLink::Annex {
            role: AnnexRole::Context,
            target: value.to_string(),
        }),
        // Préfixe non réservé : dépendance de contenu section:ULID.
        other => parse_dep(other, value, raw),
    }
}

/// Parse the value of a `[[version:<project>/<x.y.z>]]` wikilink.
///
/// The inner `/` separates the project namespace from the version string,
/// avoiding collision with the `:` prefix delimiter.
fn parse_version(value: &str) -> Result<ProjectMapLink, SchemaError> {
    // Exactement un `/` séparant projet et version, les deux non vides.
    match value.split_once('/') {
        Some((project, version)) if !project.is_empty() && !version.is_empty() => {
            validate_ident("version.project", project)?;
            validate_ident("version.version", version)?;
            Ok(ProjectMapLink::Version {
                project: project.to_string(),
                version: version.to_string(),
            })
        }
        _ => Err(SchemaError::MalformedVersion(value.to_string())),
    }
}

/// Parse une dépendance `section:ULID` (préfixe non réservé).
///
/// `raw` sert au message d'erreur si la cible n'a pas de préfixe du tout.
fn parse_dep(section: &str, ulid: &str, raw: &str) -> Result<ProjectMapLink, SchemaError> {
    if ulid.is_empty() {
        // Pas de `:` dans la cible (ou rien après) → ni typé, ni section:ULID.
        return Err(SchemaError::MissingPrefix(raw.to_string()));
    }
    Ok(ProjectMapLink::Dep {
        section: section.to_string(),
        ulid: ulid.to_string(),
    })
}

/// Validates the cardinality of a set of project-map links.
///
/// Rule: exactly 1 `Project`, 1 `Status`, 1 `Kind`; at most 1 `Version`;
/// any number of `Annex` and `Dep`. If a `Version` is present, its namespaced
/// project must match the `Project` link.
///
/// # Errors
///
/// Une [`SchemaError`] de cardinalité ou de cohérence au premier écart constaté
/// (ordre : project → status → kind → version → mismatch).
#[must_use = "le résultat de validation doit être inspecté avant d'accepter l'écriture"]
pub fn validate_links(links: &[ProjectMapLink]) -> Result<(), SchemaError> {
    let mut projects: Vec<&str> = Vec::new();
    let mut status_count = 0usize;
    let mut kind_count = 0usize;
    let mut versions: Vec<&str> = Vec::new();

    for link in links {
        match link {
            ProjectMapLink::Project(p) => projects.push(p),
            ProjectMapLink::Status(_) => status_count += 1,
            ProjectMapLink::Kind(_) => kind_count += 1,
            ProjectMapLink::Version { project, .. } => versions.push(project),
            ProjectMapLink::Annex { .. } | ProjectMapLink::Dep { .. } => {}
        }
    }

    if projects.len() != 1 {
        return Err(SchemaError::ProjectCardinality(projects.len()));
    }
    if status_count != 1 {
        return Err(SchemaError::StatusCardinality(status_count));
    }
    if kind_count != 1 {
        return Err(SchemaError::KindCardinality(kind_count));
    }
    if versions.len() > 1 {
        return Err(SchemaError::VersionCardinality(versions.len()));
    }

    // Cohérence version.project == project (si une version est présente).
    let project = projects[0];
    if let Some(version_project) = versions.first()
        && *version_project != project
    {
        return Err(SchemaError::VersionProjectMismatch {
            project: project.to_string(),
            version_project: (*version_project).to_string(),
        });
    }

    Ok(())
}

/// Valide le schéma project-map à partir des cibles de wikilinks **déjà extraites**.
///
/// Conçu pour le write-path : le serveur extrait les cibles `[[…]]` du body (via
/// `gradatum_curator::wikilinks::extract_wikilinks`, hors de cette crate pour
/// éviter un cycle de dépendances) puis délègue ici la validation.
///
/// # Sémantique
///
/// Chaque cible est passée à [`parse_link`]. Les cibles qui **échouent** au parse
/// (titre humain nu, prose `[[Voir aussi]]`…) sont **ignorées** : ce ne sont pas
/// des liens structurels project-map. Les cibles qui parsent sont collectées puis
/// soumises à [`validate_links`] (cardinalité 1 project + 1 status + 1 kind, ≤1
/// version, cohérence version.project).
///
/// Une valeur **réservée mal formée** (`[[status:nope]]`, `[[version:x]]`) parse
/// en `Err` et serait donc ignorée ici ; le triple-obligatoire la rattrape
/// néanmoins (un `status:` invalide ne fournit pas le `Status` requis → rejet par
/// cardinalité). Voir tests.
///
/// # Errors
///
/// Une [`SchemaError`] de cardinalité/cohérence si le schéma obligatoire n'est
/// pas satisfait par les liens typés présents.
#[must_use = "le résultat de validation gate l'écriture project-map"]
pub fn validate_links_from_targets(targets: &[String]) -> Result<(), SchemaError> {
    let links: Vec<ProjectMapLink> = targets.iter().filter_map(|t| parse_link(t).ok()).collect();
    validate_links(&links)
}

/// Identifiant canonique d'un **nœud-cible réservé** synthétique pour le graphe.
///
/// Les liens typés `project:` / `status:` / `kind:` / `version:` ne pointent PAS
/// vers une note existante (pas d'ULID à résoudre) : ils référencent un **nœud
/// réservé** (hub) du graphe `note_links`. Cette fonction normalise la cible
/// brute d'un wikilink en l'identifiant `dst_note_id` canonique de ce nœud, que
/// le resolver worker insère directement sans lookup réseau.
///
/// `note_links.dst_note_id` est un `TEXT` libre (pas de FK, cf. migration 0002),
/// ce qui autorise un `dst` synthétique navigable par `vault_graph`/`vault_trace`.
///
/// # Différence avec les annexes
///
/// `spec:` / `plan:` / `context:` sont des préfixes réservés MAIS pointent vers
/// de **vraies notes** (ULID/chemin) — ils restent résolus par le flux ULID
/// existant et ne sont donc PAS des nœuds réservés ici → `None`.
///
/// # Retour
///
/// - `Some(dst)` pour `project`/`status`/`kind`/`version` bien formés, où `dst`
///   est l'identifiant canonique du nœud (statut/kind normalisés en wire
///   SCREAMING_SNAKE via [`parse_link`]).
/// - `None` pour tout autre lien (annexe, dépendance `section:ULID`, titre nu,
///   ou valeur réservée mal formée) → le caller applique son flux normal.
#[must_use]
pub fn reserved_node_target(raw: &str) -> Option<String> {
    match parse_link(raw).ok()? {
        ProjectMapLink::Project(p) => Some(format!("project:{p}")),
        ProjectMapLink::Status(s) => Some(format!("status:{}", s.as_wire())),
        ProjectMapLink::Kind(k) => Some(format!("kind:{}", k.as_wire())),
        ProjectMapLink::Version { project, version } => {
            Some(format!("version:{project}/{version}"))
        }
        // Annexes (spec/plan/context) et dépendances (section:ULID) pointent vers
        // de vraies notes → résolution ULID normale, pas un nœud réservé.
        ProjectMapLink::Annex { .. } | ProjectMapLink::Dep { .. } => None,
    }
}

#[cfg(test)]
mod validate_from_targets_tests {
    use super::*;

    fn t(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn valid_triple_with_prose_and_deps_passes() {
        let targets = t(&[
            "project:gradatum",
            "status:IN_PROGRESS",
            "kind:FEATURE",
            "Mon Titre Humain", // prose ignorée
            "decisions:01KVBTMYNK4XXZJAKWMTB4AM9K",
        ]);
        assert_eq!(validate_links_from_targets(&targets), Ok(()));
    }

    #[test]
    fn missing_status_is_rejected() {
        let targets = t(&["project:gradatum", "kind:FIX"]);
        assert_eq!(
            validate_links_from_targets(&targets),
            Err(SchemaError::StatusCardinality(0))
        );
    }

    #[test]
    fn invalid_status_value_fails_via_cardinality() {
        // status:nope parse en Err → ignoré → 0 Status → rejet par cardinalité.
        let targets = t(&["project:gradatum", "status:nope", "kind:FIX"]);
        assert_eq!(
            validate_links_from_targets(&targets),
            Err(SchemaError::StatusCardinality(0))
        );
    }

    #[test]
    fn empty_targets_is_rejected_no_project() {
        assert_eq!(
            validate_links_from_targets(&[]),
            Err(SchemaError::ProjectCardinality(0))
        );
    }
}

#[cfg(test)]
mod reserved_node_tests {
    use super::*;

    #[test]
    fn status_maps_to_canonical_reserved_node() {
        assert_eq!(
            reserved_node_target("status:DONE"),
            Some("status:DONE".to_string())
        );
    }

    #[test]
    fn project_and_kind_and_version_map_to_reserved_nodes() {
        assert_eq!(
            reserved_node_target("project:gradatum"),
            Some("project:gradatum".to_string())
        );
        assert_eq!(
            reserved_node_target("kind:FIX"),
            Some("kind:FIX".to_string())
        );
        assert_eq!(
            reserved_node_target("version:gradatum/0.6.1"),
            Some("version:gradatum/0.6.1".to_string())
        );
    }

    #[test]
    fn status_node_is_normalised_to_wire_casing() {
        // Une casse invalide n'est pas un nœud réservé (parse_link rejette).
        assert_eq!(reserved_node_target("status:done"), None);
    }

    #[test]
    fn annexes_are_not_reserved_nodes() {
        // spec/plan/context pointent vers de vraies notes → flux ULID normal.
        assert_eq!(
            reserved_node_target("spec:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            None
        );
        assert_eq!(reserved_node_target("plan:plans/x.md"), None);
        assert_eq!(
            reserved_node_target("context:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            None
        );
    }

    #[test]
    fn dependency_section_ulid_is_not_a_reserved_node_unregressed() {
        // Rétrocompat dure : [[decisions:ULID]] reste une dépendance résolue par
        // ULID, jamais un nœud réservé.
        assert_eq!(
            reserved_node_target("decisions:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            None
        );
    }

    #[test]
    fn human_title_is_not_a_reserved_node() {
        assert_eq!(reserved_node_target("Mon Titre Humain"), None);
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn project_prefix_parses_to_project() {
        assert_eq!(
            parse_link("project:gradatum"),
            Ok(ProjectMapLink::Project("gradatum".to_string()))
        );
    }

    #[test]
    fn status_done_parses_to_status_done() {
        assert_eq!(
            parse_link("status:DONE"),
            Ok(ProjectMapLink::Status(StatusKind::Done))
        );
    }

    #[test]
    fn status_lowercase_is_rejected_case_sensitive() {
        // Spec §15 A4 : SCREAMING_SNAKE strict.
        assert_eq!(
            parse_link("status:done"),
            Err(SchemaError::InvalidStatus("done".to_string()))
        );
    }

    #[test]
    fn status_unknown_value_is_rejected() {
        assert_eq!(
            parse_link("status:NOPE"),
            Err(SchemaError::InvalidStatus("NOPE".to_string()))
        );
    }

    #[test]
    fn kind_fix_parses_to_kind_fix() {
        assert_eq!(
            parse_link("kind:FIX"),
            Ok(ProjectMapLink::Kind(KindKind::Fix))
        );
    }

    #[test]
    fn kind_chore_and_spike_are_accepted() {
        // Spec §15 A5 : CHORE + SPIKE ajoutés à la taxonomie.
        assert_eq!(
            parse_link("kind:CHORE"),
            Ok(ProjectMapLink::Kind(KindKind::Chore))
        );
        assert_eq!(
            parse_link("kind:SPIKE"),
            Ok(ProjectMapLink::Kind(KindKind::Spike))
        );
    }

    #[test]
    fn kind_unknown_value_is_rejected() {
        assert_eq!(
            parse_link("kind:BUGFIX"),
            Err(SchemaError::InvalidKind("BUGFIX".to_string()))
        );
    }

    #[test]
    fn version_namespaced_parses_to_version() {
        assert_eq!(
            parse_link("version:gradatum/0.6.1"),
            Ok(ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "0.6.1".to_string(),
            })
        );
    }

    #[test]
    fn version_without_slash_is_malformed() {
        assert_eq!(
            parse_link("version:0.6.1"),
            Err(SchemaError::MalformedVersion("0.6.1".to_string()))
        );
    }

    #[test]
    fn version_empty_project_is_malformed() {
        assert_eq!(
            parse_link("version:/0.6.1"),
            Err(SchemaError::MalformedVersion("/0.6.1".to_string()))
        );
    }

    #[test]
    fn annex_spec_plan_context_parse_to_annex() {
        assert_eq!(
            parse_link("spec:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            Ok(ProjectMapLink::Annex {
                role: AnnexRole::Spec,
                target: "01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
            })
        );
        assert_eq!(
            parse_link("plan:plans/2026-06-19.md"),
            Ok(ProjectMapLink::Annex {
                role: AnnexRole::Plan,
                target: "plans/2026-06-19.md".to_string(),
            })
        );
        assert_eq!(
            parse_link("context:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            Ok(ProjectMapLink::Annex {
                role: AnnexRole::Context,
                target: "01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
            })
        );
    }

    #[test]
    fn dep_section_ulid_parses_to_dep_unregressed() {
        // Rétrocompat : le format historique section:ULID reste une dépendance.
        assert_eq!(
            parse_link("decisions:01KVBTMYNK4XXZJAKWMTB4AM9K"),
            Ok(ProjectMapLink::Dep {
                section: "decisions".to_string(),
                ulid: "01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
            })
        );
    }

    #[test]
    fn bare_value_without_prefix_is_missing_prefix() {
        // Un titre humain nu (pas de `:`) n'est ni typé ni section:ULID.
        assert_eq!(
            parse_link("Mon Titre Humain"),
            Err(SchemaError::MissingPrefix("Mon Titre Humain".to_string()))
        );
    }

    #[test]
    fn reserved_prefix_with_empty_value_is_rejected() {
        assert_eq!(
            parse_link("project:"),
            Err(SchemaError::EmptyValue("project".to_string()))
        );
        assert_eq!(
            parse_link("status:"),
            Err(SchemaError::EmptyValue("status".to_string()))
        );
    }

    #[test]
    fn leading_trailing_whitespace_is_trimmed() {
        assert_eq!(
            parse_link("  project:gradatum  "),
            Ok(ProjectMapLink::Project("gradatum".to_string()))
        );
    }

    // ── V1 : contraindre charset + longueur project / version ────────────────

    /// `[[project:../../etc]]` → InvalidChars (path traversal rejeté).
    #[test]
    fn project_path_traversal_is_rejected_invalid_chars() {
        let err = parse_link("project:../../etc").unwrap_err();
        assert!(
            matches!(err, SchemaError::InvalidChars(ref p, _) if p == "project"),
            "attendu InvalidChars(project, …), reçu {err:?}"
        );
    }

    /// `[[project:<65 chars>]]` → ValueTooLong.
    #[test]
    fn project_name_over_64_chars_is_rejected() {
        let long_name = "a".repeat(65);
        let err = parse_link(&format!("project:{long_name}")).unwrap_err();
        assert!(
            matches!(err, SchemaError::ValueTooLong(ref p, _, 65) if p == "project"),
            "attendu ValueTooLong(project, _, 65), reçu {err:?}"
        );
    }

    /// `[[project:My-Project]]` → InvalidChars (majuscules interdites).
    #[test]
    fn project_uppercase_is_rejected() {
        let err = parse_link("project:My-Project").unwrap_err();
        assert!(
            matches!(err, SchemaError::InvalidChars(ref p, _) if p == "project"),
            "attendu InvalidChars(project, …), reçu {err:?}"
        );
    }

    /// `[[version:My_Project/0.6.1]]` → InvalidChars sur version.project (majuscules).
    #[test]
    fn version_project_uppercase_is_rejected() {
        let err = parse_link("version:My_Project/0.6.1").unwrap_err();
        assert!(
            matches!(err, SchemaError::InvalidChars(ref p, _) if p == "version.project"),
            "attendu InvalidChars(version.project, …), reçu {err:?}"
        );
    }

    /// `[[version:gradatum/1.0.0 evil]]` → InvalidChars sur version.version (espace).
    #[test]
    fn version_version_with_space_is_rejected() {
        let err = parse_link("version:gradatum/1.0.0 evil").unwrap_err();
        assert!(
            matches!(err, SchemaError::InvalidChars(ref p, _) if p == "version.version"),
            "attendu InvalidChars(version.version, …), reçu {err:?}"
        );
    }

    /// `[[project:my-project_v1.2]]` → valide (charset OK : `-`, `_`, `.` acceptés).
    #[test]
    fn project_with_dash_underscore_dot_is_valid() {
        assert_eq!(
            parse_link("project:my-project_v1.2"),
            Ok(ProjectMapLink::Project("my-project_v1.2".to_string()))
        );
    }

    /// `[[version:my-proj/1.2.3-rc.1]]` → valide.
    #[test]
    fn version_with_semver_prerelease_is_valid() {
        assert_eq!(
            parse_link("version:my-proj/1.2.3-rc.1"),
            Ok(ProjectMapLink::Version {
                project: "my-proj".to_string(),
                version: "1.2.3-rc.1".to_string(),
            })
        );
    }

    #[test]
    fn status_kind_wire_roundtrip() {
        for s in [
            StatusKind::Brainstorming,
            StatusKind::Open,
            StatusKind::InProgress,
            StatusKind::Blocked,
            StatusKind::Done,
            StatusKind::Obsolete,
        ] {
            assert_eq!(StatusKind::from_wire(s.as_wire()), Some(s));
        }
        for k in [
            KindKind::Feature,
            KindKind::Enhancement,
            KindKind::Fix,
            KindKind::Chore,
            KindKind::Spike,
            KindKind::Task,
        ] {
            assert_eq!(KindKind::from_wire(k.as_wire()), Some(k));
        }
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;

    /// Jeu minimal valide : 1 project + 1 status + 1 kind.
    fn minimal_valid() -> Vec<ProjectMapLink> {
        vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Feature),
        ]
    }

    #[test]
    fn minimal_triple_is_valid() {
        assert_eq!(validate_links(&minimal_valid()), Ok(()));
    }

    #[test]
    fn zero_project_is_rejected() {
        let links = vec![
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Feature),
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ProjectCardinality(0))
        );
    }

    #[test]
    fn two_projects_is_rejected() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Project("other".to_string()));
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ProjectCardinality(2))
        );
    }

    #[test]
    fn zero_kind_is_rejected() {
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
        ];
        assert_eq!(validate_links(&links), Err(SchemaError::KindCardinality(0)));
    }

    #[test]
    fn two_status_is_rejected() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Status(StatusKind::Done));
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::StatusCardinality(2))
        );
    }

    #[test]
    fn two_versions_is_rejected() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Version {
            project: "gradatum".to_string(),
            version: "0.6.1".to_string(),
        });
        links.push(ProjectMapLink::Version {
            project: "gradatum".to_string(),
            version: "0.6.2".to_string(),
        });
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::VersionCardinality(2))
        );
    }

    #[test]
    fn version_project_mismatch_is_rejected() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Version {
            project: "example-project".to_string(),
            version: "0.6.1".to_string(),
        });
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::VersionProjectMismatch {
                project: "gradatum".to_string(),
                version_project: "example-project".to_string(),
            })
        );
    }

    #[test]
    fn one_version_matching_project_is_valid() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Version {
            project: "gradatum".to_string(),
            version: "0.6.1".to_string(),
        });
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn annexes_and_deps_in_any_number_are_valid() {
        let mut links = minimal_valid();
        links.push(ProjectMapLink::Annex {
            role: AnnexRole::Spec,
            target: "01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
        });
        links.push(ProjectMapLink::Annex {
            role: AnnexRole::Plan,
            target: "plans/x.md".to_string(),
        });
        links.push(ProjectMapLink::Dep {
            section: "decisions".to_string(),
            ulid: "01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
        });
        links.push(ProjectMapLink::Dep {
            section: "council".to_string(),
            ulid: "01KVBTMYNK4XXZJAKWMTB4AM9C".to_string(),
        });
        assert_eq!(validate_links(&links), Ok(()));
    }
}
