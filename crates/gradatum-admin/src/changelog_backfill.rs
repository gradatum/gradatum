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
//! existe déjà, la carte est sautée. La détection est en 4 étapes : (1) fast-path
//! snippet/title BM25, (2) fallback `vault_read` sur les hits retournés, (3)
//! fallback déterministe `vault_list` + `vault_read` exhaustif si `vault_search`
//! retourne exactement 50 hits (éjection ranking possible — section ≥ 50 cartes).
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

/// Vrai si `marker` apparaît dans `haystack` avec une frontière droite valide.
///
/// Anti-collision sous-chaîne : la seule ambiguïté réelle entre marqueurs est
/// l'extension numérique (`pm-feature-source:F-4` ⊂ `...F-42`,
/// `changelog/0.5.2/added/0` ⊂ `.../01`). Une occurrence ne compte que si le
/// caractère qui suit n'est PAS un chiffre ASCII (ou s'il n'y a pas de caractère
/// suivant). Case-sensitive, littéral exact (markers machine-générés — P2 reviewer).
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

    /// Énumère TOUS les paths d'une section via `POST /api/v1/vault_list` avec
    /// pagination complète (suit `next_cursor` jusqu'à épuisement).
    ///
    /// Garantit 0 faux-négatif quelle que soit la taille de la section : la borne
    /// limit=1000 par page est transparente — la boucle suit les pages jusqu'à
    /// `next_cursor == null`.
    ///
    /// Used as a deterministic fallback in `marker_exists` when `vault_search`
    /// did not return the target card (BM25 ranking may eject low-scoring matches
    /// from the top-N results).
    ///
    /// # Errors
    ///
    /// Erreur si un appel HTTP échoue ou si le parsing de la réponse est invalide.
    async fn vault_list_section_paths(&self, section: &str) -> Result<Vec<String>> {
        let url = format!("{}/api/v1/vault_list", self.base_url);
        let mut all_paths: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0u32;

        loop {
            page += 1;
            let mut body = serde_json::json!({
                "section": section,
                // limit=1000 = maximum accepté par vault_list (VaultListRequest.limit max).
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
                bail!("vault_list page={page} échoué : HTTP {status} — section={section}");
            }

            let payload: serde_json::Value = resp.json().await.with_context(|| {
                format!("parsing réponse vault_list page={page} section={section}")
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

    /// Lit le body markdown complet d'une note (`content`) via `POST /api/v1/vault_read`.
    ///
    /// # Errors
    ///
    /// Erreur si l'appel HTTP échoue, si le statut n'est pas 2xx, ou si le champ
    /// `content` est absent de la réponse.
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
            bail!("vault_read échoué : HTTP {status} — path={path}");
        }

        let payload: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("parsing réponse vault_read path={path}"))?;

        // Champ markdown = `content` (VaultReadResponse.content, dto.rs:154).
        payload["content"]
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("champ 'content' absent de la réponse vault_read path={path}"))
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
            bail!("vault_search échoué : HTTP {status}");
        }

        let payload: serde_json::Value =
            resp.json().await.context("parsing réponse vault_search")?;

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
                .context("hit vault_search sans champ 'path' (réponse malformée)")?;
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
