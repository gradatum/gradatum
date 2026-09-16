//! Gradatum note tag — validated `String` newtype.
//!
//! A tag is a lowercase string matching `^[a-z0-9][a-z0-9-]{0,63}$`.
//! Validation is performed manually to avoid a `regex` dependency in the core crate.

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

/// Tag for a Gradatum note.
///
/// Validated `String` newtype — validation occurs in the constructor.
/// Format: `^[a-z0-9][a-z0-9-]{0,63}$` (lowercase, digits, hyphens, 1–64 chars).
///
/// ## Examples
///
/// ```
/// use gradatum_core::tag::Tag;
///
/// let t = Tag::new("knowledge-base").unwrap();
/// assert_eq!(t.as_str(), "knowledge-base");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tag(String);

impl Tag {
    /// Constructs a `Tag` from a string, validating its format.
    ///
    /// # Errors
    ///
    /// Returns `ValidationError::InvalidTag` if:
    /// - The string is empty or exceeds 64 characters.
    /// - The first character is not `[a-z0-9]`.
    /// - Any subsequent character is outside `[a-z0-9-]`.
    /// - The string names a reserved tag, in any spelling that coerces onto one
    ///   (see [`ReservedTag`]).
    pub fn new(s: impl Into<String>) -> Result<Self, ValidationError> {
        let s = s.into();

        // Longueur : 1–64 caractères.
        if s.is_empty() || s.len() > 64 {
            return Err(ValidationError::InvalidTag(s));
        }

        // F-160 — the reservation guard, on the **coerced** form, and ahead of the
        // format checks.
        //
        // On the coerced form because the inputs that matter here are exactly the ones
        // the format checks let through: `personal--classified` is well-formed and only
        // becomes the reserved name once `-` runs collapse. A literal comparison would
        // miss it. Ahead of them so that a refusal is a refusal, not a format
        // complaint — the length bound above is what keeps the coercion's allocation
        // bounded.
        if coerce(&s).is_some_and(|coerced| is_reserved_literal(&coerced)) {
            return Err(ValidationError::InvalidTag(s));
        }

        let bytes = s.as_bytes();

        // Premier caractère : [a-z0-9] uniquement (pas de tiret en tête).
        if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
            return Err(ValidationError::InvalidTag(s));
        }

        // Caractères suivants : [a-z0-9-].
        for &b in bytes.iter().skip(1) {
            if !b.is_ascii_lowercase() && !b.is_ascii_digit() && b != b'-' {
                return Err(ValidationError::InvalidTag(s));
            }
        }

        Ok(Tag(s))
    }

    /// Returns the tag value as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Coerces an arbitrary string into a valid `Tag` instead of rejecting it.
    ///
    /// The algorithm never fails; it returns `None` only when nothing usable is left:
    ///
    /// 1. Lowercase the whole input.
    /// 2. Replace every run of characters outside `[a-z0-9]` with a **single** `-`.
    /// 3. Trim leading and trailing `-` (implicitly: invalid runs at either end are
    ///    never emitted).
    /// 4. Truncate to 64 characters, then drop a trailing `-` if the truncation landed
    ///    in the middle of a run.
    /// 5. If the result is empty, return `None` (nothing recoverable in the input).
    /// 6. If the result names a reserved tag (see [`ReservedTag`]), return `None`.
    /// 7. Otherwise return `Some(Tag(result))`. The produced value always satisfies
    ///    [`Tag::new`].
    ///
    /// [`Tag::new`] (strict validation) is left untouched for call sites that want to
    /// reject non-conforming input outright. The two constructors agree on which
    /// inputs they refuse: both compare the **coerced** form to the reserved names.
    ///
    /// ## Examples
    ///
    /// ```
    /// use gradatum_core::tag::Tag;
    ///
    /// assert_eq!(Tag::normalize("status:OPEN").as_ref().map(Tag::as_str), Some("status-open"));
    /// assert_eq!(Tag::normalize("v0.5.3").as_ref().map(Tag::as_str), Some("v0-5-3"));
    /// assert_eq!(Tag::normalize("P1").as_ref().map(Tag::as_str), Some("p1"));
    /// assert_eq!(Tag::normalize("___"), None);
    /// assert_eq!(Tag::normalize(""), None);
    /// // Already-valid tags are returned unchanged.
    /// assert_eq!(Tag::normalize("knowledge-base").as_ref().map(Tag::as_str), Some("knowledge-base"));
    /// // A reserved name is not recovered from any of its spellings.
    /// assert_eq!(Tag::normalize("personal-classified"), None);
    /// assert_eq!(Tag::normalize("Personal Classified"), None);
    /// // …but a tag that merely names one is ordinary.
    /// assert_eq!(
    ///     Tag::normalize("Personal Classified 2026").as_ref().map(Tag::as_str),
    ///     Some("personal-classified-2026"),
    /// );
    /// ```
    pub fn normalize(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        let result = coerce(&s)?;

        // F-160 — the same reservation as in `Tag::new`, on the other constructor.
        // `None` rather than a coerced tag: a caller asking the product to make a tag
        // out of a reserved name gets nothing back, exactly as if the input had held
        // no usable character at all. Both sides consume `coerce`, which is what makes
        // the two guards agree on their input set instead of agreeing by inspection.
        if is_reserved_literal(&result) {
            return None;
        }

        // Invariant : la valeur produite satisfait Tag::new par construction :
        // - longueur 1–64 (garantie ci-dessus)
        // - premier char ∈ [a-z0-9] (jamais '-' car skip en tête)
        // - chars suivants ∈ [a-z0-9-]
        debug_assert!(
            Self::new(result.as_str()).is_ok(),
            "Tag::normalize produced invalid tag: {:?}",
            result
        );

        Some(Tag(result))
    }
}

