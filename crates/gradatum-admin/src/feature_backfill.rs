//! `gradatum-admin project-map backfill-features` — bulk creation of feature cards
//! in the `project-map` vault section from `features.ts`.
//!
//! ## Modes
//!
//! - **Dry-run** (default): prints each payload (title + body wikilinks +
//!   source marker) without calling `POST /api/v1/vault_write`. The report
//!   shows `would_create`. **No network calls, no JWT required.**
//! - **Live** (`--apply`): exchanges the api-key for a JWT, then for each
//!   card: checks idempotency via `marker_exists`, writes if absent.
//!
//! ## Idempotency
//!
//! Each card embeds a `pm-feature-source:F-XX` marker in its body.
//! `marker_exists` searches for it via `vault_search` (+ fallback `vault_read` on the
//! full body when the FTS5 snippet truncates the marker) before any write — second run = skip.
//!
//! ## Schema compliance
//!
//! Every generated card satisfies the project-map schema, which requires exactly one of
//! each of the following links:
//! `[[feature:F-XX]] [[project:gradatum]] [[status:<S>]] [[kind:FEATURE]]
//!  [[release:<R>]] [[version:gradatum/x.y.z]]`
//!
//! Base mapping, straight from `features.ts`:
//! - `released` → `status:DONE` and `release:released`
//! - `planned` → `status:OPEN` and `release:planned`
//! - `vX.Y.Z` → `version:gradatum/x.y.z`, stripping the leading `v`
//!
//! ## The in-place overlay layer
//!
//! [`crate::feature_backfill::apply_amendment_overlay`] never appends a card: it rewrites
//! some of the parsed ones and leaves the count untouched. It exists because the website
//! expresses only two delivery states, `released` and `planned`, while the vault schema
//! has a wider `release` axis.
//!
//! ### Release-axis override: `planned` → `roadmap`
//!
//! Fourteen cards are declared `planned` on the website but genuinely belong to the
//! roadmap. For each of them only `release_wire` is flipped; `status_wire` stays `OPEN`
//! and `display_version` stays exactly as sourced from the website. A target identifier
//! that cannot be found is a hard error, so that a website change can never make an
//! override silently disappear.
//!
//! Cards already marked `released` on the website are deliberately absent from the
//! override table: `features.ts` carries the correct state, and the plain
//! `FeatureCardSpec::from` conversion already yields the right axis.
//!
//! ### The backlog display sentinel
//!
//! A `roadmap` card has the wire value `gradatum/backlog` but shows the literal `vX.Y.Z`
//! in its title, so it stays visible on the website instead of being filtered out as
//! version-less. [`crate::feature_backfill::map_version`] maps `"vX.Y.Z"` onto
//! `"gradatum/backlog"`, and the feature export performs the inverse mapping on the
//! website side.
//!
//! ### The `[[parent:F-YY]]` link
//!
//! A card that continues an earlier feature carries a `[[parent:F-XX]]` link naming that
//! original feature. It is appended at the end of the wikilink line.
//!
//! ## DRY reuse
//!
//! [`crate::changelog_backfill::VaultWriteClient`], [`crate::changelog_backfill::HttpVaultClient`], [`crate::project_map_card::VaultWriteCard`] are imported
//! from [`crate::changelog_backfill`] — no HTTP client duplication.
//!
//! # Errors
//!
//! Returns `Err` if:
//! - `apply == true` and `api_key` is empty (guard before any network access);
//! - parsing `features.ts` fails or the count differs from `expected_count`;
//! - an HTTP call produces a non-recoverable error.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::changelog_backfill::{HttpVaultClient, VaultWriteClient};
use crate::project_map_card::{VaultWriteCard, build_title_index, normalize_title};

/// Number of features `features.ts` is expected to declare.
///
/// Safety guard: a parse returning any other count raises an explicit error, which
/// prevents a truncated parse from silently producing a partial back-fill.
///
/// By convention `features.ts` uses only `released` and `planned`; the `roadmap` release
/// axis is introduced exclusively by the overlay.
const EXPECTED_FEATURE_COUNT: usize = 69;

/// Arguments for the `project-map backfill-features` sub-command.
pub struct BackfillFeaturesArgs {
    /// Path to `features.ts`.
    pub features_path: PathBuf,
    /// Apply mode: `false` (the default) previews, `true` writes to the vault.
    ///
    /// Guard rail: when `apply` is `true` and `api_key` is empty, the run fails
    /// immediately, before any network access.
    pub apply: bool,
    /// Base URL of the gradatum server (e.g. `http://127.0.0.1:19090`).
    pub server_url: String,
    /// API key used for authentication; empty means dry-run only.
    pub api_key: String,
    /// Number of features the parse must yield, defaulting to the crate's
    /// `EXPECTED_FEATURE_COUNT`. Overridable so tests can use small fixtures.
    pub expected_count: usize,
}

impl BackfillFeaturesArgs {
    /// Builds the arguments with the production defaults.
    #[must_use]
    pub fn new(features_path: PathBuf, apply: bool, server_url: String, api_key: String) -> Self {
        Self {
            features_path,
            apply,
            server_url,
            api_key,
            expected_count: EXPECTED_FEATURE_COUNT,
        }
    }
}

/// Report of a feature back-fill run.
#[derive(Debug, Default, Clone)]
#[must_use]
pub struct BackfillFeaturesReport {
    /// Number of features parsed out of `features.ts`.
    pub parsed: usize,
    /// Dry-run only: number of cards that would be created.
    ///
    /// A dry-run does not query the vault, so this counts every card, including those
    /// that already exist.
    pub would_create: usize,
    /// Always `0`: a dry-run performs no existence check, so nothing is ever counted
    /// here. Kept for symmetry with [`Self::skipped`].
    pub would_skip: usize,
    /// Apply mode only: number of cards actually created.
    pub created: usize,
    /// Apply mode only: number of cards skipped because they already existed.
    pub skipped: usize,
    /// Apply mode only: number of cards refused because a card already carried the same
    /// title, under the normal form of [`crate::project_map_card::normalize_title`].
    ///
    /// Distinct from [`Self::skipped`], which counts the source-marker match: this axis
    /// catches a duplicate the marker misses — typically a card imported from another
    /// source under the same name.
    pub skipped_title: usize,
}

/// One feature parsed out of `features.ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFeature {
    /// Identifier (e.g. `F-37`).
    pub ref_label: String,
    /// Human-readable title (e.g. `gradatum-studio: Vault Management Interface`).
    pub name: String,
    /// Delivery status as declared on the site (`released` or `planned`).
    pub status: FeatureSiteStatus,
    /// Version as declared on the site (e.g. `v0.4.6`).
    pub version: String,
}

/// Delivery status as expressed in `features.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureSiteStatus {
    Released,
    Planned,
}

impl FeatureSiteStatus {
    /// Wire value for `[[status:…]]`, in SCREAMING_SNAKE_CASE, matching `StatusKind`.
    #[must_use]
    pub const fn as_status_wire(&self) -> &'static str {
        match self {
            Self::Released => "DONE",
            Self::Planned => "OPEN",
        }
    }

    /// Wire value for `[[release:…]]`, lowercase, matching `ReleaseKind`.
    #[must_use]
    pub const fn as_release_wire(&self) -> &'static str {
        match self {
            Self::Released => "released",
            Self::Planned => "planned",
        }
    }
}

