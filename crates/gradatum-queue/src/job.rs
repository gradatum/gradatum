//! Types `Job` and `JobStatus`.
//!
//! `JobStatus` is stored in the database as lowercase text (`pending`, `claimed`,
//! `done`, `failed`). `FromSql`/`ToSql` conversions handle serialization.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use ulid::Ulid;

/// State of a job in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    /// Waiting to be processed.
    Pending,
    /// Actively claimed — lease in progress.
    Claimed,
    /// Processing completed successfully.
    Done,
    /// Processing failed permanently (no automatic retry).
    Failed,
}

impl JobStatus {
    /// Returns the text representation stored in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

impl ToSql for JobStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for JobStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        match s {
            "pending" => Ok(Self::Pending),
            "claimed" => Ok(Self::Claimed),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            other => Err(FromSqlError::Other(
                format!("valeur JobStatus inconnue : {other}").into(),
            )),
        }
    }
}

/// A job in the queue.
///
/// `id` is a monotonic ULID that also serves as FIFO ordering via `created_at`.
#[derive(Debug, Clone)]
pub struct Job {
    /// Unique job identifier.
    pub id: Ulid,
    /// Work type (e.g. `embed_note`, `reindex_note`).
    pub kind: String,
    /// JSON-serialized payload, opaque at the queue level.
    pub payload_json: String,
    /// Current state.
    pub status: JobStatus,
    /// Lease expiry timestamp in Unix milliseconds.
    /// `None` when the job is `pending`, `done`, or `failed`.
    pub lease_until: Option<i64>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at: i64,
    /// Last-update timestamp in Unix milliseconds.
    pub updated_at: i64,
    /// Number of claim attempts (incremented on each `claim_one`).
    pub attempts: u32,
    /// Last error recorded via `fail()`.
    pub last_error: Option<String>,
}
