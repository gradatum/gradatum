//! Loopback HTTP client for the gradatum server's `/internal` API.
//!
//! ## Architecture
//!
//! The worker only needs:
//! - `server_url` — base URL of the internal listener (`:19092`)
//! - `token` — `GRADATUM_INTERNAL_TOKEN` (Bearer token, never logged)
//!
//! All mutations (vault + index) flow through the server via this client.
//! The worker no longer accesses `SqliteIndex` or `Vault` directly.
//!
//! ## Auth
//!
//! `X-Gradatum-Internal: Bearer <token>` header on every request.
//!
//! ## Retry
//!
//! Retries on 5xx: max 3 attempts, exponential backoff 100ms/200ms/400ms.
//! No retry on 409 (Conflict) or 404.
//!
//! ## Ports
//!
//! - `:19090` — public API
//! - `:19092` — `/internal` API (this client, loopback-only)

use std::time::Duration;

use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse,
};
use secrecy::{ExposeSecret as _, SecretString};

// ─────────────────────────────────────────────────────────────────────────────
// Erreurs
// ─────────────────────────────────────────────────────────────────────────────

/// Error returned by [`InternalPersistClient`] and [`InternalClient`].
#[derive(thiserror::Error, Debug)]
pub enum InternalClientError {
    /// HTTP request failure (network, timeout, parse).
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// Optimistic-lock conflict (409) — the worker must call `mark_conflict`.
    /// `current_sha256_hex` is the current SHA-256 of the note (64 hex chars) when known.
    #[error("Server returned conflict (409)")]
    Conflict { current_sha256_hex: Option<String> },

    /// Note not found (404).
    #[error("Note not found (404): {ulid}")]
    NotFound { ulid: String },

    /// Server error (5xx or other unexpected status code).
    #[error("Server error {status}: {body}")]
    ServerError { status: u16, body: String },
}

// ─────────────────────────────────────────────────────────────────────────────
// DTOs de réponse locaux (indépendants de gradatum-server)
// ─────────────────────────────────────────────────────────────────────────────

/// Full note returned by `GET /internal/v1/note/:ulid`.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub struct NoteReadDto {
    /// ULID of the note.
    pub note_id: String,
    /// SHA-256 hex, 64 chars.
    pub sha256_hex: String,
    /// Markdown body.
    pub body: String,
    /// Section in kebab-case.
    pub section: String,
    /// Status in kebab-case.
    pub status: String,
    /// Tags.
    pub tags: Vec<String>,
    /// `true` if the note has been forgotten (frontmatter `forgotten = true`).
    pub forgotten: bool,
    /// `true` if the note has already been distilled (`extra["processed"] = true`).
    pub processed: bool,
}

/// Embedding returned by `GET /internal/v1/note/:ulid/embedding`.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
pub struct EmbeddingReadDto {
    /// ULID of the note.
    pub note_id: String,
    /// Model identifier.
    pub embedder_id: String,
    /// Vector dimension.
    pub dim: usize,
    /// f32 embedding vector.
    pub vector: Vec<f32>,
}

/// Trust score returned by `GET /internal/v1/note/:ulid/trust`.
#[derive(Debug, serde::Deserialize)]
struct TrustReadDto {
    /// Trust score.
    trust: f32,
}

/// Title-lookup result returned by `GET /internal/v1/title-lookup`.
#[derive(Debug, serde::Deserialize)]
struct TitleLookupDto {
    /// Resolved ULID, or `None` if the title is unknown.
    note_id: Option<String>,
}

/// ID-lookup result returned by `GET /internal/v1/id-lookup`.
#[derive(Debug, serde::Deserialize)]
struct IdLookupDto {
    /// Confirmed ULID if the note exists and is live, or `None` otherwise.
    note_id: Option<String>,
}

/// Note identifier in a listing response.
#[derive(Debug, serde::Deserialize, Clone)]
#[allow(dead_code)]
pub struct NoteIdDto {
    /// ULID of the note.
    pub note_id: String,
    /// Section (may be empty for `list_garbage`).
    pub section: String,
}

/// List of notes returned by listing endpoints.
#[derive(Debug, serde::Deserialize)]
struct NoteListDto {
    /// Listed notes.
    note_ids: Vec<NoteIdDto>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Trait InternalClient (injectable pour les tests)
// ─────────────────────────────────────────────────────────────────────────────

/// Trait abstracting HTTP calls to the server's internal API.
///
/// Enables injecting a mock in tests without depending on `reqwest`.
///
/// ## Contract
///
/// Each method maps 1:1 to an `/internal/v1/...` endpoint.
#[async_trait::async_trait]
pub trait InternalClient: Send + Sync + 'static {
    // ── Writes ──

