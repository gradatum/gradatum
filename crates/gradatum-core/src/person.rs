//! Structural validator for **person cards** (vault namespace `identity/person/<id>`).
//!
//! Module `person` — the descriptive counterpart of [`crate::soul`]. A person
//! card is *data about a human*, **referenced** by agents but **never executed** as a
//! system prompt. It is deliberately kept apart from an agent soul (`identity/<sub>`),
//! which *becomes* the agent's instructions at boot.
//!
//! The validator is **deterministic and LLM-free**, and enforces a **strict whitelist**
//! schema (invariant 7 of the person-card design):
//! - only named `key: value` fields drawn from `WHITELIST_FIELDS` are accepted;
//! - the mandatory `category` field is a closed enum (`PersonCategory`);
//! - reserved soul H2 sections (`## INVARIANTS`/`## GATES`/`## NARRATIVE`) are refused —
//!   a person card must never masquerade as executable instructions;
//! - unbounded free-text fields (`notes`/`about`/…) are refused (injection + PII vector);
//! - the body is size-capped (`PERSON_CARD_MAX_BYTES`) against DoS;
//! - GDPR Art.9 sensitive-PII field markers are refused **regardless of category**,
//!   coupled to the vault-at-rest encryption backlog.
//!
//! ## Category gate (this implementation)
//!
//! The schema is *structurally* categorial (four categories), but only `self` and
//! `private` are **open to writes** in this implementation. `public-figure` and
//! `domain-actor` are refused with a `400` until a dedicated review opens the category —
//! see `PersonSchemaError::CategoryNotOpen`.
//!
//! ## Dispatch is by PATH, never by body
//!
//! This validator is selected by the server on the note **path/title** prefix
//! (`identity/person/*`), never on a self-declared `category:`/`kind:` field of the body
//! (which would let an agent soul escape [`crate::soul::validate_soul`]).

use thiserror::Error;

/// Reserved namespace, under the `identity` section, that holds person cards.
///
/// The full note title is `identity/person/<id>`; once the leading `identity/` is
/// stripped server-side the remainder is `person/<id>`, hence this prefix.
///
/// The namespace is **reserved and non-attributable as a credential `sub`**: no
/// authenticated principal may ever carry a subject starting with `person/` (enforced at
/// API-key creation). This is what closes caveat C1 — writes to `identity/person/*` can
/// only travel through the privileged `identity_write` scope branch, never through the
/// `target == caller_sub` self-write branch, because no caller can *be* a `person/<id>`.
pub const PERSON_NAMESPACE_PREFIX: &str = "person/";

/// Maximum accepted person-card body size, in bytes (DoS safety cap, ADN 5).
///
/// A person card is a short set of whitelisted fields; 4 KiB is comfortably above any
/// legitimate profile while bounding the parse cost and the stored footprint.
pub const PERSON_CARD_MAX_BYTES: usize = 4096;

/// Whitelisted person-card field keys (lower-case), open to writes in this implementation.
///
/// Any other key is refused: unknown keys via [`PersonSchemaError::UnknownField`], the
/// explicit free-text keys via [`PersonSchemaError::FreeformFieldForbidden`], and Art.9
/// sensitive markers via [`PersonSchemaError::SensitivePii`]. Category-gated future
/// fields (`public_source`, `relation`) are intentionally **absent** until their
/// categories open.
pub const WHITELIST_FIELDS: &[&str] = &[
    "display_name",
    "pronouns",
    "category",
    "collaboration_prefs",
    "working_style",
    "context",
    "timezone",
    "locale",
];

/// Explicit unbounded free-text keys, refused to keep a person card structured and to
/// deny an injection/PII vector (invariant 7).
///
/// These are a subset of "not whitelisted" singled out for a precise error message; any
/// other unknown key is refused by the whitelist all the same.
const FREEFORM_FIELDS: &[&str] = &[
    "notes",
    "about",
    "bio",
    "biography",
    "description",
    "comment",
    "comments",
    "freeform",
    "free_text",
];

