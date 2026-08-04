//! `gradatum-admin project-map backfill-changelog` — bulk creation of project-map cards
//! from a CHANGELOG file.
//!
//! ## Modes
//!
//! - **Dry-run** (the default): prints each payload (title and source marker) without
//!   calling `POST /api/v1/vault_write`. The report only fills `would_create`.
//! - **Apply**: calls `vault_write` for every card that does not exist yet. Idempotence
//!   comes from a pre-existence check on the card's source marker.
//!
//! ## Idempotence
//!
//! Before each write, [`crate::changelog_backfill::VaultWriteClient::marker_exists`] is
//! called and the card is skipped when the note already exists. Detection proceeds in
//! three stages:
//!
//! 1. Fast path — the marker appears in the title or snippet of a `vault_search` hit.
//! 2. Fallback — read the full body of each hit, because the full-text snippet may have
//!    truncated the marker.
//! 3. Deterministic fallback — page through the whole `project-map` section with
//!    `vault_list` and read every body not already checked. It runs when the search
//!    reported more corpus matches than it returned, or when it returned a full page of
//!    50 hits, both of which mean relevance ranking may have pushed the target out of
//!    the results.
//!
//! ## Mockable client
//!
//! `VaultWriteClient` is an `async_trait` that isolates tests from the network.
//! `HttpVaultClient` performs real HTTP calls (auth + write + search).
//! Tests use a `MockVaultClient` implementation.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use crate::changelog_parse::parse_changelog;
use crate::project_map_card::{VaultWriteCard, render_card};

/// Returns `true` when `marker` occurs in `haystack` with a valid right boundary.
///
/// Plain substring matching would produce false positives, because the only real
/// ambiguity between markers is a numeric extension: `pm-feature-source:F-4` is a prefix
/// of `...F-42`, and `changelog/0.5.2/added/0` is a prefix of `.../01`. An occurrence
/// therefore counts only when the following character is not an ASCII digit, or when
/// there is no following character at all.
///
/// Matching is case-sensitive and literal, which is safe because markers are
/// machine-generated.
fn marker_matches(haystack: &str, marker: &str) -> bool {
    if marker.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(marker) {
        let abs = start + pos;
        let after = haystack[abs + marker.len()..].chars().next();
        if !matches!(after, Some(c) if c.is_ascii_digit()) {
            return true;
        }
        start = abs + marker.len();
    }
    false
}

/// Default timeout for HTTP calls to the gradatum server; bounds the time a single
/// request can hold the run hostage.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Client for `POST /api/v1/vault_write` and for the idempotence pre-check.
///
/// Kept as a trait so tests can substitute an in-memory double; the real implementation
/// is [`HttpVaultClient`].
#[async_trait]
pub trait VaultWriteClient: Send + Sync + 'static {
    /// Reports whether a note carrying this source marker already exists.
    ///
    /// # Errors
    ///
    /// The HTTP call fails, or the response cannot be parsed.
    async fn marker_exists(&self, marker: &str) -> Result<bool>;

    /// `POST /api/v1/vault_write` — creates a note and returns its ULID.
    ///
    /// # Errors
    ///
    /// The HTTP call fails, or the server answers with a non-success status.
    async fn vault_write(&self, card: &VaultWriteCard) -> Result<String>;
}

/// HTTP implementation of `VaultWriteClient`.
///
/// Authenticates by exchanging an api-key for a JWT (POST `/auth/exchange`).
/// The JWT is obtained once at construction and reused for all subsequent calls.
pub struct HttpVaultClient {
    base_url: String,
    jwt: String,
    http: reqwest::Client,
}

