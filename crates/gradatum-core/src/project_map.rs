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
//! are: `project` · `status` · `kind` · `version` · `spec` · `plan` · `context` ·
//! `feature` · `release` · `supersedes` · `parent`. Any other prefix (e.g.
//! `decisions:`) is a **content dependency** ([`ProjectMapLink::Dep`]) that reuses
//! the existing `[[section:ULID]]` format without regression.
//!
//! | Role | Form | Variant |
//! |---|---|---|
//! | project | `[[project:gradatum]]` | [`ProjectMapLink::Project`] |
//! | status | `[[status:DONE]]` | [`ProjectMapLink::Status`] |
//! | kind | `[[kind:FIX]]` | [`ProjectMapLink::Kind`] |
//! | version | `[[version:gradatum/0.6.1]]` | [`ProjectMapLink::Version`] |
//! | annex | `[[spec:…]]` `[[plan:…]]` `[[context:…]]` | [`ProjectMapLink::Annex`] |
//! | feature | `[[feature:F-37]]` | [`ProjectMapLink::Feature`] |
//! | release | `[[release:planned]]` | [`ProjectMapLink::Release`] |
//! | supersedes | `[[supersedes:F-12]]` | [`ProjectMapLink::Supersedes`] |
//! | parent | `[[parent:F-31]]` | [`ProjectMapLink::Parent`] |
//! | dependency | `[[decisions:01K…]]` | [`ProjectMapLink::Dep`] |
//!
//! ## Carte-feature
//!
//! La présence d'au moins un `[[feature:F-XX]]` désigne une **carte-feature** :
//! [`validate_links`] exige alors exactement 1 `feature` et 1 `release`. Les
//! cartes sans `feature` (changelog) conservent la validation historique sans
//! régression (et n'autorisent aucun `[[release:]]`).
//!
//! ## Case sensitivity
//!
//! `status` and `kind` values are normalised to **SCREAMING_SNAKE** on the wire
//! to prevent case-sensitive bugs (`[[status:done]]` rejected, `[[status:DONE]]`
//! accepted). `release` values are **lowercase** on the wire (mirror of the site
//! enum) : `[[release:planned]]` accepted, `[[release:PLANNED]]` rejected.
//!
//! ## Validation design
//!
//! - The project-map validator ([`validate_links`]) is **dedicated** to this schema
//!   and does not invoke a generic schema-registry subsystem.

use serde::{Deserialize, Serialize};
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

/// Statut de livraison (release) d'une carte-feature project-map.
///
/// Axe **orthogonal** au [`StatusKind`] de cycle de vie : `StatusKind` décrit
/// l'avancement du travail, `ReleaseKind` décrit la position sur la roadmap de
/// version. Contrairement à [`StatusKind`]/[`KindKind`] (SCREAMING_SNAKE), les
/// valeurs wire de `ReleaseKind` sont **en minuscules** — elles miroir
/// exactement l'énumération du site (`released`/`planned`/`roadmap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReleaseKind {
    /// Envisagée à long terme, sans version cible engagée.
    Roadmap,
    /// Planifiée pour une version cible.
    Planned,
    /// Livrée dans une version publiée.
    Released,
    /// Écartée (remplacée ou annulée).
    Dropped,
}

impl ReleaseKind {
    /// Parse la valeur wire **minuscule** d'un `[[release:…]]`.
    ///
    /// Matching sensible à la casse : `"planned"` est accepté ; `"PLANNED"` est
    /// rejeté (cohérence avec l'énumération du site).
    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "roadmap" => Some(Self::Roadmap),
            "planned" => Some(Self::Planned),
            "released" => Some(Self::Released),
            "dropped" => Some(Self::Dropped),
            _ => None,
        }
    }

    /// Représentation wire **minuscule** (inverse de `from_wire`).
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::Roadmap => "roadmap",
            Self::Planned => "planned",
            Self::Released => "released",
            Self::Dropped => "dropped",
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
    /// `[[feature:F-XX]]` — identité de la carte-feature (exactement 1, discriminant).
    ///
    /// La présence d'au moins un `Feature` fait basculer [`validate_links`] en
    /// mode carte-feature (release obligatoire).
    Feature(String),
    /// `[[release:<r>]]` — statut de livraison (1 sur carte-feature, 0 sinon).
    Release(ReleaseKind),
    /// `[[supersedes:F-YY]]` — feature remplacée (0..N).
    ///
    /// Distinct de [`ProjectMapLink::Feature`] : ne participe **pas** au compte
    /// de cardinalité feature.
    Supersedes(String),
    /// `[[parent:F-YY]]` — feature d'origine dont cette carte est une continuation (0..N).
    ///
    /// Rôle structurel Règle B (NOMENCLATURE §10e) : une carte de continuation
    /// référence la feature d'origine par ce lien. Ne participe **pas** au compte
    /// de cardinalité feature. Cardinalité 0..N (multi-parent autorisé).
    Parent(String),
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

    /// Identifiant `feature:`/`supersedes:` hors format `F-\d{2,3}` (ex. `f-37`, `F-1`).
    #[error("identifiant feature invalide {0:?} (attendu F-NN ou F-NNN, ex. F-37, F-061)")]
    FeatureIdentInvalid(String),

    /// Valeur de release hors taxonomie (`[[release:…]]`).
    #[error("release invalide {0:?} (attendu minuscule ∈ roadmap/planned/released/dropped)")]
    InvalidRelease(String),

    /// `feature:` multiple sur une carte-feature (exactement 1 requis).
    #[error("exactement 1 lien feature: requis sur une carte-feature, trouvé {0}")]
    FeatureCardinality(usize),

    /// `release:` en nombre incorrect (1 si carte-feature, 0 sinon).
    #[error("nombre de liens release: incorrect (1 si carte-feature, 0 sinon), trouvé {0}")]
    ReleaseCardinality(usize),

    /// `version:` link absent or appears more than once on a feature card (exactly 1 required).
    ///
    /// A feature card (carrying `[[feature:F-XX]]`) requires exactly 1 `[[version:]]`.
    /// Use the sentinel `[[version:<project>/backlog]]` when no concrete version is yet
    /// known.
    #[error(
        "carte-feature: exactement 1 [[version:]] requis (ou sentinel projet/backlog), trouvé {0}"
    )]
    FeatureVersionCardinality(usize),
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