/// Parses `features.ts` and returns the list of features it declares.
///
/// Extraction proceeds block by block, anchored on each `refLabel:` and running to the
/// next `refLabel:` or to the end of the input, which keeps it robust against interleaved
/// fields.
///
/// # Errors
///
/// - The parsed feature count differs from `expected_count`, which guards against a
///   partial parse silently producing a truncated back-fill.
/// - A block is missing its `name:`, `status:` or `version:` field.
pub fn parse_features(content: &str, expected_count: usize) -> Result<Vec<ParsedFeature>> {
    // Découpe le contenu en blocs en se basant sur `refLabel:`.
    // Chaque bloc démarre juste avant `refLabel:` et se termine avant le suivant.
    let ref_label_positions: Vec<usize> = content
        .match_indices("refLabel:")
        .map(|(pos, _)| pos)
        .collect();

    if ref_label_positions.is_empty() {
        bail!("parse features.ts: no `refLabel:` found — empty file or unexpected format");
    }

    let mut features = Vec::with_capacity(ref_label_positions.len());

    for (i, &start_pos) in ref_label_positions.iter().enumerate() {
        // Le bloc se termine juste avant le prochain `refLabel:` (ou à la fin).
        let end_pos = ref_label_positions
            .get(i + 1)
            .copied()
            .unwrap_or(content.len());
        let block = &content[start_pos..end_pos];

        let feature = parse_feature_block(block, i + 1)?;
        features.push(feature);
    }

    if features.len() != expected_count {
        bail!(
            "parse features.ts: {parsed} features parsed, {expected} expected — \
             incomplete parse or modified file. Reject the partial backfill (ADN 1).",
            parsed = features.len(),
            expected = expected_count,
        );
    }

    Ok(features)
}

/// Parses the TypeScript block describing one feature.
///
/// Expected shape, one field per line:
/// ```text
/// refLabel: 'F-XX',
/// name: '<texte>',
/// ...
/// status: 'released'|'planned',
/// version: 'vX.Y.Z',
/// ```
///
/// # Errors
///
/// One of `refLabel`, `name`, `status` or `version` is absent or malformed, or `status`
/// holds a value other than `released` or `planned`.
fn parse_feature_block(block: &str, block_idx: usize) -> Result<ParsedFeature> {
    let ref_label = extract_single_quoted_value(block, "refLabel:")
        .with_context(|| format!("block #{block_idx}: `refLabel:` absent or malformed"))?;

    let name = extract_single_quoted_value(block, "name:").with_context(|| {
        format!("block #{block_idx} ({ref_label}): `name:` absent or malformed")
    })?;

    let status_raw = extract_single_quoted_value(block, "status:").with_context(|| {
        format!("block #{block_idx} ({ref_label}): `status:` absent or malformed")
    })?;

    let status = match status_raw.as_str() {
        "released" => FeatureSiteStatus::Released,
        "planned" => FeatureSiteStatus::Planned,
        other => bail!(
            "block #{block_idx} ({ref_label}): unknown status {other:?} \
             (expected 'released' or 'planned')"
        ),
    };

    let version = extract_single_quoted_value(block, "version:").with_context(|| {
        format!("block #{block_idx} ({ref_label}): `version:` absent or malformed")
    })?;

    Ok(ParsedFeature {
        ref_label,
        name,
        status,
        version,
    })
}

/// Extracts the single-quoted value that follows `key` on the same line.
///
/// Looks up `key` in `text`, then captures whatever sits between the first `'` and its
/// closing `'` on that same line.
///
/// Returns `None` when the key is absent, when no quoted value follows it, or when the
/// quoted value is empty.
fn extract_single_quoted_value(text: &str, key: &str) -> Option<String> {
    // Cherche la position de la clé dans le texte.
    let key_pos = text.find(key)?;
    // Limite la recherche à la ligne contenant la clé.
    let after_key = &text[key_pos + key.len()..];
    let line_end = after_key.find('\n').unwrap_or(after_key.len());
    let line = &after_key[..line_end];

    // Capture le contenu entre la première paire de guillemets simples.
    let open = line.find('\'')?;
    let after_open = &line[open + 1..];
    let close = after_open.find('\'')?;
    let value = &after_open[..close];

    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Maps a website version `vX.Y.Z` onto the wire value of `[[version:gradatum/…]]`.
///
/// Strips the leading `v` and prefixes `gradatum/`, so `v0.4.0` becomes
/// `gradatum/0.4.0`.
///
/// Two inputs map to the backlog sentinel instead:
/// - `""` (empty) → `gradatum/backlog`
/// - `"vX.Y.Z"`, the literal placeholder shown on the website → `gradatum/backlog`
///
/// This is the exact inverse of the feature export, which renders a backlog card's
/// version as the literal `"vX.Y.Z"` so it stays visible on the website.
#[must_use]
pub fn map_version(version: &str) -> String {
    // Cas sentinelle : vide ou littéral "vX.Y.Z" → backlog wire.
    if version.is_empty() || version == "vX.Y.Z" {
        return "gradatum/backlog".to_string();
    }
    let numeric = version.strip_prefix('v').unwrap_or(version);
    format!("gradatum/{numeric}")
}

/// Builds the idempotence source marker of a feature.
///
/// The marker is `pm-feature-source:F-XX`. Being a pure function of the feature
/// identifier, it is stable across runs, which is what makes the back-fill idempotent.
#[must_use]
pub fn feature_marker(ref_label: &str) -> String {
    format!("pm-feature-source:{ref_label}")
}

// ─── Overlay amendment 41 → 45 ────────────────────────────────────────────────

/// Expected card count after applying the overlay.
///
/// The overlay performs only in-place overrides (release axis + `[[parent:]]` links);
/// it appends no new cards. Current value: 69 (matches `EXPECTED_FEATURE_COUNT`).
/// Fails loudly if the output of `apply_amendment_overlay` differs.
const TARGET_CARD_COUNT: usize = 69;

/// Specification of a project-map feature card, decoupled from the website parse.
///
/// Built by converting a [`ParsedFeature`], then refined in place by the overlay layer.
///
/// It carries every wire field `render_card_spec` needs. In particular `status_wire` and
/// `release_wire` describe the release axis, which is orthogonal to [`FeatureSiteStatus`]:
/// the website only ever expresses `released` or `planned`, while the release axis also
/// covers `roadmap` and `dropped`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureCardSpec {
    /// Short identifier (e.g. `F-37`).
    pub ref_label: String,
    /// Full human-readable name.
    pub name: String,
    /// Wire value for `[[status:…]]`, in SCREAMING_SNAKE_CASE (`OPEN` or `DONE`).
    pub status_wire: &'static str,
    /// Wire value for `[[release:…]]`, lowercase
    /// (`roadmap`, `planned`, `released` or `dropped`).
    pub release_wire: &'static str,
    /// Display version, used in the card title and fed to [`map_version`].
    ///
    /// - `"v0.4.3"` → wire `gradatum/0.4.3`
    /// - `"vX.Y.Z"` → wire `gradatum/backlog` (the backlog sentinel)
    /// - `""` → wire `gradatum/backlog`
    pub display_version: String,
    /// The originating feature this card continues, when it is a continuation.
    ///
    /// `Some("F-31")` adds a `[[parent:F-31]]` wikilink to the card's link line.
    pub parent: Option<String>,
}

impl From<&ParsedFeature> for FeatureCardSpec {
    /// Base conversion: the website's two-state status becomes a full spec, before any
    /// overlay is applied.
    fn from(f: &ParsedFeature) -> Self {
        Self {
            ref_label: f.ref_label.clone(),
            name: f.name.clone(),
            status_wire: f.status.as_status_wire(),
            release_wire: f.status.as_release_wire(),
            display_version: f.version.clone(),
            parent: None,
        }
    }
}

