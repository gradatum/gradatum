//! `gradatum-admin project-map backfill-changelog` — création en masse de cartes
//! project-map depuis le CHANGELOG.
//!
//! ## Modes
//!
//! - **Dry-run** (défaut) : affiche chaque payload (title + source_marker) sans
//!   appeler `POST /api/v1/vault_write`. Le report indique `would_create`.
//! - **Réel** : appelle `vault_write` pour chaque nouvelle carte. L'idempotence
//!   est garantie par un test de pré-existence via `vault_search` du marqueur.
//!
//! ## Idempotence
//!
//! Avant chaque write, `client.marker_exists(marker)` est appelé. Si la note
//! existe déjà (marqueur trouvé dans `vault_search`), la carte est sautée.
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

/// Timeout par défaut pour les appels HTTP vers le serveur gradatum (cap DoS — ADN 5).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Client pour `POST /api/v1/vault_write` et vérification d'idempotence.
///
/// Mockable dans les tests — l'implémentation réelle utilise reqwest.
#[async_trait]
pub trait VaultWriteClient: Send + Sync + 'static {
    /// Vérifie si une note avec ce marqueur source existe déjà.
    ///
    /// # Errors
    ///
    /// Retourne une erreur si l'appel HTTP échoue ou si le parsing de la
    /// réponse est invalide.
    async fn marker_exists(&self, marker: &str) -> Result<bool>;

    /// `POST /api/v1/vault_write` — crée une note. Retourne l'ULID de la note créée.
    ///
    /// # Errors
    ///
    /// Retourne une erreur si l'appel HTTP échoue ou si le statut de réponse
    /// n'est pas 200/201.
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
    /// Crée un client HTTP en échangeant l'api-key contre un JWT.
    ///
    /// # Errors
    ///
    /// - Si l'échange api-key échoue (HTTP != 2xx ou body invalide).
    /// - Si la construction du client `reqwest` échoue.
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
            .context("échange api-key (POST /auth/exchange)")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("échange api-key échoué : HTTP {status} — {body}");
        }

        let payload: serde_json::Value = resp
            .json()
            .await
            .context("parsing réponse /auth/exchange")?;
        // Champ JWT = "token" (struct ExchangeResponse.token — auth_routes.rs:42).
        // Le fallback "jwt" est conservé par défaut de robustesse si la struct évolue.
        let jwt = payload["token"]
            .as_str()
            .or_else(|| payload["jwt"].as_str())
            .context("champ 'token' absent de la réponse /auth/exchange")?
            .to_string();

        Ok(Self {
            base_url: base_url.to_string(),
            jwt,
            http,
        })
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
                "limit": 1
            }))
            .send()
            .await
            .context("vault_search (marker_exists)")?;

        if !resp.status().is_success() {
            let status = resp.status();
            bail!("vault_search échoué : HTTP {status}");
        }

        let payload: serde_json::Value =
            resp.json().await.context("parsing réponse vault_search")?;
        let count = payload["results"].as_array().map(|a| a.len()).unwrap_or(0);
        Ok(count > 0)
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
            bail!("vault_write échoué : HTTP {status} — {body}");
        }

        let payload: serde_json::Value =
            resp.json().await.context("parsing réponse vault_write")?;
        let ulid = payload["note_id"]
            .as_str()
            .or_else(|| payload["id"].as_str())
            .unwrap_or("unknown")
            .to_string();
        Ok(ulid)
    }
}

/// Arguments pour la sous-commande `backfill-changelog`.
pub struct BackfillChangelogArgs {
    /// Chemin du fichier CHANGELOG.md.
    pub changelog_path: PathBuf,
    /// Version SemVer minimale incluse.
    pub from_version: String,
    /// Version SemVer maximale incluse.
    pub to_version: String,
    /// Mode apply : `false` (défaut) = dry-run preview, `true` = POST réel vers le vault.
    ///
    /// Garde-fou : si `apply == true` et `api_key` est vide, `run_backfill` retourne `Err`
    /// avant tout accès réseau.
    pub apply: bool,
    /// URL de base du serveur gradatum.
    pub server_url: String,
    /// Clé API pour l'authentification.
    pub api_key: String,
    /// Inclut les sections méta (Tests, Internal, Documentation…) comme cartes KindKind::Task.
    /// Par défaut false : seules les sections Keep-a-Changelog standard sont incluses.
    pub include_meta: bool,
}

/// Rapport d'un run de backfill changelog.
#[derive(Debug, Default, Clone)]
#[must_use]
pub struct BackfillChangelogReport {
    /// Nombre total d'entrées parsées depuis le CHANGELOG.
    pub parsed: usize,
    /// (dry-run) Nombre de notes qui seraient créées.
    pub would_create: usize,
    /// (dry-run) Nombre de notes sautées (déjà existantes).
    pub would_skip: usize,
    /// (réel) Nombre de notes effectivement créées.
    pub created: usize,
    /// (réel) Nombre de notes sautées (déjà existantes).
    pub skipped: usize,
    /// Nombre d'entrées méta skippées (sections hors allowlist, include_meta=false).
    pub skipped_meta: usize,
}

/// Orchestre le backfill des entrées CHANGELOG vers le vault gradatum.
///
/// Sans `apply` (défaut) : affiche chaque payload (title + source_marker) sur stdout,
/// ne POST rien. Avec `apply=true` : vérifie l'idempotence via `marker_exists` avant
/// chaque write.
///
/// # Errors
///
/// Retourne `Err` si `apply == true` et `api_key` est vide (garde-fou avant tout accès
/// réseau), si le parsing CHANGELOG échoue, ou si un appel HTTP produit une erreur
/// non-récupérable.
pub async fn run_backfill<C: VaultWriteClient>(
    args: &BackfillChangelogArgs,
    client: &C,
) -> Result<BackfillChangelogReport> {
    // Garde-fou : --apply sans --api-key → erreur immédiate, avant tout accès réseau.
    if args.apply && args.api_key.trim().is_empty() {
        anyhow::bail!("--apply requires a non-empty --api-key");
    }

    let content = std::fs::read_to_string(&args.changelog_path)
        .with_context(|| format!("lecture CHANGELOG : {}", args.changelog_path.display()))?;

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
                .with_context(|| {
                    format!("vérification idempotence marker={}", entry.source_marker)
                })?;

            if exists {
                report.skipped += 1;
                tracing::debug!(marker = %entry.source_marker, "carte déjà existante — skip");
            } else {
                client
                    .vault_write(&card)
                    .await
                    .with_context(|| format!("vault_write pour marker={}", entry.source_marker))?;
                report.created += 1;
                tracing::info!(marker = %entry.source_marker, "carte créée");
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