/// Valide qu'un identifiant `feature:`/`supersedes:` respecte le format `F-\d{2,3}`.
///
/// Format exact (parser dédié, **pas** [`validate_ident`] qui refuserait le `F`
/// majuscule) : un `F` majuscule, un tiret, puis 2 ou 3 chiffres ASCII. Exemples
/// valides : `F-37`, `F-061`. Invalides : `f-37`, `F-1`, `F-1234`, `feature37`.
///
/// Check manuel par caractères (aucune dépendance `regex`).
///
/// # Errors
///
/// [`SchemaError::FeatureIdentInvalid`] si `value` ne respecte pas le format.
fn validate_feature_ident(value: &str) -> Result<(), SchemaError> {
    let invalid = || SchemaError::FeatureIdentInvalid(value.to_string());
    // `F-` + 2..=3 chiffres → longueur totale 4 ou 5.
    let Some(digits) = value.strip_prefix("F-") else {
        return Err(invalid());
    };
    if !matches!(digits.len(), 2 | 3) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
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
/// - A **reserved prefix** (`project`/`status`/`kind`/`version`/`spec`/`plan`/
///   `context`/`feature`/`release`/`supersedes`) → typed structural or annex link.
/// - Any other prefix in `<section>:<ULID>` form → [`ProjectMapLink::Dep`].
///
/// # Errors
///
/// - [`SchemaError::EmptyValue`] si la valeur après un préfixe réservé est vide.
/// - [`SchemaError::InvalidStatus`] / [`SchemaError::InvalidKind`] /
///   [`SchemaError::InvalidRelease`] hors taxonomie.
/// - [`SchemaError::MalformedVersion`] si `version:` n'est pas `projet/x.y.z`.
/// - [`SchemaError::FeatureIdentInvalid`] si `feature:`/`supersedes:` n'est pas `F-\d{2,3}`.
/// - [`SchemaError::MissingPrefix`] si la cible n'a pas de préfixe `role:` et
///   n'est pas un `section:ULID` valide.
#[must_use = "le résultat de parsing doit être inspecté"]
pub fn parse_link(raw: &str) -> Result<ProjectMapLink, SchemaError> {
    let raw = raw.trim();
    let (prefix, value) = split_prefix(raw);

    // Préfixes réservés : valeur obligatoire et non vide.
    let reserved = matches!(
        prefix,
        "project"
            | "status"
            | "kind"
            | "version"
            | "spec"
            | "plan"
            | "context"
            | "feature"
            | "release"
            | "supersedes"
            | "parent"
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
        "feature" => {
            validate_feature_ident(value)?;
            Ok(ProjectMapLink::Feature(value.to_string()))
        }
        "release" => ReleaseKind::from_wire(value)
            .map(ProjectMapLink::Release)
            .ok_or_else(|| SchemaError::InvalidRelease(value.to_string())),
        "supersedes" => {
            validate_feature_ident(value)?;
            Ok(ProjectMapLink::Supersedes(value.to_string()))
        }
        "parent" => {
            validate_feature_ident(value)?;
            Ok(ProjectMapLink::Parent(value.to_string()))
        }
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
/// any number of `Annex`, `Dep`, `Supersedes` and `Parent`. If a `Version` is present, its
/// namespaced project must match the `Project` link.
///
/// **Carte-feature** : si au moins un `Feature` est présent, exactement 1
/// `Feature`, 1 `Release`, 1 `Version` et un `kind ∈ KindKind` sont exigés.
/// Tout kind de l'enum est accepté — seul `kind:FEATURE` est exporté vers le
/// site (export T2) ; les autres kinds (FIX/CHORE/SPIKE/TASK/ENHANCEMENT) sont
/// vault-only. En l'absence de `Feature` (carte changelog), aucun `Release`
/// n'est autorisé et `Kind` reste libre — la validation historique est inchangée.
///
/// # Errors
///
/// Une [`SchemaError`] de cardinalité ou de cohérence au premier écart constaté
/// (ordre : project → status → kind → version → mismatch → feature → release).
#[must_use = "le résultat de validation doit être inspecté avant d'accepter l'écriture"]
pub fn validate_links(links: &[ProjectMapLink]) -> Result<(), SchemaError> {
    let mut projects: Vec<&str> = Vec::new();
    let mut status_count = 0usize;
    let mut kind_count = 0usize;
    let mut versions: Vec<&str> = Vec::new();
    let mut feature_count = 0usize;
    let mut release_count = 0usize;

    for link in links {
        match link {
            ProjectMapLink::Project(p) => projects.push(p),
            ProjectMapLink::Status(_) => status_count += 1,
            ProjectMapLink::Kind(_) => kind_count += 1,
            ProjectMapLink::Version { project, .. } => versions.push(project),
            ProjectMapLink::Feature(_) => feature_count += 1,
            ProjectMapLink::Release(_) => release_count += 1,
            // Supersedes et Parent ne participent à aucun compte de cardinalité.
            ProjectMapLink::Annex { .. }
            | ProjectMapLink::Dep { .. }
            | ProjectMapLink::Supersedes(_)
            | ProjectMapLink::Parent(_) => {}
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

    // Règle conditionnelle carte-feature (axe orthogonal, non régressif pour les
    // cartes changelog sans feature). Évaluée après les checks existants pour
    // préserver leurs messages d'erreur.
    if feature_count >= 1 {
        // Carte-feature : exactement 1 feature + exactement 1 release +
        // exactement 1 version (sentinel gradatum/backlog admis) + kind ∈ enum.
        // Ordre : feature → release → version (cohérent avec §10e).
        // Note : seul kind:FEATURE est exporté vers le site (export T2 S2) ;
        // les autres kinds sont vault-only (FIX/CHORE/SPIKE/TASK/ENHANCEMENT).
        if feature_count != 1 {
            return Err(SchemaError::FeatureCardinality(feature_count));
        }
        if release_count != 1 {
            return Err(SchemaError::ReleaseCardinality(release_count));
        }
        // NOMENCLATURE §10e + spec §3.1 : version obligatoire (cardinalité 1).
        // Le sentinel [[version:<projet>/backlog]] satisfait cette contrainte
        // pour les features sans version concrète.
        if versions.len() != 1 {
            return Err(SchemaError::FeatureVersionCardinality(versions.len()));
        }
        // kind_count == 1 est garanti par le check KindCardinality ci-dessus.
        // Slice 1 S1 : tout kind ∈ KindKind est accepté sur une carte-feature.
        // La contrainte kind == FEATURE est intentionnellement retirée.
    } else if release_count != 0 {
        // Carte non-feature : aucun [[release:]] autorisé sans [[feature:]].
        return Err(SchemaError::ReleaseCardinality(release_count));
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
/// - `Some(dst)` pour `project`/`status`/`kind`/`version`/`feature`/`release`/
///   `supersedes`/`parent` bien formés, où `dst` est l'identifiant canonique du
///   nœud (statut/kind normalisés en wire via [`parse_link`]).
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
        // Rôles feature : nœuds réservés typés navigables (pas de note ULID).
        // `supersedes:F-YY` et `parent:F-YY` pointent vers le nœud `feature:F-YY`.
        ProjectMapLink::Feature(f) => Some(format!("feature:{f}")),
        ProjectMapLink::Release(r) => Some(format!("release:{}", r.as_wire())),
        ProjectMapLink::Supersedes(f) => Some(format!("feature:{f}")),
        ProjectMapLink::Parent(f) => Some(format!("feature:{f}")),
        // Annexes (spec/plan/context) et dépendances (section:ULID) pointent vers
        // de vraies notes → résolution ULID normale, pas un nœud réservé.
        ProjectMapLink::Annex { .. } | ProjectMapLink::Dep { .. } => None,
    }
}

// ── Export feature entries — logique de projection partagée ──────────────────
//
// Cette section fournit les types et la fonction de projection PURE qui permettent
// à l'admin CLI ET au serveur HTTP de produire la même liste `Vec<FeatureEntry>`
// depuis des notes project-map brutes, sans duplication du mapping.
//
// ## Architecture DRY
//
// - `FeatureEntry` / `ExportOptions` sont définis ici (gradatum-core), accessible
//   depuis l'admin et le serveur sans dépendance croisée.
// - `project_map_feature_entries` prend `&[(body_text, title)]` — entrées pures,
//   sans I/O. La couche de récupération (SQL admin / Index serveur) est séparée.
// - `map_version_raw` et `feature_sort_key` sont réutilisés par les deux
//   consommateurs.

/// Projection d'une carte-feature pour l'export JSON miroir-site.
///
/// Produit par [`project_map_feature_entries`] depuis les notes brutes
/// section `project-map`. Utilisé par l'admin CLI et le handler HTTP
/// `GET /api/v1/project-map/export-features`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureEntry {
    /// Identifiant de la carte-feature (ex. `"F-37"`).
    pub feature: String,
    /// Statut de livraison wire lowercase (`"roadmap"` | `"planned"` | `"released"` | `"dropped"`).
    pub release: String,
    /// Version cible au format site (`"vX.Y.Z"` réel), ou `"vX.Y.Z"` littéral
    /// pour les cartes backlog (Règle A NOMENCLATURE §10e — jamais `null`).
    pub version: Option<String>,
    /// Titre H1 de la carte.
    pub title: String,
}

/// Options de filtrage pour [`project_map_feature_entries`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOptions {
    /// Si `true`, inclut les cartes `release:dropped` (audit complet).
    /// Défaut (`false`) : miroir-site, exclut uniquement `dropped`.
    /// Les cartes `version:*/backlog` sont toujours incluses (Règle A).
    pub include_dropped: bool,
}

/// Trie deux identifiants F-XX numériquement sur la partie `\d{2,3}`.
///
/// `"F-37"` → 37, `"F-061"` → 61. Identifiants invalides → 0 (ordre stable).
fn feature_sort_key(id: &str) -> u32 {
    id.strip_prefix("F-")
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

/// Mappe la valeur brute `version:` vers la chaîne d'affichage du miroir-site.
///
/// - `"gradatum/0.6.4"` → `Some("v0.6.4")` (version concrète préfixée `v`).
/// - `"gradatum/backlog"` → `Some("vX.Y.Z")` (Règle A NOMENCLATURE §10e :
///   les cartes backlog sont visibles sur le miroir-site avec ce sentinel
///   littéral, afin de ne pas être filtrées côté CI).
///
/// Le namespace projet (partie avant `/`) est ignoré : validé par le triple
/// `[[project:]]`/`[[version:]]` du schéma project-map.
pub fn map_version_raw(raw: &str) -> Option<String> {
    let ver = raw.split_once('/').map(|(_, v)| v).unwrap_or(raw);
    if ver == "backlog" {
        Some("vX.Y.Z".to_string())
    } else {
        Some(format!("v{ver}"))
    }
}

/// Projection pure notes project-map → `Vec<FeatureEntry>`.
///
/// Entrées : slice de tuples `(body_text, title)` représentant les notes brutes
/// de la section `project-map` (déjà filtrées `status != 'downgraded'/'garbage'`
/// par la couche de stockage amont).
///
/// Traitement :
/// 1. Parse les wikilinks via [`parse_link`] — les liens invalides sont ignorés.
/// 2. Ne garde que les cartes avec `[[feature:F-XX]]`.
/// 3. Filtre `release:dropped` si `opts.include_dropped == false`.
/// 4. Mappe `[[version:…]]` via [`map_version_raw`] (Règle A : backlog → `"vX.Y.Z"`).
/// 5. Trie par identifiant F-XX croissant numérique.
///
/// Cette fonction est **pure** (pas d'I/O, pas de `Result`) — les erreurs de
/// parsing de wikilinks individuels sont ignorées défensivement (carte ignorée
/// si les rôles obligatoires sont absents).
#[must_use]
pub fn project_map_feature_entries(
    notes: &[(String, String)],
    opts: ExportOptions,
) -> Vec<FeatureEntry> {
    let mut entries: Vec<FeatureEntry> = Vec::new();

    for (body, title) in notes {
        // Parser les wikilinks — ignorer silencieusement les cibles invalides.
        let links: Vec<ProjectMapLink> = extract_wikilink_targets(body)
            .into_iter()
            .filter_map(|t| parse_link(&t).ok())
            .collect();

        let mut feature_id: Option<String> = None;
        let mut release_wire: Option<String> = None;
        let mut version_raw: Option<String> = None;
        let mut kind_wire: Option<&KindKind> = None;

        for link in &links {
            match link {
                ProjectMapLink::Feature(f) => feature_id = Some(f.clone()),
                ProjectMapLink::Release(r) => release_wire = Some(r.as_wire().to_string()),
                ProjectMapLink::Version { project, version } => {
                    version_raw = Some(format!("{project}/{version}"));
                }
                ProjectMapLink::Kind(k) => kind_wire = Some(k),
                _ => {}
            }
        }

        // Seules les cartes-feature (présence de [[feature:F-XX]]) sont projetées.
        let feature_id = match feature_id {
            Some(id) => id,
            None => continue,
        };

        let release = match release_wire {
            Some(r) => r,
            None => continue, // carte-feature sans release — invalide, ignorée défensivement
        };

        let version = version_raw.as_deref().and_then(map_version_raw);

        // Filtrage miroir-site : exclure release:dropped par défaut (Règle A).
        if !opts.include_dropped && release == "dropped" {
            continue;
        }

        // Filtrage miroir-site S2 : seul kind:FEATURE alimente le site (export T2).
        // Les cartes kind:FIX/CHORE/SPIKE/TASK/ENHANCEMENT sont vault-only.
        // `include_dropped` = mode audit complet : lève le filtre kind pour
        // inclure toutes les cartes-feature quelle que soit leur taxonomie.
        if !opts.include_dropped && !matches!(kind_wire, Some(KindKind::Feature)) {
            continue;
        }

        entries.push(FeatureEntry {
            feature: feature_id,
            release,
            version,
            title: title.clone(),
        });
    }

    // Tri par identifiant F-XX numérique croissant.
    entries.sort_by_key(|e| feature_sort_key(&e.feature));

    entries
}

/// Extrait les cibles brutes de wikilinks `[[target]]` d'un body.
///
/// Scan non-regex, char-safe. Retourne les contenus entre `[[` et `]]`
/// (la cible brute, y compris le préfixe `role:` — prête pour `parse_link`).
///
/// Usage interne : factorisation pour [`project_map_feature_entries`].
fn extract_wikilink_targets(body: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after_open = &rest[start + 2..];
        if let Some(end) = after_open.find("]]") {
            result.push(after_open[..end].to_string());
            rest = &after_open[end + 2..];
        } else {
            break;
        }
    }
    result
}

#[cfg(test)]
mod feature_entries_tests {
    use super::*;

    fn note(body: &str, title: &str) -> (String, String) {
        (body.to_string(), title.to_string())
    }

    /// Carte backlog → `version == Some("vX.Y.Z")` (Règle A).
    #[test]
    fn backlog_card_exports_with_vxyz_version() {
        let notes = vec![note(
            "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/backlog]]",
            "F-50 backlog",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(entries.len(), 1, "carte backlog incluse : {entries:?}");
        assert_eq!(entries[0].version, Some("vX.Y.Z".to_string()));
        assert_eq!(entries[0].feature, "F-50");
    }

    /// Carte backlog → **incluse** par défaut (miroir-site).
    #[test]
    fn backlog_card_included_in_default_mirror() {
        let notes = vec![note(
            "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
             [[release:planned]] [[version:gradatum/backlog]]",
            "F-50 backlog",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert!(
            !entries.is_empty(),
            "carte backlog doit être incluse par défaut"
        );
    }

    /// Carte `release:dropped` → **exclue** par défaut.
    #[test]
    fn dropped_card_excluded_by_default() {
        let notes = vec![note(
            "[[feature:F-51]] [[project:gradatum]] [[status:OBSOLETE]] [[kind:FEATURE]] \
             [[release:dropped]] [[version:gradatum/0.6.0]]",
            "F-51 dropped",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert!(
            entries.is_empty(),
            "carte dropped exclue par défaut : {entries:?}"
        );
    }

    /// Carte `release:dropped` → **incluse** avec `include_dropped`.
    #[test]
    fn dropped_card_included_with_include_dropped() {
        let notes = vec![note(
            "[[feature:F-51]] [[project:gradatum]] [[status:OBSOLETE]] [[kind:FEATURE]] \
             [[release:dropped]] [[version:gradatum/0.6.0]]",
            "F-51 dropped",
        )];
        let entries = project_map_feature_entries(
            &notes,
            ExportOptions {
                include_dropped: true,
            },
        );
        assert_eq!(
            entries.len(),
            1,
            "carte dropped incluse avec include_dropped"
        );
        assert_eq!(entries[0].feature, "F-51");
    }

    /// Version concrète → `"v0.6.3"` (préfixe `v` ajouté).
    #[test]
    fn concrete_version_exports_with_v_prefix() {
        let notes = vec![note(
            "[[feature:F-37]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
             [[release:released]] [[version:gradatum/0.6.3]]",
            "F-37 released",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(entries.len(), 1, "carte released incluse");
        assert_eq!(entries[0].version, Some("v0.6.3".to_string()));
    }

    /// Feature IDs are sorted numerically: `F-37` sorts before `F-50`.
    #[test]
    fn notes_are_sorted_by_feature_id_numerically() {
        let notes = vec![
            note(
                "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
                 [[release:planned]] [[version:gradatum/backlog]]",
                "F-50 backlog",
            ),
            note(
                "[[feature:F-37]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
                 [[release:released]] [[version:gradatum/0.6.3]]",
                "F-37 released",
            ),
        ];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].feature, "F-37");
        assert_eq!(entries[1].feature, "F-50");
    }

    /// Carte changelog sans `[[feature:]]` → **exclue**.
    #[test]
    fn changelog_card_without_feature_is_excluded() {
        let notes = vec![note(
            "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] [[version:gradatum/0.5.2]]\n\nFix.",
            "Fix changelog",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert!(entries.is_empty(), "carte changelog exclue : {entries:?}");
    }

    /// Empty input slice yields an empty result.
    #[test]
    fn empty_notes_returns_empty_vec() {
        let entries = project_map_feature_entries(&[], ExportOptions::default());
        assert!(entries.is_empty());
    }

    /// `map_version_raw` : backlog → `"vX.Y.Z"`, version concrète → `"vX.Y.Z"`.
    #[test]
    fn map_version_raw_formats_correctly() {
        assert_eq!(
            map_version_raw("gradatum/0.5.2"),
            Some("v0.5.2".to_string())
        );
        assert_eq!(
            map_version_raw("gradatum/backlog"),
            Some("vX.Y.Z".to_string())
        );
    }

    /// Titre est bien propagé dans l'entrée.
    #[test]
    fn title_is_propagated_to_entry() {
        let notes = vec![note(
            "[[feature:F-10]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
             [[release:released]] [[version:gradatum/0.5.0]]",
            "Ma Feature Titre",
        )];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(entries[0].title, "Ma Feature Titre");
    }

    // ── S2 : filtre kind:FEATURE sur l'export miroir-site ────────────────────

    /// The site-mirror export excludes cards with `kind != FEATURE` by default.
    ///
    /// Cards with `kind:FIX` (debt, vault-only) are excluded; cards with `kind:FEATURE`
    /// are included. Only `kind:FEATURE` cards are published to the site export.
    #[test]
    fn project_map_feature_entries_excludes_non_feature_kind() {
        let notes = vec![
            note(
                "[[feature:F-99]] [[project:gradatum]] [[status:OPEN]] [[kind:FIX]] \
                 [[release:roadmap]] [[version:gradatum/backlog]]",
                "Dette F-99",
            ),
            note(
                "[[feature:F-37]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
                 [[release:released]] [[version:gradatum/0.6.3]]",
                "Feature F-37",
            ),
        ];
        let entries = project_map_feature_entries(&notes, ExportOptions::default());
        assert_eq!(
            entries.len(),
            1,
            "seul F-37 kind:FEATURE attendu : {entries:?}"
        );
        assert_eq!(entries[0].feature, "F-37");
    }

    /// Mode audit `include_dropped=true` : lève le filtre kind — toutes les
    /// cartes-feature sont incluses quelle que soit leur taxonomie.
    ///
    /// Ce mode sert les audits internes (inventaire complet du vault) ;
    /// il ne doit PAS être utilisé pour générer `features.ts` site.
    #[test]
    fn project_map_feature_entries_include_dropped_lifts_kind_filter() {
        let notes = vec![
            note(
                "[[feature:F-99]] [[project:gradatum]] [[status:OPEN]] [[kind:FIX]] \
                 [[release:roadmap]] [[version:gradatum/backlog]]",
                "Dette F-99",
            ),
            note(
                "[[feature:F-37]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
                 [[release:released]] [[version:gradatum/0.6.3]]",
                "Feature F-37",
            ),
        ];
        let entries = project_map_feature_entries(
            &notes,
            ExportOptions {
                include_dropped: true,
            },
        );
        // Mode audit = filtre kind levé : F-37 + F-99 attendus.
        assert_eq!(
            entries.len(),
            2,
            "mode audit doit inclure toutes les cartes-feature : {entries:?}"
        );
        assert_eq!(entries[0].feature, "F-37");
        assert_eq!(entries[1].feature, "F-99");
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

#[cfg(test)]
mod feature_card_tests {
    use super::*;

    // ── parse_link unitaires : nouveaux rôles ────────────────────────────────

    #[test]
    fn release_value_parses_to_release_kind() {
        assert_eq!(
            parse_link("release:released"),
            Ok(ProjectMapLink::Release(ReleaseKind::Released))
        );
        assert_eq!(
            parse_link("release:planned"),
            Ok(ProjectMapLink::Release(ReleaseKind::Planned))
        );
        assert_eq!(
            parse_link("release:roadmap"),
            Ok(ProjectMapLink::Release(ReleaseKind::Roadmap))
        );
        assert_eq!(
            parse_link("release:dropped"),
            Ok(ProjectMapLink::Release(ReleaseKind::Dropped))
        );
    }

    #[test]
    fn release_uppercase_is_rejected_lowercase_wire() {
        // ReleaseKind est lowercase (miroir Zod du site), pas SCREAMING_SNAKE.
        assert_eq!(
            parse_link("release:PLANNED"),
            Err(SchemaError::InvalidRelease("PLANNED".to_string()))
        );
    }

    #[test]
    fn release_unknown_value_is_rejected() {
        assert_eq!(
            parse_link("release:wip"),
            Err(SchemaError::InvalidRelease("wip".to_string()))
        );
    }

    #[test]
    fn release_empty_value_is_rejected() {
        assert_eq!(
            parse_link("release:"),
            Err(SchemaError::EmptyValue("release".to_string()))
        );
    }

    #[test]
    fn feature_ident_parses_to_feature() {
        assert_eq!(
            parse_link("feature:F-37"),
            Ok(ProjectMapLink::Feature("F-37".to_string()))
        );
        assert_eq!(
            parse_link("feature:F-061"),
            Ok(ProjectMapLink::Feature("F-061".to_string()))
        );
    }

    #[test]
    fn feature_empty_value_is_rejected() {
        assert_eq!(
            parse_link("feature:"),
            Err(SchemaError::EmptyValue("feature".to_string()))
        );
    }

    #[test]
    fn feature_lowercase_f_is_rejected() {
        assert_eq!(
            parse_link("feature:f-37"),
            Err(SchemaError::FeatureIdentInvalid("f-37".to_string()))
        );
    }

    #[test]
    fn feature_single_digit_is_rejected() {
        assert_eq!(
            parse_link("feature:F-1"),
            Err(SchemaError::FeatureIdentInvalid("F-1".to_string()))
        );
    }

    #[test]
    fn feature_four_digit_is_rejected() {
        assert_eq!(
            parse_link("feature:F-1234"),
            Err(SchemaError::FeatureIdentInvalid("F-1234".to_string()))
        );
    }

    #[test]
    fn feature_non_pattern_is_rejected() {
        assert_eq!(
            parse_link("feature:feature37"),
            Err(SchemaError::FeatureIdentInvalid("feature37".to_string()))
        );
    }

    #[test]
    fn supersedes_ident_parses_to_supersedes() {
        assert_eq!(
            parse_link("supersedes:F-12"),
            Ok(ProjectMapLink::Supersedes("F-12".to_string()))
        );
    }

    #[test]
    fn supersedes_invalid_ident_is_rejected() {
        assert_eq!(
            parse_link("supersedes:f-12"),
            Err(SchemaError::FeatureIdentInvalid("f-12".to_string()))
        );
    }

    #[test]
    fn supersedes_empty_value_is_rejected() {
        assert_eq!(
            parse_link("supersedes:"),
            Err(SchemaError::EmptyValue("supersedes".to_string()))
        );
    }

    #[test]
    fn release_kind_wire_roundtrip() {
        for r in [
            ReleaseKind::Roadmap,
            ReleaseKind::Planned,
            ReleaseKind::Released,
            ReleaseKind::Dropped,
        ] {
            assert_eq!(ReleaseKind::from_wire(r.as_wire()), Some(r));
        }
    }

    // ── validate_links : carte-feature ───────────────────────────────────────

    /// Carte-feature minimale valide : feature + project + status + kind + release + version.
    ///
    /// Le sentinel `[[version:gradatum/backlog]]` satisfait l'obligation §10e pour
    /// les features sans version concrète encore connue.
    fn minimal_feature_card() -> Vec<ProjectMapLink> {
        vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ]
    }

    #[test]
    fn feature_card_complete_is_valid() {
        assert_eq!(validate_links(&minimal_feature_card()), Ok(()));
    }

    #[test]
    fn feature_card_with_backlog_version_is_valid() {
        // minimal_feature_card() porte déjà version:gradatum/backlog — le test
        // vérifie que le sentinel est bien accepté comme version conforme §10e.
        assert_eq!(validate_links(&minimal_feature_card()), Ok(()));
    }

    #[test]
    fn feature_card_with_concrete_version_is_valid() {
        // Carte-feature avec version concrète (0.6.4) à la place du sentinel.
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "0.6.4".to_string(),
            },
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn feature_card_version_project_mismatch_is_rejected() {
        // La version namespacée doit correspondre au projet déclaré.
        // Carte construite directement (pas de push sur minimal) pour
        // avoir exactement 1 version — celle qui mismatche.
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "example-project".to_string(),
                version: "0.6.4".to_string(),
            },
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::VersionProjectMismatch {
                project: "gradatum".to_string(),
                version_project: "example-project".to_string(),
            })
        );
    }

    #[test]
    fn feature_card_with_one_supersedes_is_valid() {
        let mut links = minimal_feature_card();
        links.push(ProjectMapLink::Supersedes("F-12".to_string()));
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn feature_card_with_two_supersedes_does_not_inflate_feature_count() {
        let mut links = minimal_feature_card();
        links.push(ProjectMapLink::Supersedes("F-12".to_string()));
        links.push(ProjectMapLink::Supersedes("F-13".to_string()));
        // supersedes ne compte pas comme feature → toujours valide.
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn two_features_is_rejected_by_feature_cardinality() {
        let mut links = minimal_feature_card();
        links.push(ProjectMapLink::Feature("F-38".to_string()));
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::FeatureCardinality(2))
        );
    }

    #[test]
    fn feature_card_zero_release_is_rejected() {
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ReleaseCardinality(0))
        );
    }

    #[test]
    fn feature_card_two_releases_is_rejected() {
        let mut links = minimal_feature_card();
        links.push(ProjectMapLink::Release(ReleaseKind::Released));
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ReleaseCardinality(2))
        );
    }

    #[test]
    fn release_without_feature_is_rejected() {
        // Un [[release:]] sur une carte SANS [[feature:]] est invalide.
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ReleaseCardinality(1))
        );
    }

    // ── NON-RÉGRESSION : cartes changelog (sans feature) ─────────────────────

    #[test]
    fn changelog_card_without_feature_or_release_stays_valid() {
        // Carte changelog historique typique : project + status + kind, RIEN d'autre.
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Feature),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn changelog_card_with_version_stays_valid() {
        // Carte changelog avec version : toujours valide, inchangée.
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Fix),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "0.5.2".to_string(),
            },
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn changelog_card_zero_project_still_rejected_same_error() {
        // Régression : une carte changelog invalide (0 project) garde la MÊME erreur.
        let links = vec![
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Fix),
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::ProjectCardinality(0))
        );
    }

    #[test]
    fn validate_from_targets_routes_feature_card() {
        // Wiring : validate_links_from_targets construit bien une carte-feature.
        // Quintuple §10e : feature + project + status + release + version.
        // Le sentinel version:gradatum/backlog est la forme minimale conforme.
        let targets: Vec<String> = [
            "feature:F-37",
            "project:gradatum",
            "status:IN_PROGRESS",
            "kind:FEATURE",
            "release:planned",
            "version:gradatum/backlog",
            "Titre humain ignoré",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(validate_links_from_targets(&targets), Ok(()));
    }

    // ── P2-1 (mis à jour S1) : carte-feature accepte tout kind ∈ KindKind ─────
    //
    // Slice 1 S1 : la contrainte `kind == FEATURE` sur les cartes-feature est
    // levée. Seul `kind:FEATURE` est exporté vers le site (export T2) ; les
    // autres kinds (FIX/CHORE/SPIKE/TASK/ENHANCEMENT) restent vault-only.

    #[test]
    fn feature_card_with_kind_fix_is_accepted() {
        // Carte-feature avec kind:FIX → Ok (Slice 1 S1, mapping debt→FIX).
        // La version est requise (§10e).
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Fix),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    #[test]
    fn feature_card_with_kind_feature_is_valid_kind_check() {
        // Carte-feature avec kind:FEATURE → Ok (inchangé, compatibilité).
        assert_eq!(validate_links(&minimal_feature_card()), Ok(()));
    }

    #[test]
    fn changelog_card_kind_fix_stays_valid_without_feature() {
        // Non-régression : kind:FIX est légitime sur une carte changelog (sans feature).
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Fix),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    // ── S1 : carte-feature accepte tout kind ∈ KindKind (pas FEATURE seul) ────

    /// A feature card with `kind:FIX` passes validation (the `kind` link is not constrained).
    ///
    /// The 5 required link roles (feature/project/status/release/version) remain
    /// mandatory; only the `kind == FEATURE` constraint is relaxed.
    #[test]
    fn validate_links_accepts_feature_card_with_non_feature_kind() {
        // kind:FIX — mapping gov-todo debt→FIX (§3.1 spec Slice 1).
        let links_fix = vec![
            ProjectMapLink::Feature("F-83".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Fix),
            ProjectMapLink::Release(ReleaseKind::Roadmap),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ];
        assert_eq!(
            validate_links(&links_fix),
            Ok(()),
            "kind:FIX doit être accepté sur une carte-feature"
        );

        // kind:CHORE — mapping gov-todo chore→CHORE.
        let links_chore = vec![
            ProjectMapLink::Feature("F-84".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Kind(KindKind::Chore),
            ProjectMapLink::Release(ReleaseKind::Roadmap),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ];
        assert_eq!(
            validate_links(&links_chore),
            Ok(()),
            "kind:CHORE doit être accepté sur une carte-feature"
        );
    }

    /// Non-régression : un `kind` absent reste rejeté (`KindCardinality`).
    ///
    /// Le relâchement S1 ne touche que la contrainte `== FEATURE` ;
    /// l'obligation d'avoir exactement 1 `kind` reste inchangée.
    #[test]
    fn validate_links_still_rejects_missing_kind() {
        let links = vec![
            ProjectMapLink::Feature("F-83".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Open),
            ProjectMapLink::Release(ReleaseKind::Roadmap),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
            // Pas de Kind → KindCardinality(0).
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::KindCardinality(0)),
            "kind absent doit toujours être rejeté"
        );
    }

    // ── P1 conformité §10e : [[version:]] OBLIGATOIRE sur carte-feature ──────

    #[test]
    fn feature_card_without_version_is_rejected() {
        // NOMENCLATURE §10e + spec §3.1 : quintuple feature+project+status+release+version.
        // Une carte-feature sans [[version:]] doit être rejetée (le sentinel
        // [[version:gradatum/backlog]] existe précisément pour ce cas).
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Kind(KindKind::Feature),
            // Pas de Version — doit provoquer FeatureVersionCardinality(0).
        ];
        assert_eq!(
            validate_links(&links),
            Err(SchemaError::FeatureVersionCardinality(0))
        );
    }
}

