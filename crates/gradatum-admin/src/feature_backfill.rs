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
//! Feature cards satisfy the quintuple wikilink rule of the T1 validator:
//! `[[feature:F-XX]] [[project:gradatum]] [[status:<S>]] [[kind:FEATURE]]
//!  [[release:<R>]] [[version:gradatum/x.y.z]]`
//!
//! Base mapping (from `features.ts`):
//! - `released  → status:DONE   + release:released`
//! - `planned   → status:OPEN   + release:planned`
//! - `vX.Y.Z   → version:gradatum/x.y.z`  (strips the leading `v`)
//!
//! ## Overlay layer `apply_amendment_overlay` (in-place)
//!
//! The overlay appends no cards. The cards for F-02/08/13/15/16/42 (released) and
//! F-29/66 (planned) are site-resident in `features.ts` and parsed as the base.
//! The overlay only corrects the `release` axis and `[[parent:]]` links:
//!
//! ### `release` axis override: planned → roadmap (14 cards)
//!
//! F-06, F-36, F-17, F-25, F-26, F-51, F-63..F-70: parsed as `planned vX.Y.Z`
//! on the site (Rule A), but the canonical `release` axis is `roadmap`
//! (orthogonal to status). Only `release_wire` is flipped; `status_wire` (OPEN)
//! and `display_version` (vX.Y.Z) remain as sourced from the site.
//! F-09: promoted out of the backlog (planned v1.0.0 on the site).
//!
//! Fails loudly if an expected ref_label is missing (silent-regression guard).
//!
//! The `released` refs (F-31/44/55 + F-02/08/13/15/16/42) are NOT in the
//! override table: `features.ts` already carries the correct state, so
//! `FeatureCardSpec::from` produces the correct axis without intervention.
//!
//! ### Rule A — display sentinel
//!
//! A `roadmap` card has wire value `gradatum/backlog` but displays `vX.Y.Z`
//! in its title. `map_version("vX.Y.Z") → "gradatum/backlog"` (sentinel case).
//! The T2 export performs the inverse mapping (`backlog → "vX.Y.Z"`) on the site side.
//!
//! ### `[[parent:F-YY]]` link (Rule B — continuations)
//!
//! Continuation cards carry `[[parent:F-XX]]` (original feature):
//! F-63 → F-31, F-64 → F-44, F-65 → F-55, F-66 → F-42. Appended at the end of the
//! wikilinks line.
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
use crate::project_map_card::VaultWriteCard;

/// Expected feature count in `features.ts`.
///
/// Safety guard: if the parser returns a different count → explicit error
/// (prevents silent partial backfill).
///
/// Current count includes:
/// - F-75 (planned v0.7.0, Anthropic gateway)
/// - F-76..F-79 (planned v0.8.0, gradatum-code, 4 cards)
/// - F-80 (planned vX.Y.Z, gradatum-as-channel)
/// - F-81 (planned vX.Y.Z, HippoRAG-2)
/// - F-82 (planned vX.Y.Z, Arbor HTR)
///
/// All are `planned` with no parent and no overlay override.
/// Convention: `features.ts` uses only `released` / `planned`; the `roadmap`
/// release axis is carried exclusively by the `roadmap_overrides` overlay.
const EXPECTED_FEATURE_COUNT: usize = 69;

/// Arguments pour la sous-commande `project-map backfill-features`.
pub struct BackfillFeaturesArgs {
    /// Chemin vers `features.ts`.
    pub features_path: PathBuf,
    /// Mode apply : `false` (défaut) = dry-run, `true` = POST réel.
    ///
    /// Garde-fou : `apply == true` ET `api_key` vide → `Err` immédiat.
    pub apply: bool,
    /// URL de base du serveur gradatum (ex. `http://127.0.0.1:19090`).
    pub server_url: String,
    /// Clé API pour l'authentification (vide = dry-run uniquement).
    pub api_key: String,
    /// Nombre de features attendu (défaut : 53 — overridable pour les tests).
    pub expected_count: usize,
}