/// GDPR Art.9 "special category" markers, matched as a **substring of the field key**.
///
/// Any field whose key contains one of these is refused **regardless of `category`**,
/// coupled to the vault-at-rest encryption backlog (operator decision 2026-09-06).
///
/// Covered Art.9 categories: health, biometric, genetic, religious/philosophical beliefs,
/// political opinions, trade-union membership, sex life / sexual orientation, racial or
/// ethnic origin.
///
/// # Limitation (surfaced to the auditor)
///
/// Detection is **key-based** only. Sensitive content typed *inside* a free-text value of
/// an otherwise whitelisted field (e.g. `context: recovering from surgery`) is **not**
/// scanned here — value-level scanning is a heuristic with a false-positive cost and is
/// deferred to the encryption backlog. This validator refuses the obvious vector (a
/// dedicated sensitive field) without pretending to catch prose.
const SENSITIVE_MARKERS: &[&str] = &[
    "health",
    "medical",
    "diagnosis",
    "biometric",
    "genetic",
    "religion",
    "religious",
    "political",
    "opinion",
    "sexual",
    "orientation",
    "ethnicity",
    "ethnic",
    "racial",
    "union",
    "disability",
];

/// Reserved soul H2 section names (upper-case) that a person card must never carry.
///
/// Their presence signals an attempt to smuggle executable soul structure into a
/// descriptive card — refused at the write with [`PersonSchemaError::ReservedSection`].
const RESERVED_SECTIONS: &[&str] = &["INVARIANTS", "GATES", "NARRATIVE"];

/// The closed set of person-card categories (invariant 8 of the person-card design).
///
/// Structurally present in full so the schema never migrates; only [`Self::SelfProfile`]
/// and [`Self::Private`] are open to writes in this implementation (see
/// [`PersonCategory::is_write_open`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PersonCategory {
    /// `self` — the operator's own profile. Trivial consent, restricted read.
    SelfProfile,
    /// `private` — relatives/close peers, created by operator proxy. Restricted read.
    Private,
    /// `public-figure` — public information only. **Closed** until a dedicated review.
    PublicFigure,
    /// `domain-actor` — professional/semi-public actor. **Closed** until a dedicated review.
    DomainActor,
}

impl PersonCategory {
    /// Parses the `category:` value (case-insensitive) into a [`PersonCategory`].
    ///
    /// # Errors
    /// Returns [`PersonSchemaError::InvalidCategory`] for any value outside the closed set.
    fn parse(value: &str) -> Result<Self, PersonSchemaError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "self" => Ok(Self::SelfProfile),
            "private" => Ok(Self::Private),
            "public-figure" => Ok(Self::PublicFigure),
            "domain-actor" => Ok(Self::DomainActor),
            other => Err(PersonSchemaError::InvalidCategory(other.to_string())),
        }
    }

    /// Returns `true` if this category is open to writes in the current implementation.
    ///
    /// Only `self` and `private` are open; `public-figure`/`domain-actor` await a
    /// dedicated review.
    #[must_use]
    pub fn is_write_open(self) -> bool {
        matches!(self, Self::SelfProfile | Self::Private)
    }

    /// The canonical lower-case string form of the category.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelfProfile => "self",
            Self::Private => "private",
            Self::PublicFigure => "public-figure",
            Self::DomainActor => "domain-actor",
        }
    }
}