#[cfg(test)]
mod parent_tests {
    use super::*;

    // ── parse_link : rôle parent ─────────────────────────────────────────────

    /// `[[parent:F-31]]` parse en `ProjectMapLink::Parent("F-31")`.
    #[test]
    fn parent_valid_ident_parses_to_parent() {
        assert_eq!(
            parse_link("parent:F-31"),
            Ok(ProjectMapLink::Parent("F-31".to_string()))
        );
    }

    /// `[[parent:F-100]]` : 3 chiffres acceptés.
    #[test]
    fn parent_three_digit_ident_is_accepted() {
        assert_eq!(
            parse_link("parent:F-100"),
            Ok(ProjectMapLink::Parent("F-100".to_string()))
        );
    }

    /// `[[parent:f-31]]` : minuscule `f` rejeté via `validate_feature_ident`.
    #[test]
    fn parent_lowercase_f_is_rejected() {
        assert_eq!(
            parse_link("parent:f-31"),
            Err(SchemaError::FeatureIdentInvalid("f-31".to_string()))
        );
    }

    /// `[[parent:foo]]` : format non-`F-\d{2,3}` rejeté.
    #[test]
    fn parent_invalid_format_is_rejected() {
        assert_eq!(
            parse_link("parent:foo"),
            Err(SchemaError::FeatureIdentInvalid("foo".to_string()))
        );
    }