/// Validates a caller-supplied **tag query** value — a tag *name being asked about*,
/// never a `Tag` being built.
///
/// That distinction is the whole of this function. [`Tag::new`] refuses both reserved
/// names, deliberately: a client must not be able to **pose** a capability. It must,
/// however, be able to **ask** which notes carry one — that is the rescan surface of the
/// privacy lot, and the read guard, not the parser, is what decides whether the caller
/// may see what comes back. A validator that answered `None` here would turn a legitimate
/// query into a `400` and put the operator's rescan out of reach; a validator that
/// answered `Some` for everything would push a malformed name down to the SQL and read
/// as an empty vault rather than as a malformed request.
///
/// The accepted form is [`Tag::new`]'s, on the **raw** spelling: no coercion. The two
/// reserved names are accepted spelled exactly as they are reserved, so a spelling that
/// merely coerces onto one (`Personal Classified`) is refused like any other
/// non-conforming input — the match downstream is by exact token, and a coerced query
/// would name a tag no note carries.
///
/// Returns `Some(name)` — the validated name, unchanged — or `None`.
///
/// ## Examples
///
/// ```
/// use gradatum_core::tag::parse_tag_query;
///
/// assert_eq!(parse_tag_query("knowledge-base"), Some("knowledge-base"));
/// // The two reserved names are legitimate *queries*, even though `Tag::new` refuses them.
/// assert_eq!(parse_tag_query("personal-classified"), Some("personal-classified"));
/// assert_eq!(parse_tag_query("privacy-unscanned"), Some("privacy-unscanned"));
/// // The form, not the reservation, is what this rejects.
/// assert_eq!(parse_tag_query("Personal Classified"), None);
/// assert_eq!(parse_tag_query("knowledge%base"), None);
/// assert_eq!(parse_tag_query("knowledge_base"), None);
/// ```
pub fn parse_tag_query(s: &str) -> Option<&str> {
    // The reservation is compared on the **raw** spelling here, where `Tag::new`
    // compares it on the coerced one. Not an inconsistency: `Tag::new` guards a write,
    // so anything that could ever coerce onto a reserved name must be refused, while
    // this function asks whether a *query* names a tag a note can actually carry — and
    // the downstream match is by exact token. On the raw spelling the two answers this
    // function owes fall out of the same comparison: `personal-classified` is a
    // legitimate question, `Personal Classified` is not, because a note's tags are
    // stored as written and no note carries the latter.
    if ReservedTag::ALL.iter().any(|reserved| reserved.0 == s) {
        return Some(s);
    }

    // Everything else is exactly the write-side format, on the raw spelling. Reusing
    // `Tag::new` rather than re-deriving the format from the doc-comment is what keeps
    // the two in step: a second spelling of `^[a-z0-9][a-z0-9-]{0,63}$` would be a
    // second thing to keep true.
    Tag::new(s).is_ok().then_some(s)
}