impl BackfillFeaturesArgs {
    /// Crée les args avec les valeurs par défaut production.
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

/// Rapport d'un run de backfill features.
#[derive(Debug, Default, Clone)]
#[must_use]
pub struct BackfillFeaturesReport {
    /// Nombre de features parsées depuis `features.ts`.
    pub parsed: usize,
    /// (dry-run) Nombre de cartes qui seraient créées.
    pub would_create: usize,
    /// (dry-run) Nombre de cartes sautées (déjà existantes — N/A en dry-run).
    pub would_skip: usize,
    /// (réel) Nombre de cartes effectivement créées.
    pub created: usize,
    /// (réel) Nombre de cartes sautées (déjà existantes).
    pub skipped: usize,
}

/// Feature parsée depuis `features.ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFeature {
    /// Identifiant (ex. `F-37`).
    pub ref_label: String,
    /// Titre lisible (ex. `gradatum-studio: Vault Management Interface`).
    pub name: String,
    /// Statut du site (`released` ou `planned`).
    pub status: FeatureSiteStatus,
    /// Version du site (ex. `v0.4.6`).
    pub version: String,
}

/// Statut de livraison tel qu'exprimé dans `features.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureSiteStatus {
    Released,
    Planned,
}

impl FeatureSiteStatus {
    /// Valeur wire `[[status:…]]` — SCREAMING_SNAKE (comme StatusKind).
    #[must_use]
    pub const fn as_status_wire(&self) -> &'static str {
        match self {
            Self::Released => "DONE",
            Self::Planned => "OPEN",
        }
    }

    /// Valeur wire `[[release:…]]` — lowercase (comme ReleaseKind).
    #[must_use]
    pub const fn as_release_wire(&self) -> &'static str {
        match self {
            Self::Released => "released",
            Self::Planned => "planned",
        }
    }
}

/// Parse `features.ts` et retourne la liste des features.
///
/// Stratégie : extraction bloc-par-bloc ancrée sur `refLabel:` jusqu'au
/// prochain `refLabel:` ou fin de fichier. Robuste aux champs intercalés.
///
/// # Errors
///
/// - Si le fichier est illisible.
/// - Si le compte de features parsées ≠ `expected_count` (guard anti-partiel).
/// - Si un bloc ne contient pas `name:`, `status:` ou `version:`.
pub fn parse_features(content: &str, expected_count: usize) -> Result<Vec<ParsedFeature>> {
    // Découpe le contenu en blocs en se basant sur `refLabel:`.
    // Chaque bloc démarre juste avant `refLabel:` et se termine avant le suivant.
    let ref_label_positions: Vec<usize> = content
        .match_indices("refLabel:")
        .map(|(pos, _)| pos)
        .collect();

    if ref_label_positions.is_empty() {
        bail!("parse features.ts : aucun `refLabel:` trouvé — fichier vide ou format inattendu");
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
            "parse features.ts : {parsed} features parsées, {expected} attendues — \
             parse incomplet ou fichier modifié. Refuser le backfill partiel (ADN 1).",
            parsed = features.len(),
            expected = expected_count,
        );
    }

    Ok(features)
}

/// Parse un bloc TS correspondant à une feature.
///
/// Format attendu (lignes distinctes) :
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
/// [`anyhow::Error`] si `ref_label`, `name`, `status` ou `version` sont absents
/// ou ont un format inattendu.
fn parse_feature_block(block: &str, block_idx: usize) -> Result<ParsedFeature> {
    let ref_label = extract_single_quoted_value(block, "refLabel:")
        .with_context(|| format!("bloc #{block_idx} : `refLabel:` absent ou mal formé"))?;

    let name = extract_single_quoted_value(block, "name:").with_context(|| {
        format!("bloc #{block_idx} ({ref_label}) : `name:` absent ou mal formé")
    })?;

    let status_raw = extract_single_quoted_value(block, "status:").with_context(|| {
        format!("bloc #{block_idx} ({ref_label}) : `status:` absent ou mal formé")
    })?;

    let status = match status_raw.as_str() {
        "released" => FeatureSiteStatus::Released,
        "planned" => FeatureSiteStatus::Planned,
        other => bail!(
            "bloc #{block_idx} ({ref_label}) : statut inconnu {other:?} \
             (attendu 'released' ou 'planned')"
        ),
    };

    let version = extract_single_quoted_value(block, "version:").with_context(|| {
        format!("bloc #{block_idx} ({ref_label}) : `version:` absent ou mal formé")
    })?;

    Ok(ParsedFeature {
        ref_label,
        name,
        status,
        version,
    })
}