impl HttpVaultClient {
    /// Builds an HTTP client, exchanging the API key for a JWT up front.
    ///
    /// # Errors
    ///
    /// - The API key exchange fails: non-2xx status, or an unusable response body.
    /// - The underlying `reqwest` client cannot be built.
    pub async fn new(base_url: &str, api_key: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("construction client reqwest")?;

        // Échange api-key → JWT.
        //
        // Contrat serveur vérifié contre `gradatum-server/src/auth_routes.rs` :
        //   - Route : POST /auth/exchange (PAS /api/v1/auth/exchange)
        //   - Header attendu : `Authorization: Bearer ak_<secret>`
        //     (ou `Authorization: ak_<secret>` — alternative acceptée)
        //   - Pas de body JSON — l'api-key est UNIQUEMENT dans le header
        //   - Réponse succès : { "token": "<jwt>", "ttl_secs", "scopes", "tenant_id", "kid" }
        //   - HTTP 400 si header absent ou format invalide
        //   - HTTP 401 si clé invalide ou révoquée
        let exchange_url = format!("{base_url}/auth/exchange");
        let resp = http
            .post(&exchange_url)
            .bearer_auth(api_key)
            .send()
            .await
            .context("api-key exchange (POST /auth/exchange)")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("api-key exchange failed: HTTP {status} — {body}");
        }

        let payload: serde_json::Value = resp
            .json()
            .await
            .context("parsing /auth/exchange response")?;
        // Champ JWT = "token" (struct ExchangeResponse.token — auth_routes.rs:42).
        // Le fallback "jwt" est conservé par défaut de robustesse si la struct évolue.
        let jwt = payload["token"]
            .as_str()
            .or_else(|| payload["jwt"].as_str())
            .context("'token' field absent from /auth/exchange response")?
            .to_string();