    /// `POST /internal/v1/persist/curated`
    async fn persist_curated(
        &self,
        req: &PersistCuratedRequest,
    ) -> Result<PersistOkResponse, InternalClientError>;

    /// `POST /internal/v1/persist/embedding`
    async fn persist_embedding(
        &self,
        req: &PersistEmbeddingRequest,
    ) -> Result<EmbeddingOkResponse, InternalClientError>;

    /// `POST /internal/v1/persist/forget`
    async fn persist_forget(
        &self,
        req: &PersistForgetRequest,
    ) -> Result<PersistOkResponse, InternalClientError>;

    /// `POST /internal/v1/persist/distill`
    async fn persist_distill(
        &self,
        req: &PersistDistillRequest,
    ) -> Result<PersistOkResponse, InternalClientError>;

    /// `DELETE /internal/v1/note/:ulid`
    async fn delete_note(&self, ulid: &str) -> Result<(), InternalClientError>;

    // ── Reads ──

    /// `GET /internal/v1/note/:ulid`
    async fn get_note(&self, ulid: &str) -> Result<NoteReadDto, InternalClientError>;

    /// `GET /internal/v1/note/:ulid/embedding?embedder_id=<id>`
    async fn get_note_embedding(
        &self,
        ulid: &str,
        embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError>;

    /// `GET /internal/v1/note/:ulid/trust`
    async fn get_trust(&self, ulid: &str) -> Result<f32, InternalClientError>;

    /// `GET /internal/v1/title-lookup?tenant=<t>&title=<title>`
    async fn title_lookup(
        &self,
        tenant: &str,
        title: &str,
    ) -> Result<Option<String>, InternalClientError>;

    /// `GET /internal/v1/id-lookup?tenant=<t>&note_id=<ulid>` — checks that a note exists and is live.
    ///
    /// Used for ULID-first resolution of `[[section:ULID]]` wikilinks.
    /// Returns `Ok(Some(id))` if the note exists and is live, `Ok(None)` otherwise.
    /// Non-fatal: any failure must be handled by the caller.
    async fn id_lookup(
        &self,
        tenant: &str,
        note_id: &str,
    ) -> Result<Option<String>, InternalClientError>;

    /// `GET /internal/v1/notes/by-locus?vault=<v>&prefix=<p>`
    async fn list_notes_by_locus(
        &self,
        vault: &str,
        prefix: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError>;

    /// `GET /internal/v1/notes/by-status?vault=<v>&status=<s>`
    async fn list_by_status(
        &self,
        vault: &str,
        status: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError>;

    /// `GET /internal/v1/notes/garbage?vault=<v>&before_ms=<i64>&grace_days=<u32>`
    async fn list_garbage(
        &self,
        vault: &str,
        before_ms: i64,
        grace_days: u32,
    ) -> Result<Vec<NoteIdDto>, InternalClientError>;

    /// `GET /internal/v1/forget/search?vault=<v>&query=<q>&limit=<n>`
    async fn search_fts_for_forget(
        &self,
        vault: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteIdDto>, InternalClientError>;

    /// `GET /internal/v1/notes/by-agent?agent=<a>&vaults[]=<v1>&vaults[]=<v2>`
    async fn list_notes_by_agent(
        &self,
        agent: &str,
        vaults: &[String],
    ) -> Result<Vec<NoteIdDto>, InternalClientError>;
}

// ─────────────────────────────────────────────────────────────────────────────
// Implémentation concrète — reqwest
// ─────────────────────────────────────────────────────────────────────────────

/// Concrete HTTP client for the gradatum server's `/internal` API.
///
/// The token is stored in a `SecretString` — never printed in logs.
/// `Debug` is implemented manually to mask the token.
pub struct InternalPersistClient {
    client: reqwest::Client,
    base_url: String,
    token: SecretString,
}

impl std::fmt::Debug for InternalPersistClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalPersistClient")
            .field("base_url", &self.base_url)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl InternalPersistClient {
    /// Builds an HTTP client with a 30-second global timeout.
    ///
    /// `server_url`: base URL of the internal listener (e.g. `http://127.0.0.1:19092`).
    /// `token`: Bearer token — must match `GRADATUM_INTERNAL_TOKEN` on the server side.
    ///
    /// # Errors
    ///
    /// Returns an error if `reqwest::Client` cannot be constructed.
    pub fn new(
        server_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self, InternalClientError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            base_url: server_url.into().trim_end_matches('/').to_string(),
            token: SecretString::new(token.into().into()),
        })
    }

    /// Value of the internal authentication header.
    fn auth_value(&self) -> String {
        format!("Bearer {}", self.token.expose_secret())
    }

    /// Executes a request with retries on 5xx responses (max 3 attempts, exponential backoff).
    ///
    /// No retry on 409 (Conflict), 404, or 4xx responses in general.
    async fn execute_with_retry(
        &self,
        build_request: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, InternalClientError> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut last_err: Option<InternalClientError> = None;

        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                // Backoff exponentiel : 100ms, 200ms, 400ms
                let backoff_ms = 100u64 * (1u64 << (attempt - 1));
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }

            let req = build_request().header("X-Gradatum-Internal", self.auth_value());

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    // Réponses terminales (pas de retry) :
                    // - succès (2xx)
                    // - 4xx (client error, y compris 409 Conflict et 404)
                    if resp.status().is_success() || resp.status().is_client_error() {
                        return Ok(resp);
                    }
                    // 5xx → retry
                    let body = resp
                        .text()
                        .await
                        .unwrap_or_else(|_| "<lecture body échouée>".to_string());
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = MAX_ATTEMPTS,
                        status,
                        "internal_client: 5xx — retry"
                    );
                    last_err = Some(InternalClientError::ServerError { status, body });
                }
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = MAX_ATTEMPTS,
                        error = %e,
                        "internal_client: erreur réseau — retry"
                    );
                    last_err = Some(InternalClientError::Http(e));
                }
            }
        }

        Err(last_err.expect("MAX_ATTEMPTS > 0 — last_err est toujours Some ici"))
    }

    /// Parses the JSON response, or returns a semantic error (409, 404, 5xx).
    async fn parse_json<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
        key_for_err: &str,
    ) -> Result<T, InternalClientError> {
        let status = resp.status();
        if status.as_u16() == 409 {
            return Err(InternalClientError::Conflict {
                current_sha256_hex: None,
            });
        }
        if status.as_u16() == 404 {
            return Err(InternalClientError::NotFound {
                ulid: key_for_err.to_string(),
            });
        }
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<lecture body échouée>".to_string());
            return Err(InternalClientError::ServerError {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp.json::<T>().await?)
    }

    /// Checks the response status when no body is expected (e.g. 204 No Content for DELETE).
    async fn check_no_body(
        resp: reqwest::Response,
        key_for_err: &str,
    ) -> Result<(), InternalClientError> {
        let status = resp.status();
        if status.as_u16() == 404 {
            return Err(InternalClientError::NotFound {
                ulid: key_for_err.to_string(),
            });
        }
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<lecture body échouée>".to_string());
            return Err(InternalClientError::ServerError {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl InternalClient for InternalPersistClient {
    async fn persist_curated(
        &self,
        req: &PersistCuratedRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        let url = format!("{}/internal/v1/persist/curated", self.base_url);
        let resp = self
            .execute_with_retry(|| self.client.post(&url).json(req))
            .await?;
        Self::parse_json(resp, &req.note_id).await
    }

    async fn persist_embedding(
        &self,
        req: &PersistEmbeddingRequest,
    ) -> Result<EmbeddingOkResponse, InternalClientError> {
        let url = format!("{}/internal/v1/persist/embedding", self.base_url);
        let resp = self
            .execute_with_retry(|| self.client.post(&url).json(req))
            .await?;
        Self::parse_json(resp, &req.note_id).await
    }

    async fn persist_forget(
        &self,
        req: &PersistForgetRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        let url = format!("{}/internal/v1/persist/forget", self.base_url);
        let resp = self
            .execute_with_retry(|| self.client.post(&url).json(req))
            .await?;
        Self::parse_json(resp, &req.note_id).await
    }

    async fn persist_distill(
        &self,
        req: &PersistDistillRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        let url = format!("{}/internal/v1/persist/distill", self.base_url);
        let resp = self
            .execute_with_retry(|| self.client.post(&url).json(req))
            .await?;
        Self::parse_json(resp, &req.note_id).await
    }

    async fn delete_note(&self, ulid: &str) -> Result<(), InternalClientError> {
        let url = format!("{}/internal/v1/note/{ulid}", self.base_url);
        let resp = self.execute_with_retry(|| self.client.delete(&url)).await?;
        Self::check_no_body(resp, ulid).await
    }

    async fn get_note(&self, ulid: &str) -> Result<NoteReadDto, InternalClientError> {
        let url = format!("{}/internal/v1/note/{ulid}", self.base_url);
        let resp = self.execute_with_retry(|| self.client.get(&url)).await?;
        Self::parse_json(resp, ulid).await
    }

    async fn get_note_embedding(
        &self,
        ulid: &str,
        embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError> {
        let url = format!("{}/internal/v1/note/{ulid}/embedding", self.base_url);
        let resp = self
            .execute_with_retry(|| self.client.get(&url).query(&[("embedder_id", embedder_id)]))
            .await?;
        Self::parse_json(resp, ulid).await
    }

    async fn get_trust(&self, ulid: &str) -> Result<f32, InternalClientError> {
        let url = format!("{}/internal/v1/note/{ulid}/trust", self.base_url);
        let resp = self.execute_with_retry(|| self.client.get(&url)).await?;
        let dto: TrustReadDto = Self::parse_json(resp, ulid).await?;
        Ok(dto.trust)
    }

    async fn title_lookup(
        &self,
        tenant: &str,
        title: &str,
    ) -> Result<Option<String>, InternalClientError> {
        let url = format!("{}/internal/v1/title-lookup", self.base_url);
        let resp = self
            .execute_with_retry(|| {
                self.client
                    .get(&url)
                    .query(&[("tenant", tenant), ("title", title)])
            })
            .await?;
        let dto: TitleLookupDto = Self::parse_json(resp, title).await?;
        Ok(dto.note_id)
    }

    async fn id_lookup(
        &self,
        tenant: &str,
        note_id: &str,
    ) -> Result<Option<String>, InternalClientError> {
        let url = format!("{}/internal/v1/id-lookup", self.base_url);
        let resp = self
            .execute_with_retry(|| {
                self.client
                    .get(&url)
                    .query(&[("tenant", tenant), ("note_id", note_id)])
            })
            .await?;
        let dto: IdLookupDto = Self::parse_json(resp, note_id).await?;
        Ok(dto.note_id)
    }

    async fn list_notes_by_locus(
        &self,
        vault: &str,
        prefix: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        let url = format!("{}/internal/v1/notes/by-locus", self.base_url);
        let resp = self
            .execute_with_retry(|| {
                self.client
                    .get(&url)
                    .query(&[("vault", vault), ("prefix", prefix)])
            })
            .await?;
        let dto: NoteListDto = Self::parse_json(resp, vault).await?;
        Ok(dto.note_ids)
    }

    async fn list_by_status(
        &self,
        vault: &str,
        status: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        let url = format!("{}/internal/v1/notes/by-status", self.base_url);
        let resp = self
            .execute_with_retry(|| {
                self.client
                    .get(&url)
                    .query(&[("vault", vault), ("status", status)])
            })
            .await?;
        let dto: NoteListDto = Self::parse_json(resp, vault).await?;
        Ok(dto.note_ids)
    }

    async fn list_garbage(
        &self,
        vault: &str,
        before_ms: i64,
        grace_days: u32,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        let url = format!("{}/internal/v1/notes/garbage", self.base_url);
        let before_ms_str = before_ms.to_string();
        let grace_days_str = grace_days.to_string();
        let resp = self
            .execute_with_retry(|| {
                self.client.get(&url).query(&[
                    ("vault", vault),
                    ("before_ms", before_ms_str.as_str()),
                    ("grace_days", grace_days_str.as_str()),
                ])
            })
            .await?;
        let dto: NoteListDto = Self::parse_json(resp, vault).await?;
        Ok(dto.note_ids)
    }

    async fn search_fts_for_forget(
        &self,
        vault: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        let url = format!("{}/internal/v1/forget/search", self.base_url);
        let limit_str = limit.to_string();
        let resp = self
            .execute_with_retry(|| {
                self.client.get(&url).query(&[
                    ("vault", vault),
                    ("query", query),
                    ("limit", limit_str.as_str()),
                ])
            })
            .await?;
        let dto: NoteListDto = Self::parse_json(resp, vault).await?;
        Ok(dto.note_ids)
    }

    async fn list_notes_by_agent(
        &self,
        agent: &str,
        vaults: &[String],
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        let url = format!("{}/internal/v1/notes/by-agent", self.base_url);
        // Construire les query-params : agent + répétition vaults[]
        let mut params: Vec<(&str, &str)> = vec![("agent", agent)];
        // `reqwest::RequestBuilder::query` prend `impl Serialize`.
        // Pour répétition de paramètres, on utilise un Vec<(str, str)>.
        let vault_params: Vec<(&str, &str)> =
            vaults.iter().map(|v| ("vaults[]", v.as_str())).collect();
        params.extend(vault_params.iter().copied());

        let resp = self
            .execute_with_retry(|| self.client.get(&url).query(&params))
            .await?;
        let dto: NoteListDto = Self::parse_json(resp, agent).await?;
        Ok(dto.note_ids)
    }
}