/// Applies the amendment overlay on top of the specs parsed from the website.
///
/// The overlay only ever rewrites cards in place; it appends none, so the input and
/// output lengths are identical. It performs two steps:
///
/// 1. Fourteen overrides on the `release` axis, moving cards from `planned` to `roadmap`
///    and setting their `[[parent:]]` link. A target that cannot be found is a hard
///    error rather than a silent no-op.
/// 2. A final assertion that the output length still equals the expected card count.
///
/// # Errors
///
/// - One of the fourteen target identifiers is absent from `base`, which usually means
///   the website changed and the overlay would have silently missed an override.
/// - The final card count differs from the expected one, meaning the overlay invariant
///   is broken.
pub fn apply_amendment_overlay(
    mut base: Vec<FeatureCardSpec>,
) -> anyhow::Result<Vec<FeatureCardSpec>> {
    // ── Overrides axe `release` : planned → roadmap (14 cartes — lot A) ──────
    //
    // Convention : features.ts n'exprime que `released`/`planned`. L'axe
    // `roadmap` (backlog master — Règle A) est UNIQUEMENT porté ici.
    // `status_wire` reste OPEN, `display_version` reste `vX.Y.Z` (inchangés
    // depuis le site) — seul `release_wire` bascule.
    //
    // `parent` (Règle B — continuations) est appliqué dans la même passe.
    //
    // Backlog historique (6, après lot A) :
    //   F-06, F-36 : backlog sans parent.
    //   F-63 → F-31, F-64 → F-44, F-65 → F-55, F-66 → F-42 : continuations.
    //   F-09 : sorti — devient planned v1.0.0 côté site (lot A).
    //
    // Demotes lot A (4 nouvelles entrées) :
    //   F-17 (était planned v0.4.4), F-25, F-26, F-51 → roadmap sans parent.
    //
    // Cartes-filles lot A (4 nouvelles entrées — Règle B) :
    //   F-67 → F-19, F-68 → F-60, F-69 → F-22, F-70 → F-62.
    //
    // Retirés vs Voie A (2 refs) :
    //   F-29 : devient planned v0.7.0 (lot A §2 sync version — sorti du backlog).
    //   F-62 : devient released v0.6.4 (lot A §1 flip — sorti du backlog).
    let roadmap_overrides: &[(&str, Option<&str>)] = &[
        // ── backlog historique (sans parent) ───────────────────────────────
        ("F-06", None),
        ("F-36", None),
        // ── demotes lot A (sans parent) ────────────────────────────────────
        ("F-17", None),
        ("F-25", None),
        ("F-26", None),
        ("F-51", None),
        // ── continuations historiques (Règle B) ────────────────────────────
        ("F-63", Some("F-31")),
        ("F-64", Some("F-44")),
        ("F-65", Some("F-55")),
        ("F-66", Some("F-42")),
        // ── cartes-filles lot A (Règle B) ──────────────────────────────────
        ("F-67", Some("F-19")),
        ("F-68", Some("F-60")),
        ("F-69", Some("F-22")),
        ("F-70", Some("F-62")),
    ];

    for (ref_label, parent) in roadmap_overrides {
        let spec = base
            .iter_mut()
            .find(|s| s.ref_label == *ref_label)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "overlay: ref_label {ref_label:?} absent from the base — \
                     the site may have changed. Silently missed override avoided (ADN 1)."
                )
            })?;
        spec.release_wire = "roadmap";
        spec.parent = parent.map(str::to_string);
        // `status_wire` (OPEN) et `display_version` (vX.Y.Z) restent ceux du site.
        // `name` reste celui du site (inchangé).
    }

    // ── Invariant final : 53 cartes exactement (zéro append) ────────────────
    if base.len() != TARGET_CARD_COUNT {
        anyhow::bail!(
            "overlay: {got} maps produced, {expected} expected — overlay invariant broken",
            got = base.len(),
            expected = TARGET_CARD_COUNT,
        );
    }

    Ok(base)
}

/// Renders a [`VaultWriteCard`] from a [`FeatureCardSpec`], overlay included.
///
/// Handles all three axes: `[[status:…]]`, `[[release:…]]`, and the optional
/// `[[parent:…]]`. The `display_version` is turned into its wire form by [`map_version`],
/// backlog sentinel included.
///
/// Body :
/// ```text
/// [[feature:F-XX]] [[project:gradatum]] [[status:<S>]] [[kind:FEATURE]]
/// [[release:<R>]] [[version:gradatum/x.y.z]] [[parent:F-YY]]  ← si parent
///
/// <name>
///
/// pm-feature-source:F-XX
/// ```
#[must_use]
pub fn render_card_spec(spec: &FeatureCardSpec) -> VaultWriteCard {
    let version_wire = map_version(&spec.display_version);
    let marker = feature_marker(&spec.ref_label);

    // Escape les `[[` dans le name pour éviter des wikilinks parasites dans le body.
    let escaped_name = spec.name.replace("[[", "[ [");

    let mut wikilinks_line = format!(
        "[[feature:{ref}]] [[project:gradatum]] [[status:{status}]] [[kind:FEATURE]] [[release:{release}]] [[version:{ver}]]",
        ref = spec.ref_label,
        status = spec.status_wire,
        release = spec.release_wire,
        ver = version_wire,
    );

    if let Some(parent) = &spec.parent {
        wikilinks_line.push_str(&format!(" [[parent:{parent}]]"));
    }

    let body = format!(
        "{wikilinks}\n\n{name}\n\n{marker}",
        wikilinks = wikilinks_line,
        name = escaped_name,
        marker = marker,
    );

    let title = format!(
        "[PROJECT-MAP][gradatum] {} — {}",
        spec.name, spec.display_version,
    );

    let ref_tag = spec.ref_label.to_ascii_lowercase();
    let tags = vec![
        "project-map".to_string(),
        "gradatum".to_string(),
        "feature".to_string(),
        ref_tag,
    ];

    VaultWriteCard {
        title,
        body,
        tags,
        section_hint: "project-map".to_string(),
    }
}

/// Renders a schema-valid project-map feature card from a [`ParsedFeature`].
///
/// Body format — six typed wikilinks, then the name, then the source marker:
/// ```text
/// [[feature:F-XX]] [[project:gradatum]] [[status:<S>]] [[kind:FEATURE]]
/// [[release:<R>]] [[version:gradatum/x.y.z]]
///
/// <name>
///
/// pm-feature-source:F-XX
/// ```
///
/// This satisfies the project-map schema validator, which requires a feature card to
/// carry exactly one `[[feature:]]`, `[[project:]]`, `[[status:]]`, `[[kind:]]` and
/// `[[version:]]` link.
#[must_use]
pub fn render_feature_card(feature: &ParsedFeature) -> VaultWriteCard {
    let version_wire = map_version(&feature.version);
    let status_wire = feature.status.as_status_wire();
    let release_wire = feature.status.as_release_wire();
    let marker = feature_marker(&feature.ref_label);

    // Escape les `[[` dans le titre pour éviter des wikilinks parasites.
    let escaped_name = feature.name.replace("[[", "[ [");

    let wikilinks_line = format!(
        "[[feature:{ref}]] [[project:gradatum]] [[status:{status}]] [[kind:FEATURE]] [[release:{release}]] [[version:{ver}]]",
        ref = feature.ref_label,
        status = status_wire,
        release = release_wire,
        ver = version_wire,
    );

    let body = format!(
        "{wikilinks}\n\n{name}\n\n{marker}",
        wikilinks = wikilinks_line,
        name = escaped_name,
        marker = marker,
    );

    let title = format!(
        "[PROJECT-MAP][gradatum] {} — {}",
        feature.name, feature.version,
    );

    // Tags : kebab-case, cohérents avec changelog_backfill.
    let ref_tag = feature.ref_label.to_ascii_lowercase(); // "f-37"
    let tags = vec![
        "project-map".to_string(),
        "gradatum".to_string(),
        "feature".to_string(),
        ref_tag,
    ];

    VaultWriteCard {
        title,
        body,
        tags,
        section_hint: "project-map".to_string(),
    }
}