/// Reasons a person card fails the strict whitelist schema (invariant 7).
///
/// Every variant maps to an HTTP `400 InvalidInput` on the write path.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PersonSchemaError {
    /// The body exceeds [`PERSON_CARD_MAX_BYTES`].
    #[error("person: body too large ({size} bytes > {max} cap)")]
    TooLarge {
        /// Actual body size in bytes.
        size: usize,
        /// The configured cap ([`PERSON_CARD_MAX_BYTES`]).
        max: usize,
    },
    /// A line is neither a blank, an H1 title, nor a `key: value` pair.
    #[error("person: malformed line (expected `key: value`): {0}")]
    MalformedLine(String),
    /// A reserved soul H2 section (`## INVARIANTS`/`## GATES`/`## NARRATIVE`) was found.
    #[error("person: reserved soul section forbidden in a person card: ## {0}")]
    ReservedSection(String),
    /// A non-reserved markdown H2+ section header was found (schema is field-only).
    #[error("person: markdown section headers are not allowed in a person card: {0}")]
    MarkdownSectionForbidden(String),
    /// An unbounded free-text field (`notes`/`about`/…) was found.
    #[error("person: unbounded free-text field forbidden: {0}")]
    FreeformFieldForbidden(String),
    /// A field carrying a GDPR Art.9 sensitive-PII marker was found.
    #[error("person: GDPR Art.9 sensitive field refused (coupled to encryption backlog): {0}")]
    SensitivePii(String),
    /// A field key is not part of [`WHITELIST_FIELDS`].
    #[error("person: field not in whitelist: {0}")]
    UnknownField(String),
    /// The mandatory `category` field is absent.
    #[error("person: mandatory `category` field missing")]
    MissingCategory,
    /// The `category` value is outside the closed set.
    #[error("person: invalid category value: {0}")]
    InvalidCategory(String),
    /// The category is valid but not open to writes in this implementation.
    #[error("person: category `{0}` is not open — a dedicated review is required to open it")]
    CategoryNotOpen(&'static str),
}