/// Extrait la valeur entre guillemets simples après `key` sur la même ligne.
///
/// Cherche `key` dans `text`, puis capture le contenu entre la première `'`
/// et la `'` fermante sur la même ligne.
///
/// Retourne `None` si la clé est absente ou si aucune valeur entre `'…'` n'est trouvée.
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

/// Mappe une version site `vX.Y.Z` vers le wire de `[[version:gradatum/…]]`.
///
/// Retire le `v` initial et préfixe `gradatum/`.
/// Ex. `v0.4.0 → gradatum/0.4.0`.
///
/// Cas sentinelle (Règle A) :
/// - `""` (vide) → `gradatum/backlog`
/// - `"vX.Y.Z"` (littéral sentinelle d'affichage) → `gradatum/backlog`
///
/// Ce mapping est l'inverse de l'export T2 qui fait `backlog → "vX.Y.Z"` côté site.
#[must_use]
pub fn map_version(version: &str) -> String {
    // Cas sentinelle : vide ou littéral "vX.Y.Z" → backlog wire.
    if version.is_empty() || version == "vX.Y.Z" {
        return "gradatum/backlog".to_string();
    }
    let numeric = version.strip_prefix('v').unwrap_or(version);
    format!("gradatum/{numeric}")
}

/// Construit le marqueur source d'idempotence pour une feature.
///
/// Format : `pm-feature-source:F-XX` (identique pour chaque run → idempotent).
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

/// Spécification d'une carte-feature project-map, indépendante du parse site.
///
/// Produite par conversion depuis [`ParsedFeature`] (base 41) puis enrichie
/// par la couche overlay (6 overrides + 4 cartes neuves).
///
/// Contient tous les champs wire nécessaires à `render_card_spec` :
/// `status_wire`/`release_wire` dérivés de l'axe release (orthogonal au
/// `FeatureSiteStatus` qui ne couvre que `released`/`planned`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureCardSpec {
    /// Identifiant court (ex. `F-37`).
    pub ref_label: String,
    /// Nom lisible complet.
    pub name: String,
    /// Valeur wire `[[status:…]]` — SCREAMING_SNAKE (`OPEN` | `DONE`).
    pub status_wire: &'static str,
    /// Valeur wire `[[release:…]]` — lowercase (`roadmap`|`planned`|`released`|`dropped`).
    pub release_wire: &'static str,
    /// Version d'affichage (dans le titre et le mapping `map_version`).
    ///
    /// - `"v0.4.3"` → wire `gradatum/0.4.3`
    /// - `"vX.Y.Z"` → wire `gradatum/backlog` (Règle A sentinelle)
    /// - `""` → wire `gradatum/backlog`
    pub display_version: String,
    /// Feature d'origine dont cette carte est une continuation (Règle B).
    ///
    /// Si `Some("F-31")` → wikilink `[[parent:F-31]]` ajouté à la ligne des liens.
    pub parent: Option<String>,
}

impl From<&ParsedFeature> for FeatureCardSpec {
    /// Conversion base : site binaire → spec riche (sans overlay).
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

/// Applique la couche overlay amendment sur la base de 53 specs parse-site.
///
/// Opérations (in-place uniquement — zéro append depuis la rebaseline Voie A) :
/// 1. 9 overrides axe `release` (planned → roadmap) + `[[parent:]]` — fail-loud
///    si un ref attendu est absent.
/// 2. Assert sortie == `TARGET_CARD_COUNT` (53).
///
/// # Errors
///
/// - Si un des 9 ref_labels cibles est absent dans `base` (anti-régression).
/// - Si le compte final ≠ 53 (invariant overlay cassé).
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
                    "overlay: ref_label {ref_label:?} absent de la base — \
                     le site a peut-être changé. Override silencieusement manqué évité (ADN 1)."
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
            "overlay: {got} cartes produites, {expected} attendues — invariant overlay cassé",
            got = base.len(),
            expected = TARGET_CARD_COUNT,
        );
    }

    Ok(base)
}