        Ok(Self {
            base_url: base_url.to_string(),
            jwt,
            http,
        })
    }

    /// Enumerates every path of a section through `POST /api/v1/vault_list`, following
    /// `next_cursor` until the listing is exhausted.
    ///
    /// The per-page limit of 1000 is therefore invisible to callers, and the result
    /// cannot miss an entry however large the section is.
    ///
    /// Used as a deterministic fallback in [`VaultWriteClient::marker_exists`] when
    /// `vault_search` did not return the target card, since relevance ranking may eject
    /// low-scoring matches from the top-N results.
    ///
    /// # Errors
    ///
    /// An HTTP call fails, or a response cannot be parsed.
    async fn vault_list_section_paths(&self, section: &str) -> Result<Vec<String>> {
        let url = format!("{}/api/v1/vault_list", self.base_url);
        let mut all_paths: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0u32;

        loop {
            page += 1;
            let mut body = serde_json::json!({
                "section": section,
                // ⚠️ vault_list borne `limit` à 200 (`unwrap_or(20).clamp(1, 200)`) : les
                // 1000 demandés ici sont silencieusement ramenés à 200 par le serveur.
                // La valeur est laissée telle quelle — la pagination par `cursor`
                // ci-dessous couvre le reste, donc le backfill reste complet.
                "limit": 1000
            });
            if let Some(ref c) = cursor {
                body["cursor"] = serde_json::Value::String(c.clone());
            }

            let resp = self
                .http
                .post(&url)
                .bearer_auth(&self.jwt)
                .json(&body)
                .send()
                .await
                .with_context(|| {
                    format!("vault_list page={page} (POST /api/v1/vault_list) section={section}")
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                bail!("vault_list page={page} failed: HTTP {status} — section={section}");
            }

            let payload: serde_json::Value = resp.json().await.with_context(|| {
                format!("parsing vault_list response page={page} section={section}")
            })?;

            // Champ entries = VaultListResponse.entries (dto.rs:180).
            let entries = payload["entries"].as_array().cloned().unwrap_or_default();
            all_paths.extend(
                entries
                    .into_iter()
                    .filter_map(|e| e["path"].as_str().map(str::to_string)),
            );

            // Pagination : suivre next_cursor jusqu'à null.
            match payload["next_cursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }

        Ok(all_paths)
    }

    /// Reads a note's full Markdown body through `POST /api/v1/vault_read`.
    ///
    /// # Errors
    ///
    /// The HTTP call fails, the status is not 2xx, or the response carries no `content`
    /// field.
    async fn vault_read_content(&self, path: &str) -> Result<String> {
        let url = format!("{}/api/v1/vault_read", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.jwt)
            // VaultReadRequest a deny_unknown_fields : envoyer UNIQUEMENT `path`.
            // tenant_id défaut "main" côté serveur, section optionnel.
            .json(&serde_json::json!({ "path": path }))
            .send()
            .await
            .with_context(|| format!("vault_read (POST /api/v1/vault_read) path={path}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            bail!("vault_read failed: HTTP {status} — path={path}");
        }

        let payload: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("parsing vault_read response path={path}"))?;

        // Champ markdown = `content` (VaultReadResponse.content, dto.rs:154).
        payload["content"]
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("'content' field absent from vault_read response path={path}"))
    }
}

#[async_trait]
impl VaultWriteClient for HttpVaultClient {
    async fn marker_exists(&self, marker: &str) -> Result<bool> {
        let url = format!("{}/api/v1/vault_search", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.jwt)
            .json(&serde_json::json!({
                "query": marker,
                "section": "project-map",
                // limit:50 (max serveur) pour maximiser la couverture BM25 avant
                // de recourir au fallback vault_list.
                "limit": 50,
                // include_corpus_count:true → corpus_match_count dans la réponse.
                // Permet de détecter si des cartes ont été éjectées du top-50 BM25
                // (corpus_match_count > items.len()) sans se coupler à une constante
                // limite côté client (P1-C2 reviewer 2026-06-23).
                "include_corpus_count": true
            }))
            .send()
            .await
            .context("vault_search (marker_exists)")?;

        if !resp.status().is_success() {
            let status = resp.status();
            bail!("vault_search failed: HTTP {status}");
        }

        let payload: serde_json::Value =
            resp.json().await.context("parsing vault_search response")?;

        // Correction bug P1 : le champ API réel est `items`, pas `results`.
        // Référence : VaultSearchResponse.items (gradatum-server/src/api_v1/dto.rs:117).
        let items = payload["items"].as_array().cloned().unwrap_or_default();

        // Step 2. Fast-path : snippet ou title contient le marqueur (match BORNÉ).
        //         `marker_matches` évite les collisions sous-chaîne (F-4 ⊂ F-42).
        let fast = items.iter().any(|hit| {
            let snippet = hit["snippet"].as_str().unwrap_or("");
            let title = hit["title"].as_str().unwrap_or("");
            marker_matches(snippet, marker) || marker_matches(title, marker)
        });
        if fast {
            return Ok(true);
        }

        // Step 3. Fallback vault_read sur les hits vault_search : le snippet FTS5
        //         peut tronquer le marqueur. Lire le body complet des candidats,
        //         short-circuit au 1er match. Read-error → propage (fail-loud).
        //         Hit sans `path` = réponse vault_search malformée → bail.
        let mut searched_paths: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for hit in &items {
            let path = hit["path"]
                .as_str()
                .context("vault_search hit without 'path' field (malformed response)")?;
            searched_paths.insert(path.to_string());
            let body = self.vault_read_content(path).await?;
            if marker_matches(&body, marker) {
                return Ok(true);
            }
        }

        // Step 4. Fallback déterministe vault_list + pagination complète.
        //
        //         Activation : si corpus_match_count > items.len(), des cartes
        //         correspondent aux tokens FTS5 du marqueur mais ont été éjectées
        //         du top-N BM25 → la cible peut se trouver parmi elles. Sans
        //         corpus_match_count (serveur antérieur) → fallback conservatif sur
        //         items.len() >= VAULT_SEARCH_LIMIT (comportement v1).
        //
        //         Récupérer TOUS les paths via vault_list (pagination complète, boucle
        //         next_cursor) — garantit 0 faux-négatif quelle que soit la taille de
        //         la section. Lire les paths non encore vus en step 3.
        //         Read-error → propage (fail-loud).
        const VAULT_SEARCH_LIMIT: usize = 50; // max du serveur — fallback conservatif
        let corpus_count = payload["corpus_match_count"].as_u64().unwrap_or(0);
        let items_seen = items.len() as u64;
        let needs_full_scan = (corpus_count > 0 && corpus_count > items_seen)
            || items_seen >= VAULT_SEARCH_LIMIT as u64;
        if needs_full_scan {
            let all_paths = self.vault_list_section_paths("project-map").await?;
            for path in &all_paths {
                if searched_paths.contains(path) {
                    // Déjà lu en step 3 sans match → skip.
                    continue;
                }
                let body = self.vault_read_content(path).await?;
                if marker_matches(&body, marker) {
                    return Ok(true);
                }
            }
        }

        // Step 5. Vault_search + vault_list exhaustif (si déclenché) : marqueur absent.
        Ok(false)
    }

    async fn vault_write(&self, card: &VaultWriteCard) -> Result<String> {
        let url = format!("{}/api/v1/vault_write", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.jwt)
            .json(&serde_json::json!({
                "title": card.title,
                "body": card.body,
                "tags": card.tags,
                "section_hint": card.section_hint
            }))
            .send()
            .await
            .context("vault_write (POST /api/v1/vault_write)")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("vault_write failed: HTTP {status} — {body}");
        }

        let payload: serde_json::Value =
            resp.json().await.context("parsing vault_write response")?;
        let ulid = payload["note_id"]
            .as_str()
            .or_else(|| payload["id"].as_str())
            .unwrap_or("unknown")
            .to_string();
        Ok(ulid)
    }
}

