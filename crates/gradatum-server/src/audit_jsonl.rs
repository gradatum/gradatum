//! JsonlFileSink — production audit sink with daily rotation.
//!
//! Audit log JSONL mode 0640, rotation by UTC date.
//! Atomic `dropped_total` counter accessible from test fixtures to verify I/O errors.
//!
//! ## Output files
//!
//! `${base_dir}/audit.YYYY-MM-DD.jsonl` with permissions `0640`.
//!
//! Rotation triggers as soon as the UTC date of the event changes relative
//! to the current file's date. One file per day.
//!
//! ## Concurrency
//!
//! The current file is protected by a `tokio::sync::Mutex`. Writes are
//! serialized — suited for HTTP audit throughput (< 10 k req/s).
//! For higher throughput, consider a channel + dedicated task.
//!
//! ## dropped_total counter
//!
//! Each `record` call that returns an I/O error atomically increments
//! `dropped_total`. This counter is readable via `dropped_total()` for
//! test fixtures and monitoring.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use gradatum_core::audit::http::{AuditSink, HttpAuditEvent};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;

/// Internal sink state: open file + current date.
// Phase 2.1 : Inner sera utilisé quand JsonlFileSink sera câblé dans AppState.
#[allow(dead_code)]
struct Inner {
    /// Current file date in `YYYY-MM-DD` format.
    current_date: String,
    /// Tokio handle on the file being written.
    file: tokio::fs::File,
}

/// JSONL audit sink with daily rotation.
///
/// Events are serialized as single-line JSON terminated by `\n`, then
/// flushed immediately to minimize loss on crash.
///
/// Rotation occurs at the UTC day boundary (based on `event.ts`).
///
/// ## dropped_total counter
///
/// Incremented on every I/O error (disk full, insufficient permissions, etc.).
/// Accessible via [`JsonlFileSink::dropped_total`] for test fixtures
/// and monitoring.
// Phase 2.1 : JsonlFileSink sera câblé dans AppState (with_audit_dir).
#[allow(dead_code)]
pub struct JsonlFileSink {
    /// Base directory for audit files.
    base_dir: PathBuf,
    /// Current file + date, protected by a tokio mutex.
    current: Arc<Mutex<Option<Inner>>>,
    /// Atomic counter of events not persisted due to I/O error.
    ///
    /// Incremented in `record` on each `Err`. Never decremented.
    /// Accessible from tests to verify saturation.
    dropped_total: Arc<AtomicU64>,
}

// Phase 2.1 : méthodes câblées dans AppState::with_audit_dir.
#[allow(dead_code)]
impl JsonlFileSink {
    /// Creates a new sink that writes its files into `base_dir`.
    ///
    /// The directory is created automatically on the first call to `record`.
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            current: Arc::new(Mutex::new(None)),
            dropped_total: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns the cumulative count of events not persisted due to I/O error.
    ///
    /// Atomic value — Relaxed load, sufficient for monitoring and tests.
    /// Accessible from test fixtures.
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total.load(Ordering::Relaxed)
    }

    /// Internal recording logic — called from `record`.
    ///
    /// Separated to allow `record` to encapsulate error counting
    /// without code duplication (Extract Inner pattern for the dropped counter).
    async fn record_inner(&self, event: HttpAuditEvent) -> Result<(), std::io::Error> {
        let today = event.ts.format("%Y-%m-%d").to_string();
        let mut guard = self.current.lock().await;

        // Rotation : premier appel ou franchissement de minuit UTC.
        let needs_rotate = guard
            .as_ref()
            .is_none_or(|inner| inner.current_date != today);

        if needs_rotate {
            let file = self.open_file_for_date(&today).await?;
            *guard = Some(Inner {
                current_date: today.clone(),
                file,
            });
        }

        // SAFETY : guard est Some après la rotation ci-dessus.
        let inner = guard
            .as_mut()
            .expect("Inner est Some — initialisé juste au-dessus");

        let line = serde_json::to_string(&event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        inner.file.write_all(line.as_bytes()).await?;
        inner.file.write_all(b"\n").await?;
        inner.file.flush().await?;

        Ok(())
    }

    /// Opens (or creates) the `audit.{date}.jsonl` file in append mode 0640.
    async fn open_file_for_date(&self, date: &str) -> Result<tokio::fs::File, std::io::Error> {
        tokio::fs::create_dir_all(&self.base_dir).await?;
        let path = self.base_dir.join(format!("audit.{date}.jsonl"));
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o640)
            .open(&path)
            .await
    }
}

#[async_trait]
impl AuditSink for JsonlFileSink {
    /// Records `event` as JSONL in `audit.YYYY-MM-DD.jsonl`.
    ///
    /// ## Behavior
    ///
    /// - Creates the base directory if absent.
    /// - Rotates if the UTC date of the event differs from the current file's date.
    /// - Each line is flushed immediately.
    ///
    /// ## Errors
    ///
    /// `std::io::Error` on I/O failure (dir creation, open, write, flush).
    /// JSON serialization fails only if the event contains non-serializable values
    /// (e.g., NaN in `curator`) — `InvalidData` error.
    ///
    /// ## dropped_total counter
    ///
    /// Any returned error increments `dropped_total`.
    /// This includes directory creation, file open, write, and flush errors.
    async fn record(&self, event: HttpAuditEvent) -> Result<(), std::io::Error> {
        let result = self.record_inner(event).await;
        if result.is_err() {
            self.dropped_total.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("audit sink : événement non persisté — dropped_total incrémenté");
        }
        result
    }
}