/// Drives the back-fill of the feature cards into the gradatum vault.
///
/// The pipeline reads `features.ts`, parses it into features, converts them into card
/// specs, refines those in place through [`apply_amendment_overlay`], renders each card,
/// and finally either prints it or writes it.
///
/// Without `apply` (the default) each payload is printed to stdout and nothing is posted.
/// With `apply` set, idempotence is checked through [`VaultWriteClient::marker_exists`]
/// before every write.
///
/// # Errors
///
/// - `apply` is `true` while `api_key` is empty, which fails before any network access.
/// - `features.ts` cannot be read, or the parsed count differs from
///   `args.expected_count`.
/// - [`apply_amendment_overlay`] fails: a target identifier is missing, or the card count
///   invariant is broken.
/// - An HTTP call fails unrecoverably.
pub async fn run_backfill_features<C: VaultWriteClient>(
    args: &BackfillFeaturesArgs,
    client: &C,
) -> Result<BackfillFeaturesReport> {
    // Garde-fou : --apply sans --api-key → erreur immédiate.
    if args.apply && args.api_key.trim().is_empty() {
        bail!("--apply requires a non-empty --api-key");
    }

    let content = std::fs::read_to_string(&args.features_path)
        .with_context(|| format!("reading features.ts: {}", args.features_path.display()))?;

    let features = parse_features(&content, args.expected_count)
        .with_context(|| format!("parsing features.ts : {}", args.features_path.display()))?;

    // Conversion en specs, puis application de la couche overlay (41 → 45).
    let base_specs: Vec<FeatureCardSpec> = features.iter().map(FeatureCardSpec::from).collect();
    let specs =
        apply_amendment_overlay(base_specs).context("apply_amendment_overlay (41 → 45 cartes)")?;

    let mut report = BackfillFeaturesReport {
        parsed: features.len(),
        ..Default::default()
    };

    // Index des titres déjà présents, chargé une seule fois avant toute écriture.
    // Vide en dry-run : aucun appel réseau n'y est fait.
    let mut title_index = if args.apply {
        build_title_index(
            client
                .existing_titles()
                .await
                .context("loading the existing project-map titles")?,
        )
    } else {
        std::collections::HashMap::new()
    };

    for spec in &specs {
        let card = render_card_spec(spec);
        let marker = feature_marker(&spec.ref_label);

        if !args.apply {
            // Dry-run : print payload, zéro appel réseau.
            println!("[DRY-RUN] title={:?}  marker={marker}", card.title);
            println!("  {}", card.body.lines().next().unwrap_or(""));
            report.would_create += 1;
        } else {
            // Mode réel : idempotence puis write.
            let exists = client
                .marker_exists(&marker)
                .await
                .with_context(|| format!("idempotency check marker={marker}"))?;

            let normalized = normalize_title(&card.title);

            if exists {
                report.skipped += 1;
                tracing::debug!(%marker, "feature-map already exists — skip");
            } else if let Some(existing) = title_index.get(&normalized) {
                // Une carte est insupprimable par conception : la garde refuse d'écrire
                // et nomme celle qui occupe déjà le titre, elle n'efface jamais.
                report.skipped_title += 1;
                tracing::warn!(
                    %marker,
                    title = %card.title,
                    existing = %existing,
                    "a card already carries this title — refusing to write a duplicate"
                );
            } else {
                let locus = client
                    .vault_write(&card)
                    .await
                    .with_context(|| format!("vault_write for marker={marker}"))?;
                report.created += 1;
                // Alimenter l'index au fil de l'eau ferme aussi la collision intra-run.
                title_index.insert(normalized, locus);
                tracing::info!(%marker, "feature-map created");
            }
        }
    }

    Ok(report)
}

/// Builds an [`HttpVaultClient`] from the arguments, exchanging the API key for a JWT.
///
/// Only called in apply mode: a dry-run never instantiates an HTTP client.
///
/// # Errors
///
/// The API key exchange fails, or the underlying `reqwest` client cannot be built.
pub async fn build_http_client(args: &BackfillFeaturesArgs) -> Result<HttpVaultClient> {
    HttpVaultClient::new(&args.server_url, &args.api_key)
        .await
        .context("building HttpVaultClient for backfill-features")
}

// ─── Tests unitaires ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;

    // ── Mock client ──────────────────────────────────────────────────────────

    /// Mock sans appel réseau.
    ///
    /// Mémorise les cartes écrites : rejouer un run sur la MÊME instance rend visibles,
    /// via `existing_titles`, les titres produits au run précédent.
    pub struct MockVaultClient {
        existing_markers: Vec<String>,
        preexisting_titles: Vec<String>,
        created: Arc<Mutex<Vec<VaultWriteCard>>>,
        write_count: Arc<Mutex<usize>>,
    }

    impl MockVaultClient {
        pub fn new(existing: Vec<&str>) -> Self {
            Self {
                existing_markers: existing.into_iter().map(str::to_string).collect(),
                preexisting_titles: Vec::new(),
                created: Arc::new(Mutex::new(Vec::new())),
                write_count: Arc::new(Mutex::new(0)),
            }
        }

        /// Mock dont le vault porte déjà ces titres, sans le marqueur correspondant.
        pub fn with_preexisting_titles(titles: Vec<&str>) -> Self {
            Self {
                existing_markers: Vec::new(),
                preexisting_titles: titles.into_iter().map(str::to_string).collect(),
                created: Arc::new(Mutex::new(Vec::new())),
                write_count: Arc::new(Mutex::new(0)),
            }
        }

        pub fn write_count(&self) -> usize {
            *self
                .write_count
                .lock()
                .expect("mutex non-empoisonné en test")
        }
    }

    #[async_trait]
    impl VaultWriteClient for MockVaultClient {
        async fn marker_exists(&self, marker: &str) -> Result<bool> {
            Ok(self.existing_markers.iter().any(|m| m == marker))
        }

        async fn existing_titles(&self) -> Result<Vec<(String, String)>> {
            let mut out: Vec<(String, String)> = self
                .preexisting_titles
                .iter()
                .enumerate()
                .map(|(i, t)| (format!("project-map/pre-{i}"), t.clone()))
                .collect();
            let guard = self.created.lock().expect("mutex non-empoisonné en test");
            out.extend(
                guard
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (format!("project-map/mock-{i}"), c.title.clone())),
            );
            Ok(out)
        }

        async fn vault_write(&self, card: &VaultWriteCard) -> Result<String> {
            let mut guard = self.created.lock().expect("mutex non-empoisonné en test");
            let locus = format!("project-map/mock-{}", guard.len());
            guard.push(card.clone());
            *self.write_count.lock().expect("mutex") += 1;
            Ok(locus)
        }
    }

    // ── Fixture TS inline ────────────────────────────────────────────────────

    /// Fixture TS minimale : 2 features (1 released + 1 planned).
    const FIXTURE_TS_2: &str = r#"