    /// `[[parent:F-1]]` : 1 chiffre insuffisant.
    #[test]
    fn parent_single_digit_is_rejected() {
        assert_eq!(
            parse_link("parent:F-1"),
            Err(SchemaError::FeatureIdentInvalid("F-1".to_string()))
        );
    }

    /// `[[parent:F-1234]]` : 4 chiffres = trop long.
    #[test]
    fn parent_four_digits_is_rejected() {
        assert_eq!(
            parse_link("parent:F-1234"),
            Err(SchemaError::FeatureIdentInvalid("F-1234".to_string()))
        );
    }

    /// `[[parent:]]` : valeur vide rejetée par la garde `reserved && value.is_empty()`.
    #[test]
    fn parent_empty_value_is_rejected() {
        assert_eq!(
            parse_link("parent:"),
            Err(SchemaError::EmptyValue("parent".to_string()))
        );
    }

    // ── validate_links : cardinalité 0..N, ne déclenche pas d'erreur ─────────

    /// Carte-feature avec 1 parent → acceptée (cardinalité 0..N).
    #[test]
    fn feature_card_with_one_parent_is_valid() {
        let links = vec![
            ProjectMapLink::Feature("F-44".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
            ProjectMapLink::Parent("F-31".to_string()),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    /// Deux parents sur la même carte → accepté (0..N, cardinalité non limitée).
    #[test]
    fn feature_card_with_two_parents_does_not_trigger_cardinality_error() {
        let links = vec![
            ProjectMapLink::Feature("F-44".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
            ProjectMapLink::Parent("F-31".to_string()),
            ProjectMapLink::Parent("F-44".to_string()),
        ];
        // multi-parent : ne déclenche aucune erreur de cardinalité.
        assert_eq!(validate_links(&links), Ok(()));
    }

    /// `Parent` ne gonfle pas le compteur `feature_count` (ne fait pas basculer
    /// validate_links en mode carte-feature si aucun `Feature` n'est présent).
    #[test]
    fn parent_alone_does_not_trigger_feature_card_mode() {
        // Carte changelog (sans Feature) + 1 Parent → validate_links évalue en
        // mode changelog (0 feature_count) → le Parent est ignoré du comptage.
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Fix),
            ProjectMapLink::Parent("F-31".to_string()),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    // ── carte-feature COMPLÈTE avec parent → OK ──────────────────────────────

    /// Carte-feature complète (feature+project+status+kind:FEATURE+release+version)
    /// plus `[[parent:F-31]]` → validation OK.
    #[test]
    fn complete_feature_card_with_parent_is_valid() {
        let links = vec![
            ProjectMapLink::Feature("F-44".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "0.7.0".to_string(),
            },
            ProjectMapLink::Parent("F-31".to_string()),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    // ── NON-RÉGRESSION : cartes sans parent valident identiquement ────────────

    /// Carte-feature SANS parent reste valide (0 parent = ok).
    #[test]
    fn feature_card_without_parent_stays_valid() {
        let links = vec![
            ProjectMapLink::Feature("F-37".to_string()),
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::InProgress),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Release(ReleaseKind::Planned),
            ProjectMapLink::Version {
                project: "gradatum".to_string(),
                version: "backlog".to_string(),
            },
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    /// Carte changelog (sans Feature) + Supersedes + Parent → tous ignorés du
    /// comptage, carte valide si triple obligatoire présent.
    #[test]
    fn changelog_card_with_supersedes_and_parent_is_valid() {
        let links = vec![
            ProjectMapLink::Project("gradatum".to_string()),
            ProjectMapLink::Status(StatusKind::Done),
            ProjectMapLink::Kind(KindKind::Feature),
            ProjectMapLink::Supersedes("F-12".to_string()),
            ProjectMapLink::Parent("F-31".to_string()),
        ];
        assert_eq!(validate_links(&links), Ok(()));
    }

    // ── reserved_node_target : parent → nœud feature:F-YY ───────────────────

    /// `parent:F-31` dans reserved_node_target → `Some("feature:F-31")`.
    #[test]
    fn parent_maps_to_feature_reserved_node() {
        assert_eq!(
            reserved_node_target("parent:F-31"),
            Some("feature:F-31".to_string())
        );
    }

    /// `parent:f-31` invalide → `None` (parse échoue, reserved_node_target retourne None).
    #[test]
    fn parent_invalid_maps_to_none() {
        assert_eq!(reserved_node_target("parent:f-31"), None);
    }
}