/// Coerces an arbitrary string into a valid tag literal, or `None` when nothing usable
/// is left.
///
/// Factored out so that [`Tag::new`] and [`Tag::normalize`] cannot drift: `new` refuses
/// the result when it names a reserved tag, `normalize` returns `None` for it, and both
/// read the same algorithm. Expressing one in terms of the other would not do —
/// `Tag::new` returning `normalize(s).ok()` would silently coerce instead of reject,
/// and `normalize` asserting through `new` would recurse in debug builds.
///
/// The result never exceeds 64 bytes and always matches `^[a-z0-9][a-z0-9-]*$`.
fn coerce(s: &str) -> Option<String> {
    // Step 1: lowercase.
    let lower = s.to_lowercase();

    // Steps 2-3: replace every run of characters outside `[a-z0-9]` with a single `-`,
    // trimming implicitly. One pass: a `-` is only emitted at an invalid→valid
    // boundary, and only when `result` is non-empty (which forbids a leading dash).
    let mut result = String::with_capacity(lower.len());
    let mut in_invalid_run = false;

    for ch in lower.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            // Valid character — flush the pending dash if a run just ended.
            if in_invalid_run {
                // `result` is non-empty here: a leading dash is impossible.
                if !result.is_empty() {
                    result.push('-');
                }
                in_invalid_run = false;
            }
            result.push(ch);
        } else {
            // Invalid character — mark the run, do not emit it.
            in_invalid_run = true;
        }
    }
    // `in_invalid_run` left true at the end of the loop → trailing invalid run,
    // ignored → implicit trim of trailing invalid characters.

    if result.is_empty() {
        return None;
    }

    // Step 4: truncate to 64 bytes.
    // `result` is 100% ASCII (`[a-z0-9-]`) by now, so `truncate` is byte-safe.
    if result.len() > 64 {
        result.truncate(64);
        // Re-trim if the truncation left a trailing dash.
        let new_len = result.trim_end_matches('-').len();
        result.truncate(new_len);
    }

    if result.is_empty() {
        return None;
    }

    Some(result)
}

/// Whether a coerced literal is one of the reserved tag names.
fn is_reserved_literal(coerced: &str) -> bool {
    ReservedTag::ALL
        .iter()
        .any(|reserved| reserved.0 == coerced)
}

