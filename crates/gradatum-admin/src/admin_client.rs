//! HTTP client for the internal admin API.
//!
//! Note deletion, archive listing/restore/purge and vault lifecycle operations are
//! deliberately absent from the public HTTP API and from the MCP surface. The operator
//! CLI reaches them through an internal loopback namespace (`127.0.0.1:19092` by
//! default), authenticated by a dedicated admin token read from a file — never from
//! `argv`, where it would be visible in the process table.
//!
//! Going through the running server is the only option, not a preference: the SQLite
//! index has a single owner, and that owner is the running `gradatum-server`. The CLI
//! cannot open `index.db` directly while the server holds it, so restore and purge are
//! performed in-process by the server, without downtime.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, bail};
use gradatum_dto::{
    VaultArchivesListRequest, VaultArchivesListResponse, VaultArchivesPurgeRequest,
    VaultArchivesPurgeResult, VaultArchivesRestoreRequest, VaultArchivesRestoreResult,
    VaultDeleteRequest, VaultLifecycleRequest, VaultLifecycleResponse, VaultPurgeRequest,
    VaultPurgeResponse,
};
use reqwest::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Default URL of the internal loopback API, matching the server's `[internal_api] bind`.
pub const DEFAULT_ADMIN_URL: &str = "http://127.0.0.1:19092";

/// Default path of the admin token file.
///
/// It is expected to be mode `0600` and readable only by `root` and the service user.
pub const DEFAULT_ADMIN_TOKEN_FILE: &str = "/etc/gradatum/admin.token";

/// Client for the internal admin API: loopback endpoint plus admin token.
pub struct AdminClient {
    http: Client,
    base_url: String,
    token: String,
}

impl std::fmt::Debug for AdminClient {
    /// Hand-written `Debug`: the admin token is redacted so it can never reach logs
    /// or traces through a derived implementation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminClient")
            .field("base_url", &self.base_url)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl AdminClient {
    /// Builds the client, reading the admin token from `token_file` — never from `argv`.
    ///
    /// Requests carry a 30-second timeout.
    ///
    /// # Errors
    ///
    /// - The token file cannot be read (missing, or permissions deny it).
    /// - The token is empty once trimmed.
    /// - The underlying HTTP client cannot be built.
    pub fn new(base_url: &str, token_file: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(token_file).with_context(|| {
            format!(
                "reading the admin token from {} (expected: 0600 file)",
                token_file.display()
            )
        })?;
        let token = raw.trim().to_string();
        if token.is_empty() {
            bail!("empty admin token in {}", token_file.display());
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building the admin HTTP client")?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Generic JSON POST to the admin API, carrying the admin authentication header.
    async fn post<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        body: &Req,
    ) -> anyhow::Result<Resp> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("X-Gradatum-Admin", format!("Bearer {}", self.token))
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {url} (LIVE server required)"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("response {status} from {url}: {text}");
        }
        serde_json::from_str(&text)
            .with_context(|| format!("deserialization of the {url} response: {text}"))
    }

    /// `POST /internal/v1/admin/delete` — on-demand deletion, which archives rather
    /// than destroys.
    ///
    /// The response shape depends on the mode: a deletion preview in dry-run, a deletion
    /// result otherwise. The two are structurally distinct, so the raw JSON is returned
    /// as-is for display instead of being forced into a single type.
    ///
    /// # Errors
    ///
    /// Network failure, non-2xx HTTP status, or a response body that fails to deserialize.
    pub async fn delete(&self, req: &VaultDeleteRequest) -> anyhow::Result<serde_json::Value> {
        self.post("/internal/v1/admin/delete", req).await
    }

    /// `POST /internal/v1/admin/archives/list` — lists the archive registry.
    ///
    /// # Errors
    ///
    /// Network failure, non-2xx HTTP status, or a response body that fails to deserialize.
    pub async fn archives_list(
        &self,
        req: &VaultArchivesListRequest,
    ) -> anyhow::Result<VaultArchivesListResponse> {
        self.post("/internal/v1/admin/archives/list", req).await
    }

    /// `POST /internal/v1/admin/archives/purge` — on-demand purge, guarded by a
    /// dry-run then an explicit confirmation.
    ///
    /// # Errors
    ///
    /// Network failure, non-2xx HTTP status, or a response body that fails to deserialize.
    pub async fn archives_purge(
        &self,
        req: &VaultArchivesPurgeRequest,
    ) -> anyhow::Result<VaultArchivesPurgeResult> {
        self.post("/internal/v1/admin/archives/purge", req).await
    }

    /// `POST /internal/v1/admin/archives/restore` — restores an archive into quarantine,
    /// guarded by a dry-run then an explicit confirmation.
    ///
    /// # Errors
    ///
    /// Network failure, non-2xx HTTP status (`404` no such archive, `409` ULID collision),
    /// or a response body that fails to deserialize.
    pub async fn archives_restore(
        &self,
        req: &VaultArchivesRestoreRequest,
    ) -> anyhow::Result<VaultArchivesRestoreResult> {
        self.post("/internal/v1/admin/archives/restore", req).await
    }

    /// `POST /internal/v1/admin/vaults/create` — provisions a vault.
    ///
    /// # Errors
    ///
    /// Network failure, non-2xx HTTP status (`400` malformed vault id), or a response
    /// body that fails to deserialize.
    pub async fn vault_create(
        &self,
        req: &VaultLifecycleRequest,
    ) -> anyhow::Result<VaultLifecycleResponse> {
        self.post("/internal/v1/admin/vaults/create", req).await
    }

    /// `POST /internal/v1/admin/vaults/suspend` — freezes a vault: subsequent operations
    /// are rejected immediately, and the freeze is reversible.
    ///
    /// # Errors
    ///
    /// Network failure, non-2xx HTTP status (`403` on the `main` vault, `404` unknown
    /// vault), or a response body that fails to deserialize.
    pub async fn vault_suspend(
        &self,
        req: &VaultLifecycleRequest,
    ) -> anyhow::Result<VaultLifecycleResponse> {
        self.post("/internal/v1/admin/vaults/suspend", req).await
    }

    /// `POST /internal/v1/admin/vaults/delete` — soft deletion: the vault is marked
    /// deleted, and physical removal is deferred to an explicit purge.
    ///
    /// # Errors
    ///
    /// Network failure, non-2xx HTTP status (`403` on the `main` vault, `404` unknown
    /// vault), or a response body that fails to deserialize.
    pub async fn vault_soft_delete(
        &self,
        req: &VaultLifecycleRequest,
    ) -> anyhow::Result<VaultLifecycleResponse> {
        self.post("/internal/v1/admin/vaults/delete", req).await
    }

    /// `POST /internal/v1/admin/vaults/purge` — physically removes a soft-deleted vault.
    ///
    /// # Errors
    ///
    /// Network failure, non-2xx HTTP status (`400` missing confirmation or malformed
    /// vault id, `403` on the `main` vault, `404` unknown vault, `409` vault not
    /// soft-deleted), or a response body that fails to deserialize.
    pub async fn vault_purge(&self, req: &VaultPurgeRequest) -> anyhow::Result<VaultPurgeResponse> {
        self.post("/internal/v1/admin/vaults/purge", req).await
    }
}