/// Arguments for the `backfill-changelog` sub-command.
pub struct BackfillChangelogArgs {
    /// Path to the `CHANGELOG.md` file.
    pub changelog_path: PathBuf,
    /// Lowest SemVer version to include.
    pub from_version: String,
    /// Highest SemVer version to include.
    pub to_version: String,
    /// Apply mode: `false` (the default) previews, `true` writes to the vault.
    ///
    /// Guard rail: when `apply` is `true` and `api_key` is empty, [`run_backfill`]
    /// returns an error before touching the network.
    pub apply: bool,
    /// Base URL of the gradatum server.
    pub server_url: String,
    /// API key used for authentication.
    pub api_key: String,
    /// Include meta sections (`Tests`, `Internal`, `Documentation`, …) as task cards.
    ///
    /// Defaults to `false`, in which case only the standard Keep a Changelog sections
    /// are turned into cards.
    pub include_meta: bool,
}

/// Report of a CHANGELOG back-fill run.
#[derive(Debug, Default, Clone)]
#[must_use]
pub struct BackfillChangelogReport {
    /// Total number of entries parsed out of the CHANGELOG.
    pub parsed: usize,
    /// Dry-run only: number of notes that would be created.
    ///
    /// A dry-run does not query the vault, so this counts every parsed entry, including
    /// entries whose card already exists.
    pub would_create: usize,
    /// Always `0`: a dry-run performs no existence check, so nothing is ever counted
    /// here. Kept for symmetry with [`Self::skipped`].
    pub would_skip: usize,
    /// Apply mode only: number of notes actually created.
    pub created: usize,
    /// Apply mode only: number of notes skipped because the card already existed.
    pub skipped: usize,
    /// Number of entries dropped because their section is outside the allow-list, which
    /// only happens when `include_meta` is `false`.
    pub skipped_meta: usize,
}