/// A tag name the personal-data frontier reserves — a capability, not a value.
///
/// Two names are not ordinary tags: `personal-classified` is the verdict a completed
/// detection pass leaves on a note holding personal data, and `privacy-unscanned` is
/// the operational marker of a pass that did not complete. Only a pass, or the review
/// surface, may pose them — and a client-supplied string must never be able to.
///
/// That reservation is closed at the [`Tag`] constructor: [`Tag::new`] refuses every
/// spelling that coerces onto a reserved name and [`Tag::normalize`] returns `None`,
/// so **every** write path that turns caller input into a tag is covered, whatever it
/// does with the result. This type is what remains reachable from the inside: it has a
/// private field and no general constructor, so a `ReservedTag` is only ever one of
/// the two associated constants below, and posing a reserved name is reachable only
/// from code that names it. [`From<ReservedTag> for Tag`] is the single sanctioned
/// bridge back.
///
/// A third door is deliberately left open: `Tag`'s `Deserialize` does **not** validate.
/// It is what carries a marker already written on disk back into memory, and a
/// validating `Deserialize` would retire that marker on the next re-read of a note no
/// pass is running on — which is the ordinary state of an installation.
///
/// [`From<ReservedTag> for Tag`]: From
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ReservedTag(&'static str);

impl ReservedTag {
    /// The privacy verdict: a completed pass judged the note to hold personal data of
    /// an activated category.
    pub const PERSONAL_CLASSIFIED: Self = Self("personal-classified");

    /// The operational marker: a pass ran but did not complete, so a rescan must be
    /// able to find the note again.
    pub const PRIVACY_UNSCANNED: Self = Self("privacy-unscanned");

    /// Every reserved tag — the set both constructors compare against.
    ///
    /// One list, so that a third reserved name cannot be added to one constructor and
    /// forgotten in the other.
    const ALL: [Self; 2] = [Self::PERSONAL_CLASSIFIED, Self::PRIVACY_UNSCANNED];
}

impl From<ReservedTag> for Tag {
    /// The one sanctioned bridge from a reserved name to a [`Tag`].
    ///
    /// Infallible by construction: the value comes from an associated constant, and
    /// the constants are valid tag literals. It builds the `Tag` directly rather than
    /// through [`Tag::new`], on purpose — this *is* the sanctioned pose path, and the
    /// constructor's guard is what refuses every other one.
    fn from(reserved: ReservedTag) -> Self {
        Tag(reserved.0.to_owned())
    }
}

impl AsRef<str> for ReservedTag {
    /// The canonical spelling of this reserved tag.
    fn as_ref(&self) -> &str {
        self.0
    }
}

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Tag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ── Tests ciblés : cas limites multibyte / Unicode ────────────────────────

    /// Séquence uniquement invalide → None.
    #[test]
    fn normalize_all_invalid_chars_returns_none() {
        assert_eq!(Tag::normalize("___"), None);
        assert_eq!(Tag::normalize("!!!"), None);
        assert_eq!(Tag::normalize(""), None);
        // Emoji seul → None (aucun char ASCII valide).
        assert_eq!(Tag::normalize("🦀"), None);
        assert_eq!(Tag::normalize("🦀🦀🦀"), None);
    }

    /// Chaîne multibyte > 64 octets : la troncature doit rester sur frontière char.
    #[test]
    fn normalize_multibyte_truncate_char_boundary_safe() {
        // Chaque 'é' (U+00E9) pèse 2 octets en UTF-8.
        // 40 'a' + 20 'é' = 40 + 40 = 80 octets → > 64.
        // normalize va kebab-ifier les 'é' en '-', donc le résultat sera 100% ASCII —
        // la troncature à 64 est sûre. On vérifie surtout l'absence de panique.
        let long_multibyte = "a".repeat(20) + &"é".repeat(20);
        let result = Tag::normalize(long_multibyte);
        // Doit soit retourner Some avec len ≤ 64 et satisfaire Tag::new, soit None.
        if let Some(ref t) = result {
            assert!(t.as_str().len() <= 64, "longueur > 64 : {}", t.as_str());
            Tag::new(t.as_str()).expect("normalize doit produire un tag valide");
        }
    }

    /// Chaîne avec emoji intercalés : aucun panic, résultat toujours valide.
    #[test]
    fn normalize_mixed_ascii_emoji_no_panic() {
        let inputs = [
            "hello🦀world",
            "🦀leading",
            "trailing🦀",
            "a🦀b🦀c",
            "rust🦀🦀🦀lang",
            "αβγabc", // chars multibyte non-ASCII
        ];
        for input in &inputs {
            let result = Tag::normalize(*input);
            if let Some(ref t) = result {
                Tag::new(t.as_str()).expect("normalize doit produire un tag valide");
                assert!(t.as_str().len() <= 64);
            }
        }
    }

    // ── F-160 — les deux noms réservés (D-12, P0-3) ─────────────────────────

    /// The six spellings the design names, for each reserved name, through **both**
    /// constructors: `Tag::new` returns `InvalidTag`, `Tag::normalize` returns `None`.
    ///
    /// The spellings are spelled out rather than derived from the reserved names: a
    /// derived form drifts from what the design names, and it is the named spellings
    /// that the guard is held to.
    #[test]
    fn six_normalised_forms_of_each_reserved_name_are_rejected() {
        let personal = [
            "Personal Classified",
            "PERSONAL_CLASSIFIED",
            "personal.classified",
            "personal--classified",
            " personal classified",
            "personal-classified",
        ];
        let unscanned = [
            "Privacy Unscanned",
            "PRIVACY_UNSCANNED",
            "privacy.unscanned",
            "privacy--unscanned",
            " privacy unscanned",
            "privacy-unscanned",
        ];

        for form in personal.iter().chain(unscanned.iter()) {
            assert!(
                matches!(Tag::new(*form), Err(ValidationError::InvalidTag(_))),
                "Tag::new must refuse the reserved spelling {form:?}"
            );
            assert_eq!(
                Tag::normalize(*form),
                None,
                "Tag::normalize must coerce the reserved spelling {form:?} to nothing"
            );
        }
    }

    /// The guard closes two names, not a prefix: a distinct tag that merely mentions
    /// one is an ordinary tag, through both constructors.
    #[test]
    fn personal_classified_2026_is_accepted() {
        assert_eq!(
            Tag::new("personal-classified-2026").map(|t| t.as_str().to_owned()),
            Ok("personal-classified-2026".to_owned())
        );
        assert_eq!(
            Tag::normalize("Personal Classified 2026").map(|t| t.as_str().to_owned()),
            Some("personal-classified-2026".to_owned())
        );
        assert_eq!(
            Tag::normalize("privacy-unscanned-later").map(|t| t.as_str().to_owned()),
            Some("privacy-unscanned-later".to_owned())
        );
        // Anti-vacuity: the two reserved names are still *reachable*, so the refusals
        // asserted above are a reservation and not a ban on the words. They are
        // reachable through exactly one path — the associated constants — which is the
        // property that makes the reservation a capability.
        assert_eq!(
            Tag::from(ReservedTag::PERSONAL_CLASSIFIED).as_str(),
            "personal-classified"
        );
        assert_eq!(
            Tag::from(ReservedTag::PRIVACY_UNSCANNED).as_str(),
            "privacy-unscanned"
        );
        assert_eq!(
            ReservedTag::PERSONAL_CLASSIFIED.as_ref(),
            "personal-classified"
        );
        assert_eq!(ReservedTag::PRIVACY_UNSCANNED.as_ref(), "privacy-unscanned");
    }

    /// A confusable does **not** forge a reserved name.
    ///
    /// The property holds because `Tag::normalize` applies no NFKC and projects every
    /// byte outside `[a-z0-9]` onto `-`: a confusable therefore *degrades*
    /// (`personal-clаssified` → `personal-cl-ssified`) instead of forging the reserved
    /// name. The guard compares the **output** of that projection, so its completeness
    /// rests on no enumeration of inputs.
    ///
    /// Sentinel, and this comment is its reason: read as a case list it looks
    /// redundant, and a maintainer would delete it as such. It is the only thing
    /// watching the projection. The day `normalize` gains a Unicode transliteration
    /// (`é` → `e`, fullwidth `ｐ` → `p`), the property falls **without a single line of
    /// F-160 changing** — and this test is what turns that day red.
    #[test]
    fn confusable_does_not_forge_a_reserved_tag() {
        // Cyrillic `а` (U+0430) standing in for the ASCII `a` of `personal-classified`.
        let cyrillic = "personal-clаssified";
        assert_ne!(cyrillic, "personal-classified");
        assert_eq!(
            Tag::normalize(cyrillic).map(|t| t.as_str().to_owned()),
            Some("personal-cl-ssified".to_owned()),
            "a confusable degrades into a distinct tag, it does not forge the reserved one"
        );
        assert!(Tag::new(cyrillic).is_err(), "non-ASCII is out of format");

        // Fullwidth `ｐ` (U+FF50) is the sharper sentinel: an NFKC pass would fold it to
        // ASCII `p` and produce the reserved name exactly.
        let fullwidth = "ｐersonal-classified";
        assert_ne!(
            Tag::normalize(fullwidth).map(|t| t.as_str().to_owned()),
            Some("personal-classified".to_owned()),
            "without NFKC the fullwidth initial degrades instead of folding"
        );
        // It degrades by *dropping* the byte it cannot project — `ersonal-classified`,
        // a distinct tag that nobody reserved.
        assert_eq!(
            Tag::normalize(fullwidth).map(|t| t.as_str().to_owned()),
            Some("ersonal-classified".to_owned())
        );
        assert!(Tag::new(fullwidth).is_err(), "non-ASCII is out of format");
    }

    // ── F-160 — la valeur de requête `tag` de `vault_list` ───────────────────

    /// `parse_tag_query` accepts the two reserved names and nothing else that
    /// [`Tag::new`] refuses — the pair the tag filter is held to.
    ///
    /// The reserved half is the point: it is what makes the rescan surface reachable at
    /// all. The other half is what keeps a malformed query from reaching SQL, where it
    /// would return an empty page instead of naming the caller's mistake.
    #[test]
    fn parse_tag_query_accepts_the_reserved_names_and_the_tag_form_only() {
        for accepted in [
            "knowledge-base",
            "p1",
            "personal-classified-2026",
            // The two reserved names, spelled as they are reserved.
            "personal-classified",
            "privacy-unscanned",
        ] {
            assert_eq!(
                parse_tag_query(accepted),
                Some(accepted),
                "a legitimate query value must be accepted unchanged"
            );
        }

        for refused in [
            "",                    // empty
            "Knowledge-Base",      // uppercase
            "Personal Classified", // a *coercion* onto a reserved name — not the name
            "PERSONAL_CLASSIFIED",
            "personal--classified",
            "personal classified",
            "knowledge%base",  // LIKE wildcard
            "knowledge_base",  // LIKE single-character wildcard
            "knowledge\\base", // LIKE escape character
            "knowledge.base",
            "-leading",
        ] {
            assert_eq!(
                parse_tag_query(refused),
                None,
                "a query value outside the tag form must be refused: {refused:?}"
            );
        }
    }

    /// The reservation refuses a `Tag`, never a *question* about one: the two names the
    /// constructors close are exactly the two `parse_tag_query` opens.
    ///
    /// Anti-vacuity for the test above: without it, a `parse_tag_query` that simply
    /// refused the two reserved names would satisfy every "refused" case there while
    /// closing the very surface L-D exists to open.
    #[test]
    fn parse_tag_query_opens_exactly_what_tag_new_closes() {
        for reserved in [
            ReservedTag::PERSONAL_CLASSIFIED,
            ReservedTag::PRIVACY_UNSCANNED,
        ] {
            let literal = reserved.as_ref();
            assert!(
                Tag::new(literal).is_err(),
                "the constructor closes {literal:?} — the premise of this test"
            );
            assert_eq!(
                parse_tag_query(literal),
                Some(literal),
                "{literal:?} is a legitimate query value"
            );
        }
    }

    // ── Proptest : propriétés universelles ───────────────────────────────────

    proptest! {
        /// `Tag::normalize` ne panique JAMAIS sur une entrée arbitraire (incluant multibyte).
        #[test]
        fn prop_normalize_never_panics(s in ".*") {
            let _ = Tag::normalize(s);
        }

        /// Si `normalize` retourne `Some(t)`, alors `t` passe toujours `Tag::new` (round-trip).
        #[test]
        fn prop_normalize_some_satisfies_tag_new(s in ".*") {
            if let Some(t) = Tag::normalize(s) {
                prop_assert!(
                    Tag::new(t.as_str()).is_ok(),
                    "normalize produced invalid tag: {:?}",
                    t.as_str()
                );
            }
        }

        /// Si `normalize` retourne `Some(t)`, la longueur est ≤ 64 caractères ET ≤ 64 octets.
        /// (résultat 100% ASCII → les deux coïncident, mais on vérifie les deux explicitement)
        #[test]
        fn prop_normalize_some_len_at_most_64(s in ".*") {
            if let Some(t) = Tag::normalize(s) {
                prop_assert!(
                    t.as_str().len() <= 64,
                    "tag trop long: {:?} ({} octets)",
                    t.as_str(),
                    t.as_str().len()
                );
                prop_assert!(
                    t.as_str().chars().count() <= 64,
                    "tag trop long en chars: {:?}",
                    t.as_str()
                );
            }
        }

        /// La troncature ne coupe JAMAIS sur une frontière non-char : le résultat est
        /// toujours UTF-8 valide (implicite car `String`, mais on vérifie via `chars()`).
        #[test]
        fn prop_normalize_result_is_valid_utf8(s in ".*") {
            if let Some(t) = Tag::normalize(s) {
                // `chars()` panique si la string n'est pas UTF-8 valide.
                let _ = t.as_str().chars().count();
            }
        }

        /// `Tag::normalize` est un point fixe sur son propre output (self-idempotency).
        ///
        /// Pour toute entrée `s`, si `normalize(s) == Some(t)`,
        /// alors `normalize(t.as_str()) == Some(t)` (un second passage ne change rien).
        ///
        /// Note : un tag *valide pour `Tag::new`* n'est PAS nécessairement inchangé
        /// par `normalize` (ex : `a--b` est accepté par `Tag::new` mais normalisé en `a-b`
        /// car les tirets consécutifs constituent un "run invalide" pour l'algorithme).
        /// Cette propriété teste le seul invariant correct : l'output de `normalize`
        /// est stable sous un second appel.
        #[test]
        fn prop_normalize_idempotent_on_valid_tags(s in ".*") {
            if let Some(t) = Tag::normalize(s) {
                // Second passage sur l'output de normalize.
                let second = Tag::normalize(t.as_str());
                prop_assert_eq!(
                    second.as_ref().map(|u| u.as_str()),
                    Some(t.as_str()),
                    "normalize n'est pas un point fixe : {:?} → {:?}",
                    t.as_str(),
                    second.as_ref().map(|u| u.as_str()),
                );
            }
        }
    }
}