/// Rend une [`VaultWriteCard`] depuis une [`FeatureCardSpec`] (overlay-aware).
///
/// Gère les 3 axes : `[[status:…]]`, `[[release:…]]`, `[[parent:…]]` optionnel.
/// La `display_version` est mappée vers le wire via `map_version` (Règle A incluse).
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

/// Rend une carte-feature project-map valide depuis une [`ParsedFeature`].
///
/// Body format (6 wikilinks typés + marker + name) :
/// ```text
/// [[feature:F-XX]] [[project:gradatum]] [[status:<S>]] [[kind:FEATURE]]
/// [[release:<R>]] [[version:gradatum/x.y.z]]
///
/// <name>
///
/// pm-feature-source:F-XX
/// ```
///
/// Satisfait la règle quintuple §10e du validateur T1 (LIVE).
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

/// Orchestre le backfill des 53 cartes-feature vers le vault gradatum.
///
/// Pipeline : parse `features.ts` (53) → specs → `apply_amendment_overlay`
/// (53, in-place) → render → dry-run print / apply.
///
/// Sans `apply` (défaut) : affiche chaque payload sur stdout, ne POST rien.
/// Avec `apply=true` : vérifie l'idempotence via `marker_exists` avant chaque write.
///
/// # Errors
///
/// - `apply == true` et `api_key` vide → `Err` immédiat (guard avant tout réseau).
/// - Parse `features.ts` échoue ou compte ≠ `args.expected_count`.
/// - `apply_amendment_overlay` échoue (ref_label absent ou invariant 45 cassé).
/// - Appel HTTP non-récupérable.
pub async fn run_backfill_features<C: VaultWriteClient>(
    args: &BackfillFeaturesArgs,
    client: &C,
) -> Result<BackfillFeaturesReport> {
    // Garde-fou : --apply sans --api-key → erreur immédiate.
    if args.apply && args.api_key.trim().is_empty() {
        bail!("--apply requires a non-empty --api-key");
    }

    let content = std::fs::read_to_string(&args.features_path)
        .with_context(|| format!("lecture features.ts : {}", args.features_path.display()))?;

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
                .with_context(|| format!("vérification idempotence marker={marker}"))?;

            if exists {
                report.skipped += 1;
                tracing::debug!(%marker, "carte-feature déjà existante — skip");
            } else {
                client
                    .vault_write(&card)
                    .await
                    .with_context(|| format!("vault_write pour marker={marker}"))?;
                report.created += 1;
                tracing::info!(%marker, "carte-feature créée");
            }
        }
    }

    Ok(report)
}

/// Construit un [`HttpVaultClient`] depuis les args (échange api-key → JWT).
///
/// Appelé uniquement en mode apply — DRY-RUN n'instancie pas le client HTTP.
///
/// # Errors
///
/// Si l'échange api-key échoue ou si la construction du client reqwest échoue.
pub async fn build_http_client(args: &BackfillFeaturesArgs) -> Result<HttpVaultClient> {
    HttpVaultClient::new(&args.server_url, &args.api_key)
        .await
        .context("construction HttpVaultClient pour backfill-features")
}

// ─── Tests unitaires ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;

    // ── Mock client ──────────────────────────────────────────────────────────

    /// Mock sans appel réseau.
    pub struct MockVaultClient {
        existing_markers: Vec<String>,
        created: Arc<Mutex<Vec<VaultWriteCard>>>,
        write_count: Arc<Mutex<usize>>,
    }

    impl MockVaultClient {
        pub fn new(existing: Vec<&str>) -> Self {
            Self {
                existing_markers: existing.into_iter().map(str::to_string).collect(),
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

        async fn vault_write(&self, card: &VaultWriteCard) -> Result<String> {
            let mut guard = self.created.lock().expect("mutex non-empoisonné en test");
            guard.push(card.clone());
            *self.write_count.lock().expect("mutex") += 1;
            Ok("mock-ulid".to_string())
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