/// Drives the back-fill of CHANGELOG entries into the gradatum vault.
///
/// Without `apply` (the default) each payload — title and source marker — is printed to
/// stdout and nothing is posted. With `apply` set, idempotence is checked through
/// [`VaultWriteClient::marker_exists`] before every write.
///
/// # Errors
///
/// Returns an error when `apply` is `true` and `api_key` is empty (a guard rail that
/// fires before any network access), when the CHANGELOG cannot be read, or when an HTTP
/// call fails unrecoverably.
pub async fn run_backfill<C: VaultWriteClient>(
    args: &BackfillChangelogArgs,
    client: &C,
) -> Result<BackfillChangelogReport> {
    // Garde-fou : --apply sans --api-key → erreur immédiate, avant tout accès réseau.
    if args.apply && args.api_key.trim().is_empty() {
        anyhow::bail!("--apply requires a non-empty --api-key");
    }

    let content = std::fs::read_to_string(&args.changelog_path)
        .with_context(|| format!("reading CHANGELOG: {}", args.changelog_path.display()))?;

    // Parse une fois pour compter les entrées totales (include_meta=true) afin d'alimenter
    // skipped_meta, puis parse avec le flag réel pour la boucle de write.
    let entries_total = parse_changelog(&content, &args.from_version, &args.to_version, true);
    let entries = parse_changelog(
        &content,
        &args.from_version,
        &args.to_version,
        args.include_meta,
    );
    let mut report = BackfillChangelogReport {
        parsed: entries.len(),
        skipped_meta: entries_total.len().saturating_sub(entries.len()),
        ..Default::default()
    };

    for entry in &entries {
        let card = render_card(entry);

        if !args.apply {
            println!(
                "[DRY-RUN] title={:?}  marker={}",
                card.title, entry.source_marker
            );
            // En dry-run on ne vérifie pas l'existence — tout compté comme would_create.
            report.would_create += 1;
        } else {
            // Vérification idempotence avant write.
            let exists = client
                .marker_exists(&entry.source_marker)
                .await
                .with_context(|| format!("idempotency check marker={}", entry.source_marker))?;

            if exists {
                report.skipped += 1;
                tracing::debug!(marker = %entry.source_marker, "map already exists — skip");
            } else {
                client
                    .vault_write(&card)
                    .await
                    .with_context(|| format!("vault_write for marker={}", entry.source_marker))?;
                report.created += 1;
                tracing::info!(marker = %entry.source_marker, "map created");
            }
        }
    }

    Ok(report)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// CHANGELOG inline pour les tests (pas d'I/O fichier réel).
    const TEST_CHANGELOG: &str = r#"
## [0.5.2] - 2026-06-15

### Added

- **vault_write in-place update**: supports note_id + expected_sha256.
- **vault_timeline endpoint**: new chronological listing endpoint.

### Fixed

- **Optimistic-lock Conflict**: fixed by anti-clobber guard.
"#;

    /// Mock du client vault — pas d'appel réseau réel.
    struct MockVaultClient {
        existing_markers: Vec<String>,
        created: Arc<Mutex<Vec<VaultWriteCard>>>,
    }

    impl MockVaultClient {
        fn new(existing: Vec<&str>) -> Self {
            Self {
                existing_markers: existing.into_iter().map(str::to_string).collect(),
                created: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn created_count(&self) -> usize {
            self.created
                .lock()
                .expect("mutex non-empoisonné en test")
                .len()
        }
    }

    #[async_trait]
    impl VaultWriteClient for MockVaultClient {
        async fn marker_exists(&self, marker: &str) -> Result<bool> {
            Ok(self.existing_markers.iter().any(|m| m == marker))
        }

        async fn vault_write(&self, card: &VaultWriteCard) -> Result<String> {
            self.created
                .lock()
                .expect("mutex non-empoisonné en test")
                .push(card.clone());
            Ok("mock-ulid".to_string())
        }
    }

    /// Crée un `BackfillChangelogArgs` à partir d'un changelog inline (via tempfile).
    ///
    /// `apply=false` = dry-run (défaut), `apply=true` = mode réel.
    fn make_args_from_str(
        content: &str,
        apply: bool,
    ) -> (tempfile::TempDir, BackfillChangelogArgs) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("CHANGELOG.md");
        std::fs::write(&path, content).expect("écriture CHANGELOG test");
        let args = BackfillChangelogArgs {
            changelog_path: path,
            from_version: "0.5.2".to_string(),
            to_version: "0.5.2".to_string(),
            apply,
            server_url: "http://127.0.0.1:19090".to_string(),
            api_key: String::new(),
            include_meta: false,
        };
        (dir, args)
    }

    #[tokio::test]
    async fn apply_without_api_key_returns_error() {
        // Garde-fou : --apply sans --api-key doit retourner Err avant tout appel réseau.
        let (_dir, mut args) = make_args_from_str(TEST_CHANGELOG, true);
        args.api_key = String::new(); // vide explicitement
        let client = MockVaultClient::new(vec![]);

        let result = run_backfill(&args, &client).await;
        assert!(
            result.is_err(),
            "apply=true sans api_key doit retourner Err"
        );
        assert_eq!(
            client.created_count(),
            0,
            "aucun vault_write ne doit être appelé si api_key vide"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("api-key") || err_msg.contains("api_key"),
            "message d'erreur doit mentionner api-key, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn dry_run_prints_payloads_no_post() {
        let (_dir, args) = make_args_from_str(TEST_CHANGELOG, false);
        let client = MockVaultClient::new(vec![]);

        let report = run_backfill(&args, &client)
            .await
            .expect("run_backfill dry-run");

        // Dry-run → created == 0, would_create > 0, aucun vault_write appelé
        assert_eq!(report.created, 0, "dry-run ne doit pas créer");
        assert!(
            report.would_create > 0,
            "would_create doit être > 0 (entries parsées)"
        );
        assert_eq!(
            client.created_count(),
            0,
            "mock.created doit rester vide en dry-run"
        );
    }

    #[tokio::test]
    async fn real_run_posts_all_new_cards() {
        let (_dir, mut args) = make_args_from_str(TEST_CHANGELOG, true);
        args.apply = true;
        args.api_key = "test-api-key".to_string(); // garde-fou : api_key non-vide requis avec apply
        let client = MockVaultClient::new(vec![]);

        let report = run_backfill(&args, &client)
            .await
            .expect("run_backfill réel");

        // Réel, aucun marker existant → toutes les entrées créées
        assert!(report.created > 0, "au moins 1 note doit être créée");
        assert_eq!(
            report.created,
            client.created_count(),
            "report.created doit correspondre aux appels mock"
        );
        assert_eq!(report.skipped, 0, "aucune note ne doit être sautée");
    }

    #[tokio::test]
    async fn idempotent_skips_existing_marker() {
        let (_dir, mut args) = make_args_from_str(TEST_CHANGELOG, true);
        args.apply = true;
        args.api_key = "test-api-key".to_string(); // garde-fou : api_key non-vide requis avec apply

        // On parse d'abord pour connaître le premier marqueur
        let content = std::fs::read_to_string(&args.changelog_path).expect("lecture");
        let entries = parse_changelog(&content, &args.from_version, &args.to_version, false);
        assert!(!entries.is_empty(), "le CHANGELOG doit avoir des entrées");

        let first_marker = entries[0].source_marker.clone();
        let client = MockVaultClient::new(vec![first_marker.as_str()]);

        let report = run_backfill(&args, &client)
            .await
            .expect("run_backfill idempotence");

        // Le premier marqueur existait → 1 skip, N-1 créés
        assert_eq!(report.skipped, 1, "1 note doit être sautée");
        assert_eq!(report.created, entries.len() - 1);
    }

    #[tokio::test]
    async fn parsed_count_matches_changelog_entries() {
        let (_dir, args) = make_args_from_str(TEST_CHANGELOG, false);
        let client = MockVaultClient::new(vec![]);

        let report = run_backfill(&args, &client).await.expect("run_backfill");

        // TEST_CHANGELOG a 3 bullets → 3 entrées parsées
        assert_eq!(
            report.parsed, 3,
            "3 entrées attendues dans le CHANGELOG test"
        );
    }
}