/// Validates a person-card body against the strict whitelist schema (invariant 7).
///
/// Selected by the server **on the note path** (`identity/person/*`), never on a body
/// field. On success the card is a well-formed, size-bounded set of whitelisted fields
/// carrying an **open** category (`self`/`private`).
///
/// # Errors
///
/// Returns [`PersonSchemaError`] when the body exceeds the size cap, contains a malformed
/// line, a reserved soul section, any markdown section header, an unbounded free-text
/// field, a GDPR Art.9 sensitive field, a non-whitelisted field, an absent/invalid
/// `category`, or a category not open to writes.
#[must_use = "the validation result must be checked; ignoring it defeats the schema guard"]
pub fn validate_person_card(body: &str) -> Result<(), PersonSchemaError> {
    // Safety cap first — bound the work before parsing (ADN 5, DoS).
    if body.len() > PERSON_CARD_MAX_BYTES {
        return Err(PersonSchemaError::TooLarge {
            size: body.len(),
            max: PERSON_CARD_MAX_BYTES,
        });
    }

    let mut category: Option<PersonCategory> = None;

    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        // H2+ section header — reserved soul sections give a precise error, any other
        // markdown section is refused too (the schema is field-only, not free markdown).
        if line.starts_with("##") {
            let name = line.trim_start_matches('#').trim();
            let upper = name.to_ascii_uppercase();
            if RESERVED_SECTIONS.iter().any(|r| upper.starts_with(r)) {
                return Err(PersonSchemaError::ReservedSection(name.to_string()));
            }
            return Err(PersonSchemaError::MarkdownSectionForbidden(
                name.to_string(),
            ));
        }

        // H1 title line (`# identity/person/<id>`) — tolerated, carries no field.
        if line.starts_with("# ") || line == "#" {
            continue;
        }

        // Every remaining line must be a `key: value` pair.
        let (key_raw, _value) = match line.split_once(':') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => return Err(PersonSchemaError::MalformedLine(line.to_string())),
        };

        // Art.9 sensitive marker — checked before the whitelist for a precise error.
        if SENSITIVE_MARKERS.iter().any(|m| key_raw.contains(m)) {
            return Err(PersonSchemaError::SensitivePii(key_raw));
        }

        // Explicit unbounded free-text keys — precise error before the generic whitelist.
        if FREEFORM_FIELDS.contains(&key_raw.as_str()) {
            return Err(PersonSchemaError::FreeformFieldForbidden(key_raw));
        }

        // Strict whitelist.
        if !WHITELIST_FIELDS.contains(&key_raw.as_str()) {
            return Err(PersonSchemaError::UnknownField(key_raw));
        }

        if key_raw == "category" {
            category = Some(PersonCategory::parse(_value)?);
        }
    }

    let category = category.ok_or(PersonSchemaError::MissingCategory)?;

    // Category gate: only `self`/`private` are open to writes in this implementation.
    if category.is_write_open() {
        Ok(())
    } else {
        Err(PersonSchemaError::CategoryNotOpen(category.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_SELF: &str = "\
# identity/person/ada
display_name: Ada Lovelace
pronouns: il/lui
category: self
timezone: Europe/Paris
collaboration_prefs: réponses concises, français
";

    #[test]
    fn valid_self_card_accepted() {
        validate_person_card(GOOD_SELF).expect("carte self valide doit être acceptée");
    }

    #[test]
    fn valid_private_card_accepted() {
        let body =
            "# identity/person/alice\ndisplay_name: Alice\ncategory: private\ncontext: proche\n";
        validate_person_card(body).expect("carte private valide doit être acceptée");
    }

    #[test]
    fn public_figure_category_rejected() {
        let body = "display_name: X\ncategory: public-figure\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::CategoryNotOpen("public-figure"))
        ));
    }

    #[test]
    fn domain_actor_category_rejected() {
        let body = "display_name: X\ncategory: domain-actor\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::CategoryNotOpen("domain-actor"))
        ));
    }

    #[test]
    fn invalid_category_rejected() {
        let body = "category: overlord\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::InvalidCategory(_))
        ));
    }

    #[test]
    fn missing_category_rejected() {
        let body = "display_name: X\npronouns: they/them\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::MissingCategory)
        ));
    }

    #[test]
    fn reserved_gates_section_rejected() {
        let body = "category: self\n## GATES\nGATE-X | do thing\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::ReservedSection(_))
        ));
    }

    #[test]
    fn reserved_invariants_section_rejected() {
        let body = "category: self\n## INVARIANTS\nINV-CANARY | x\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::ReservedSection(_))
        ));
    }

    #[test]
    fn non_reserved_markdown_section_rejected() {
        let body = "category: self\n## Notes libres\ndu texte\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::MarkdownSectionForbidden(_))
        ));
    }

    #[test]
    fn art9_health_field_rejected() {
        let body = "category: self\nhealth: diabète\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::SensitivePii(_))
        ));
    }

    #[test]
    fn art9_religion_field_rejected() {
        let body = "category: private\nreligion: none\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::SensitivePii(_))
        ));
    }

    #[test]
    fn freeform_notes_field_rejected() {
        let body = "category: self\nnotes: anything unbounded here\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::FreeformFieldForbidden(_))
        ));
    }

    #[test]
    fn freeform_about_field_rejected() {
        let body = "category: self\nabout: unbounded prose\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::FreeformFieldForbidden(_))
        ));
    }

    #[test]
    fn unknown_field_rejected() {
        let body = "category: self\nsalary: 100000\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::UnknownField(_))
        ));
    }

    #[test]
    fn oversized_body_rejected() {
        // A whitelisted field repeated past the cap — proves the size guard, not a field guard.
        let mut body = String::from("category: self\n");
        while body.len() <= PERSON_CARD_MAX_BYTES {
            body.push_str("context: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n");
        }
        assert!(matches!(
            validate_person_card(&body),
            Err(PersonSchemaError::TooLarge { .. })
        ));
    }

    #[test]
    fn malformed_line_rejected() {
        let body = "category: self\nthis line has no colon\n";
        assert!(matches!(
            validate_person_card(body),
            Err(PersonSchemaError::MalformedLine(_))
        ));
    }

    #[test]
    fn category_parse_is_case_insensitive() {
        let body = "category: SELF\ndisplay_name: X\n";
        validate_person_card(body).expect("category est insensible à la casse");
    }

    #[test]
    fn value_with_colon_kept_whole() {
        // `timezone: Europe/Paris` and a value containing a colon must not break parsing.
        let body = "category: self\ntimezone: Europe/Paris\ncontext: voir note: rien\n";
        validate_person_card(body).expect("une valeur contenant `:` reste valide");
    }
}