const groups = [
  {
    features: [
      {
        id: 'f-01',
        refLabel: 'F-01',
        name: 'Warden: Network Access Control Layer',
        status: 'released',
        version: 'v0.1.0',
      },
      {
        id: 'f-37',
        refLabel: 'F-37',
        name: 'gradatum-studio: Vault Management Interface',
        status: 'planned',
        version: 'v0.4.6',
      },
    ],
  },
];
"#;

    fn make_args_from_str(
        content: &str,
        apply: bool,
        expected_count: usize,
    ) -> (tempfile::TempDir, BackfillFeaturesArgs) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("features.ts");
        std::fs::write(&path, content).expect("écriture features.ts test");
        let args = BackfillFeaturesArgs {
            features_path: path,
            apply,
            server_url: "http://127.0.0.1:19090".to_string(),
            api_key: String::new(),
            expected_count,
        };
        (dir, args)
    }

    /// Generates a synthetic `features.ts` with 69 features that MUST include
    /// the 14 refs required by `apply_amendment_overlay`:
    /// historical backlog: F-06/F-36/F-63/F-64/F-65/F-66;
    /// demoted: F-17/F-25/F-26/F-51;
    /// child cards: F-67/F-68/F-69/F-70 — all `planned vX.Y.Z`.
    /// F-09: promoted out of the backlog (planned v1.0.0) — included as a generic.
    ///
    /// The 66→69 rebaseline added 3 additional generic cards.
    ///
    /// Used for `run_backfill_features` tests that traverse the full
    /// parse → overlay(69, in-place) → render pipeline.
    fn make_full_fixture_ts() -> String {
        // Les 14 refs que l'overlay doit trouver en base (sinon fail-loud).
        // Tous `planned vX.Y.Z` (Règle A) — l'overlay bascule release_wire → roadmap.
        // F-09 retiré : sorti du backlog dans lot A (planned v1.0.0 côté site).
        let required_refs = [
            "F-06", "F-17", "F-25", "F-26", "F-36", "F-51", "F-63", "F-64", "F-65", "F-66", "F-67",
            "F-68", "F-69", "F-70",
        ];
        let mut features_ts = "const groups = [\n  {\n    features: [\n".to_string();

        // D'abord les 15 requis (planned vX.Y.Z sentinelle).
        for ref_label in &required_refs {
            let id = ref_label.to_ascii_lowercase().replace('-', "");
            features_ts.push_str(&format!(
                "      {{\n        id: '{id}',\n        refLabel: '{ref_label}',\n        name: 'Feature {ref_label} Placeholder',\n        status: 'planned',\n        version: 'vX.Y.Z',\n      }},\n"
            ));
        }
        // Puis 55 features génériques pour atteindre 69 total (14 requis + 55 = 69).
        // Rebaseline F-80/81/82 : 52 → 55 génériques.
        for i in 200..255usize {
            features_ts.push_str(&format!(
                "      {{\n        id: 'f{i}',\n        refLabel: 'F-{i}',\n        name: 'Feature {i}',\n        status: 'planned',\n        version: 'v0.4.0',\n      }},\n"
            ));
        }

        features_ts.push_str("    ],\n  },\n];\n");
        features_ts
    }

    // ── Test 1 : mapping wikilinks exact pour 2 features ────────────────────

    /// Vérifie le mapping exact des 6 wikilinks pour released et planned.
    #[test]
    fn two_features_produce_correct_wikilinks() {
        let features = parse_features(FIXTURE_TS_2, 2).expect("parse fixture");
        assert_eq!(features.len(), 2);

        // Feature released (F-01)
        let f01 = &features[0];
        assert_eq!(f01.ref_label, "F-01");
        assert_eq!(f01.status, FeatureSiteStatus::Released);
        let card01 = render_feature_card(f01);
        let body01 = &card01.body;

        // Vérifie chaque wikilink exact
        assert!(
            body01.contains("[[feature:F-01]]"),
            "feature manquant : {body01}"
        );
        assert!(
            body01.contains("[[project:gradatum]]"),
            "project manquant : {body01}"
        );
        assert!(
            body01.contains("[[status:DONE]]"),
            "status:DONE attendu pour released, got : {body01}"
        );
        assert!(
            body01.contains("[[kind:FEATURE]]"),
            "kind:FEATURE manquant : {body01}"
        );
        assert!(
            body01.contains("[[release:released]]"),
            "release:released attendu, got : {body01}"
        );
        assert!(
            body01.contains("[[version:gradatum/0.1.0]]"),
            "version:gradatum/0.1.0 attendu (v retiré), got : {body01}"
        );
        assert!(
            body01.contains("pm-feature-source:F-01"),
            "marker manquant : {body01}"
        );

        // Feature planned (F-37)
        let f37 = &features[1];
        assert_eq!(f37.ref_label, "F-37");
        assert_eq!(f37.status, FeatureSiteStatus::Planned);
        let card37 = render_feature_card(f37);
        let body37 = &card37.body;

        assert!(
            body37.contains("[[feature:F-37]]"),
            "feature:F-37 manquant : {body37}"
        );
        assert!(
            body37.contains("[[status:OPEN]]"),
            "status:OPEN attendu pour planned, got : {body37}"
        );
        assert!(
            body37.contains("[[release:planned]]"),
            "release:planned attendu, got : {body37}"
        );
        assert!(
            body37.contains("[[version:gradatum/0.4.6]]"),
            "version:gradatum/0.4.6 attendu, got : {body37}"
        );
        assert!(
            body37.contains("pm-feature-source:F-37"),
            "marker F-37 manquant : {body37}"
        );
    }

    // ── Test 2 : idempotence — N-1 créées + 1 sautée en apply ──────────────
    //
    // Utilise la fixture 69 features (overlay F-80/81/82) — F-06 est le marker
    // pré-existant, toutes les autres doivent être créées.

    #[tokio::test]
    async fn idempotence_one_created_one_skipped() {
        let full_ts = make_full_fixture_ts();
        let (_dir, mut args) = make_args_from_str(&full_ts, true, 69);
        args.api_key = "test-api-key".to_string();

        // Le marker de F-06 est déjà présent (1 skip attendu sur 69).
        let client = MockVaultClient::new(vec!["pm-feature-source:F-06"]);

        let report = run_backfill_features(&args, &client)
            .await
            .expect("run_backfill_features");

        assert_eq!(
            report.parsed, 69,
            "69 features parsées depuis la fixture F-80/81/82"
        );
        assert_eq!(report.skipped, 1, "F-06 devrait être sautée");
        assert_eq!(report.created, 68, "les 68 autres doivent être créées");
        assert_eq!(client.write_count(), 68);
    }

    // ── Rejeu : la garde du titre rattrape un marqueur aveugle ─────────────
    //
    // Reproduit le mode de défaillance de juin 2026 : `marker_exists` répondait
    // toujours `false` (bug `results` → `items`, commit 58f334b3), et chaque rejeu
    // recréait l'intégralité des cartes. Ici le marqueur est tout aussi aveugle ;
    // le second run ne crée pourtant rien, et c'est l'axe du titre qui l'en empêche.

    #[tokio::test]
    async fn replay_creates_nothing_when_the_marker_is_blind() {
        let full_ts = make_full_fixture_ts();
        let (_dir, mut args) = make_args_from_str(&full_ts, true, 69);
        args.api_key = "test-api-key".to_string();

        // `existing_markers` vide ⇒ `marker_exists` est constamment faux.
        let client = MockVaultClient::new(vec![]);

        let first = run_backfill_features(&args, &client)
            .await
            .expect("premier run");
        assert_eq!(first.created, 69, "le premier run crée les 69 cartes");
        assert_eq!(
            first.skipped_title, 0,
            "aucun titre préexistant au premier run"
        );

        let second = run_backfill_features(&args, &client)
            .await
            .expect("second run");
        assert_eq!(second.created, 0, "le rejeu ne doit créer aucune carte");
        assert_eq!(
            second.skipped, 0,
            "le marqueur reste aveugle : ce n'est pas lui qui bloque"
        );
        assert_eq!(
            second.skipped_title, 69,
            "les 69 sont refusées sur l'axe du titre"
        );
        assert_eq!(
            client.write_count(),
            69,
            "aucune écriture supplémentaire au second run"
        );
    }

    // ── La garde n'est pas plus stricte que la mesure ───────────────────────
    //
    // Le nettoyage du registre a compté les doublons en minuscules et espaces
    // réduits. Une garde sensible à la casse laisserait repasser ce que la mesure
    // avait compté : le titre préexistant est ici crié et sur-espacé.

    #[tokio::test]
    async fn title_guard_folds_case_and_whitespace() {
        let full_ts = make_full_fixture_ts();
        let (_dir, mut args) = make_args_from_str(&full_ts, true, 69);
        args.api_key = "test-api-key".to_string();

        let features = parse_features(&full_ts, 69).expect("parse fixture");
        let specs = apply_amendment_overlay(features.iter().map(FeatureCardSpec::from).collect())
            .expect("overlay");
        let first_title = render_card_spec(&specs[0]).title;
        let shouted = format!("   {}   ", first_title.to_uppercase().replace(' ', "     "));
        assert_ne!(
            shouted, first_title,
            "la fixture doit bien différer littéralement"
        );

        let client = MockVaultClient::with_preexisting_titles(vec![&shouted]);

        let report = run_backfill_features(&args, &client)
            .await
            .expect("run avec titre préexistant");

        assert_eq!(
            report.skipped_title, 1,
            "casse et espaces ne doivent pas masquer le doublon"
        );
        assert_eq!(report.created, 68, "les 68 autres cartes restent créées");
    }

    // ── Test 3 : dry-run — 0 write, would_create == 69 (avec overlay) ───────

    #[tokio::test]
    async fn dry_run_no_write_would_create_all() {
        let full_ts = make_full_fixture_ts();
        let (_dir, args) = make_args_from_str(&full_ts, false, 69);
        let client = MockVaultClient::new(vec![]);

        let report = run_backfill_features(&args, &client)
            .await
            .expect("dry-run");

        assert_eq!(report.created, 0, "dry-run ne doit rien créer");
        assert_eq!(
            report.would_create, 69,
            "would_create doit être 69 (69 parse, zéro append — overlay in-place)"
        );
        assert_eq!(client.write_count(), 0, "aucun vault_write en dry-run");
    }

    // ── Test 4 : parse vrai features.ts → 41 ───────────────────────────────

    /// Parses the real `features.ts` if accessible.
    ///
    /// Expected total: 69 features. Convention: `features.ts` uses only `released` /
    /// `planned`; the `roadmap` release axis is carried exclusively by the overlay.
    ///
    /// Expected distribution:
    /// - 33 released (unchanged from the previous baseline).
    /// - 36 planned: prior count + 3 new (F-80..F-82, all planned vX.Y.Z).
    #[test]
    fn real_features_ts_parses_to_69() {
        let path = std::path::Path::new("/home/maintainer-user/gradatum-www/src/data/features.ts");
        if !path.exists() {
            eprintln!("SKIP : features.ts absent hors sandbox de test");
            return;
        }
        let content = std::fs::read_to_string(path).expect("lecture features.ts");
        let features = parse_features(&content, 69).expect("parse features.ts réel F-80/81/82");
        assert_eq!(features.len(), 69, "69 features attendues (F-80/81/82)");
        // Distribution released/planned côté site (binaire — roadmap = overlay seul).
        let released = features
            .iter()
            .filter(|f| f.status == FeatureSiteStatus::Released)
            .count();
        let planned = features
            .iter()
            .filter(|f| f.status == FeatureSiteStatus::Planned)
            .count();
        assert_eq!(
            released, 33,
            "33 released attendues (inchangé vs lot A-bis)"
        );
        assert_eq!(
            planned, 36,
            "36 planned attendues (33 lot A-bis + 3 neuves F-80..82)"
        );
    }

    // ── Tests de mapping unitaires ───────────────────────────────────────────

    #[test]
    fn map_version_strips_v_prefix_and_prefixes_gradatum() {
        assert_eq!(map_version("v0.4.0"), "gradatum/0.4.0");
        assert_eq!(map_version("v0.1.0"), "gradatum/0.1.0");
        assert_eq!(map_version("v2.0.0"), "gradatum/2.0.0");
    }

    #[test]
    fn map_version_empty_returns_backlog_sentinel() {
        assert_eq!(map_version(""), "gradatum/backlog");
    }

    #[test]
    fn released_maps_to_done_and_released_wire() {
        assert_eq!(FeatureSiteStatus::Released.as_status_wire(), "DONE");
        assert_eq!(FeatureSiteStatus::Released.as_release_wire(), "released");
    }

    #[test]
    fn planned_maps_to_open_and_planned_wire() {
        assert_eq!(FeatureSiteStatus::Planned.as_status_wire(), "OPEN");
        assert_eq!(FeatureSiteStatus::Planned.as_release_wire(), "planned");
    }

    // ── Garde-fou : apply sans api_key → Err ────────────────────────────────

    #[tokio::test]
    async fn apply_without_api_key_returns_error() {
        let (_dir, mut args) = make_args_from_str(FIXTURE_TS_2, true, 2);
        args.api_key = String::new();
        let client = MockVaultClient::new(vec![]);

        let result = run_backfill_features(&args, &client).await;
        assert!(result.is_err(), "apply sans api_key doit retourner Err");
        assert_eq!(client.write_count(), 0);
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("api-key") || msg.contains("api_key"),
            "message doit mentionner api-key : {msg}"
        );
    }

    // ── Garde-fou : count mismatch → Err ────────────────────────────────────

    #[test]
    fn count_mismatch_returns_error() {
        // On attend 99 features mais la fixture n'en a que 2.
        let result = parse_features(FIXTURE_TS_2, 99);
        assert!(result.is_err(), "count mismatch doit retourner Err");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("2") && msg.contains("99"),
            "message doit mentionner 2 et 99 : {msg}"
        );
    }

    // ── Validateur T1 : cartes conformes au schéma quintuple ────────────────

    #[test]
    fn rendered_cards_pass_validator() {
        use crate::project_map_card::extract_wikilink_targets;
        use gradatum_core::project_map::validate_links_from_targets;

        let features = parse_features(FIXTURE_TS_2, 2).expect("parse fixture");
        for feature in &features {
            let card = render_feature_card(feature);
            let targets = extract_wikilink_targets(&card.body);
            assert_eq!(
                validate_links_from_targets(&targets),
                Ok(()),
                "carte-feature non conforme pour {} : targets={targets:?}\nbody={}",
                feature.ref_label,
                card.body
            );
        }
    }

    // ── Section hint et tags ─────────────────────────────────────────────────

    #[test]
    fn card_section_hint_is_project_map() {
        let feature = ParsedFeature {
            ref_label: "F-37".to_string(),
            name: "gradatum-studio".to_string(),
            status: FeatureSiteStatus::Planned,
            version: "v0.4.6".to_string(),
        };
        let card = render_feature_card(&feature);
        assert_eq!(card.section_hint, "project-map");
    }

    #[test]
    fn card_tags_contain_ref_label_kebab() {
        let feature = ParsedFeature {
            ref_label: "F-37".to_string(),
            name: "gradatum-studio".to_string(),
            status: FeatureSiteStatus::Planned,
            version: "v0.4.6".to_string(),
        };
        let card = render_feature_card(&feature);
        assert!(
            card.tags.contains(&"f-37".to_string()),
            "tags doit contenir 'f-37' : {:?}",
            card.tags
        );
        assert!(
            card.tags.contains(&"project-map".to_string()),
            "tags doit contenir 'project-map' : {:?}",
            card.tags
        );
        assert!(
            card.tags.contains(&"feature".to_string()),
            "tags doit contenir 'feature' : {:?}",
            card.tags
        );
    }

    // ── Tests overlay amendment (41 → 45) ────────────────────────────────────

    /// Builds specs reflecting the site-parsed base for the current rebaseline.
    ///
    /// - The 14 roadmap-overridable refs are present as `planned vX.Y.Z`
    ///   (site state before override):
    ///   historical backlog: F-06/36/63/64/65/66;
    ///   demoted: F-17/25/26/51;
    ///   child cards: F-67/68/69/70.
    /// - F-09: promoted out of the backlog (planned v1.0.0 on the site) — appears
    ///   in the generic `planned` placeholders (not overridden by the overlay).
    /// - `released` site-resident refs (F-31/44/55/02/08/13/15/16/42 +
    ///   F-29 and F-62 which exited the backlog) are in `released` state
    ///   (`FeatureCardSpec::from` would produce this — overlay does not touch them).
    /// - Remainder filled with `planned` placeholders to reach the target count.
    fn make_base_specs_with_overridable_refs() -> Vec<FeatureCardSpec> {
        // 14 refs que l'overlay doit trouver (planned vX.Y.Z → roadmap).
        // F-09 retiré : sorti du backlog dans lot A (planned v1.0.0 côté site).
        let roadmap_refs = [
            "F-06", "F-17", "F-25", "F-26", "F-36", "F-51", "F-63", "F-64", "F-65", "F-66", "F-67",
            "F-68", "F-69", "F-70",
        ];
        let mut specs: Vec<FeatureCardSpec> = roadmap_refs
            .iter()
            .map(|r| FeatureCardSpec {
                ref_label: r.to_string(),
                name: format!("Feature {r} — suite placeholder"),
                status_wire: "OPEN",
                release_wire: "planned",
                display_version: "vX.Y.Z".to_string(),
                parent: None,
            })
            .collect();

        // Refs released site-resident (l'overlay NE les touche pas).
        // F-29 et F-62 rejoignent ce groupe dans lot A (sortis du backlog).
        let released_refs = [
            "F-29", "F-31", "F-44", "F-55", "F-62", "F-02", "F-08", "F-13", "F-15", "F-16", "F-42",
        ];
        for r in released_refs {
            specs.push(FeatureCardSpec {
                ref_label: r.to_string(),
                name: format!("Feature {r}"),
                status_wire: "DONE",
                release_wire: "released",
                display_version: "v0.4.0".to_string(),
                parent: None,
            });
        }

        // Compléter jusqu'à 69 avec des specs génériques planned.
        // Rebaseline F-80/81/82 : 66 → 69 (3 cartes planned supplémentaires).
        let mut idx = 100usize;
        while specs.len() < 69 {
            specs.push(FeatureCardSpec {
                ref_label: format!("F-{idx}"),
                name: format!("Feature placeholder {idx}"),
                status_wire: "OPEN",
                release_wire: "planned",
                display_version: "v0.4.0".to_string(),
                parent: None,
            });
            idx += 1;
        }
        specs
    }

    // ── Test : map_version sentinelle "vX.Y.Z" → backlog ────────────────────

    #[test]
    fn map_version_vxyz_sentinel_is_backlog() {
        assert_eq!(
            map_version("vX.Y.Z"),
            "gradatum/backlog",
            "sentinelle d'affichage vX.Y.Z doit mapper vers gradatum/backlog"
        );
    }

    // ── Test : overlay produit 69 cartes ─────────────────────────────────────

    #[test]
    fn overlay_produces_69_cards() {
        let path = std::path::Path::new("/home/maintainer-user/gradatum-www/src/data/features.ts");
        if !path.exists() {
            eprintln!("SKIP overlay_produces_69_cards : features.ts absent");
            return;
        }
        let content = std::fs::read_to_string(path).expect("lecture features.ts");
        let features = parse_features(&content, 69).expect("parse features.ts réel F-80/81/82");
        let base: Vec<FeatureCardSpec> = features.iter().map(FeatureCardSpec::from).collect();
        let specs = apply_amendment_overlay(base).expect("apply_amendment_overlay");
        assert_eq!(
            specs.len(),
            69,
            "overlay doit produire exactement 69 cartes F-80/81/82 (in-place, zéro append)"
        );
    }

    // ── Test : F-31/F-44/F-55 corrigés en released ──────────────────────────

    /// F-31/44/55 are `released` site-resident (in `features.ts`), so
    /// `FeatureCardSpec::from` produces them as released.
    /// The overlay does NOT touch them — this test verifies they REMAIN released
    /// (not accidentally overridden by the roadmap pass).
    #[test]
    fn released_refs_preserved_through_overlay() {
        let base = make_base_specs_with_overridable_refs();
        let specs = apply_amendment_overlay(base).expect("overlay");

        for ref_label in ["F-31", "F-44", "F-55"] {
            let spec = specs
                .iter()
                .find(|s| s.ref_label == ref_label)
                .unwrap_or_else(|| panic!("{ref_label} absent après overlay"));
            assert_eq!(
                spec.status_wire, "DONE",
                "{ref_label} status_wire doit rester DONE"
            );
            assert_eq!(
                spec.release_wire, "released",
                "{ref_label} release_wire doit rester released"
            );
            assert_eq!(
                spec.parent, None,
                "{ref_label} (origine released) ne doit pas avoir de parent"
            );
        }
    }

    // ── Test : F-06/F-36 re-affectés en backlog roadmap (lot A) ─────────────
    //
    // F-09 : sorti du backlog dans lot A (planned v1.0.0 côté site) — NE figure
    // plus dans roadmap_overrides → reste planned après overlay.

    #[test]
    fn overlay_reaffects_f06_f36_backlog_lot_a() {
        let base = make_base_specs_with_overridable_refs();
        let specs = apply_amendment_overlay(base).expect("overlay");

        // F-06 et F-36 : toujours roadmap dans lot A.
        for ref_label in ["F-06", "F-36"] {
            let spec = specs
                .iter()
                .find(|s| s.ref_label == ref_label)
                .unwrap_or_else(|| panic!("{ref_label} absent après overlay"));
            assert_eq!(
                spec.status_wire, "OPEN",
                "{ref_label} status_wire doit être OPEN"
            );
            assert_eq!(
                spec.release_wire, "roadmap",
                "{ref_label} release_wire doit être roadmap"
            );
            assert_eq!(
                spec.display_version, "vX.Y.Z",
                "{ref_label} display_version doit être vX.Y.Z (sentinelle)"
            );
        }

        // F-09 : sorti du backlog dans lot A — doit rester planned (non overridé).
        let f09 = specs.iter().find(|s| s.ref_label == "F-09");
        // F-09 n'est pas dans make_base_specs_with_overridable_refs (placeholder
        // générique seulement si index 109 — non garanti), donc on vérifie que
        // l'overlay ne l'a PAS basculé en roadmap si présent.
        if let Some(spec) = f09 {
            assert_ne!(
                spec.release_wire, "roadmap",
                "F-09 ne doit pas être roadmap après lot A (sorti du backlog)"
            );
        }
    }

    // ── Test : continuations F-62..65 présentes, roadmap, bons parents ───────
    //
    // Post-rebaseline Voie A : ces cartes ne sont PLUS appendées par l'overlay —
    // elles sont parsées depuis features.ts (site-resident). L'overlay leur
    // applique seulement release:roadmap + le `[[parent:]]` (Règle B).

    #[test]
    fn continuations_f63_f66_roadmap_with_parents_lot_a() {
        // Lot A : F-62 est sorti du backlog (released v0.6.4), il ne figure plus
        // dans roadmap_overrides. Ce test vérifie :
        //   1. F-62 conserve son statut released après overlay (non touché).
        //   2. Les 4 continuations historiques (F-63/64/65/66) sont toujours
        //      roadmap + parent correct (Règle B).
        //   3. Les 4 cartes-filles lot A (F-67/68/69/70) sont roadmap + parent.
        let base = make_base_specs_with_overridable_refs();
        let specs = apply_amendment_overlay(base).expect("overlay");

        // F-62 : released depuis lot A, l'overlay NE le touche plus.
        let f62 = specs.iter().find(|s| s.ref_label == "F-62").expect("F-62");
        assert_eq!(
            f62.release_wire, "released",
            "F-62 doit être released (lot A)"
        );
        assert_eq!(f62.parent, None, "F-62 ne doit pas avoir de parent");

        // Continuations historiques (Règle B).
        for (ref_label, expected_parent) in [
            ("F-63", "F-31"),
            ("F-64", "F-44"),
            ("F-65", "F-55"),
            ("F-66", "F-42"),
        ] {
            let spec = specs
                .iter()
                .find(|s| s.ref_label == ref_label)
                .unwrap_or_else(|| panic!("{ref_label} absent après overlay"));
            assert_eq!(
                spec.release_wire, "roadmap",
                "{ref_label} doit être roadmap"
            );
            assert_eq!(
                spec.parent,
                Some(expected_parent.to_string()),
                "{ref_label} parent doit être {expected_parent}"
            );
        }

        // Cartes-filles lot A (Règle B).
        for (ref_label, expected_parent) in [
            ("F-67", "F-19"),
            ("F-68", "F-60"),
            ("F-69", "F-22"),
            ("F-70", "F-62"),
        ] {
            let spec = specs
                .iter()
                .find(|s| s.ref_label == ref_label)
                .unwrap_or_else(|| panic!("{ref_label} absent après overlay"));
            assert_eq!(
                spec.release_wire, "roadmap",
                "{ref_label} doit être roadmap (lot A)"
            );
            assert_eq!(
                spec.parent,
                Some(expected_parent.to_string()),
                "{ref_label} parent doit être {expected_parent}"
            );
        }
    }

    // ── Test : wikilinks [[parent:]] émis / non émis ─────────────────────────

    #[test]
    fn parent_wikilink_emitted() {
        let base = make_base_specs_with_overridable_refs();
        let specs = apply_amendment_overlay(base).expect("overlay");

        let f63 = specs.iter().find(|s| s.ref_label == "F-63").expect("F-63");
        let card63 = render_card_spec(f63);
        assert!(
            card63.body.contains("[[parent:F-31]]"),
            "F-63 body doit contenir [[parent:F-31]] : {}",
            card63.body
        );

        let f62 = specs.iter().find(|s| s.ref_label == "F-62").expect("F-62");
        let card62 = render_card_spec(f62);
        assert!(
            !card62.body.contains("[[parent:"),
            "F-62 body NE doit PAS contenir [[parent:… : {}",
            card62.body
        );
    }

    // ── Voie A : F-66 continuation Règle B (parent F-42, roadmap, backlog) ───

    /// Card F-66 (continuation of the curator threshold feature) emits
    /// `[[parent:F-42]]`, `[[release:roadmap]]`, and `[[version:gradatum/backlog]]`.
    #[test]
    fn voie_a_f66_continuation_parent_f42_roadmap_backlog() {
        let base = make_base_specs_with_overridable_refs();
        let specs = apply_amendment_overlay(base).expect("overlay");

        let f66 = specs.iter().find(|s| s.ref_label == "F-66").expect("F-66");
        assert_eq!(f66.release_wire, "roadmap", "F-66 doit être roadmap");
        assert_eq!(
            f66.parent,
            Some("F-42".to_string()),
            "F-66 parent doit être F-42 (Règle B)"
        );
        assert_eq!(
            f66.display_version, "vX.Y.Z",
            "F-66 display_version doit rester la sentinelle vX.Y.Z"
        );

        let card = render_card_spec(f66);
        assert!(
            card.body.contains("[[parent:F-42]]"),
            "F-66 body doit contenir [[parent:F-42]] : {}",
            card.body
        );
        assert!(
            card.body.contains("[[release:roadmap]]"),
            "F-66 body doit contenir [[release:roadmap]] : {}",
            card.body
        );
        assert!(
            card.body.contains("[[version:gradatum/backlog]]"),
            "F-66 body doit contenir [[version:gradatum/backlog]] (sentinelle) : {}",
            card.body
        );
    }

    /// Card F-42 (the Rule B origin feature) is `released` + `gradatum/0.3.0`
    /// — produced directly by `FeatureCardSpec::from` (site-resident, not touched
    /// by the overlay).
    #[test]
    fn voie_a_f42_released_v030() {
        let path = std::path::Path::new("/home/maintainer-user/gradatum-www/src/data/features.ts");
        if !path.exists() {
            eprintln!("SKIP voie_a_f42_released_v030 : features.ts absent");
            return;
        }
        let content = std::fs::read_to_string(path).expect("lecture features.ts");
        let features = parse_features(&content, 69).expect("parse features.ts réel F-80/81/82");
        let base: Vec<FeatureCardSpec> = features.iter().map(FeatureCardSpec::from).collect();
        let specs = apply_amendment_overlay(base).expect("overlay");

        let f42 = specs.iter().find(|s| s.ref_label == "F-42").expect("F-42");
        assert_eq!(f42.release_wire, "released", "F-42 doit être released");
        assert_eq!(f42.status_wire, "DONE", "F-42 status_wire doit être DONE");
        assert_eq!(
            f42.display_version, "v0.3.0",
            "F-42 display_version doit être v0.3.0"
        );
        assert_eq!(
            f42.parent, None,
            "F-42 (origine) ne doit pas avoir de parent"
        );

        let card = render_card_spec(f42);
        assert!(
            card.body.contains("[[version:gradatum/0.3.0]]"),
            "F-42 body doit contenir [[version:gradatum/0.3.0]] : {}",
            card.body
        );
        assert!(
            card.body.contains("[[release:released]]"),
            "F-42 body doit contenir [[release:released]] : {}",
            card.body
        );
    }

    // ── Test : cartes overlay passent le validateur ───────────────────────────

    #[test]
    fn rendered_overlay_cards_pass_validator() {
        use crate::project_map_card::extract_wikilink_targets;
        use gradatum_core::project_map::validate_links_from_targets;

        let base = make_base_specs_with_overridable_refs();
        let specs = apply_amendment_overlay(base).expect("overlay");

        // Valider les refs released + les roadmap-overridées lot A (les plus à risque,
        // notamment les continuations à `[[parent:]]`).
        // F-09 retiré (sorti du backlog lot A, absent de make_base_specs_*).
        let refs_to_check = [
            "F-31", "F-44", "F-55", "F-06", "F-17", "F-29", "F-36", "F-62", "F-63", "F-64", "F-65",
            "F-66",
        ];
        for ref_label in refs_to_check {
            let spec = specs
                .iter()
                .find(|s| s.ref_label == ref_label)
                .unwrap_or_else(|| panic!("{ref_label} absent"));
            let card = render_card_spec(spec);
            let targets = extract_wikilink_targets(&card.body);
            assert_eq!(
                validate_links_from_targets(&targets),
                Ok(()),
                "carte {ref_label} non conforme au validateur : targets={targets:?}\nbody={}",
                card.body
            );
        }
    }

    // ── Test : distribution overlay F-80/81/82 : 33 released + 22 planned + 14 roadmap ──

    #[test]
    fn overlay_distribution_33_released_22_planned_14_roadmap() {
        let path = std::path::Path::new("/home/maintainer-user/gradatum-www/src/data/features.ts");
        if !path.exists() {
            eprintln!("SKIP overlay_distribution : features.ts absent");
            return;
        }
        let content = std::fs::read_to_string(path).expect("lecture features.ts");
        let features = parse_features(&content, 69).expect("parse features.ts réel F-80/81/82");
        let base: Vec<FeatureCardSpec> = features.iter().map(FeatureCardSpec::from).collect();
        let specs = apply_amendment_overlay(base).expect("overlay");

        let released = specs
            .iter()
            .filter(|s| s.release_wire == "released")
            .count();
        let planned = specs.iter().filter(|s| s.release_wire == "planned").count();
        let roadmap = specs.iter().filter(|s| s.release_wire == "roadmap").count();

        assert_eq!(
            released, 33,
            "33 released attendues (inchangé vs lot A-bis) : {released}"
        );
        assert_eq!(
            planned, 22,
            "22 planned attendues (36 site - 14 roadmap overrides, F-80..82 ajoutées planned) : {planned}"
        );
        assert_eq!(
            roadmap, 14,
            "14 roadmap attendues (6 backlog historique + 4 demotes lot A + 4 filles lot A) : {roadmap}"
        );
        assert_eq!(released + planned + roadmap, 69, "total 69");
    }
}
