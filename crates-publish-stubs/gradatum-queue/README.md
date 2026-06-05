# gradatum-queue

> SQLite-backed durable job queue with atomic lease acquisition and automatic recovery.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Structs

```rust
/// Durable job queue backed by SQLite.
/// Guarantees: atomic claim (UPDATE…RETURNING), lease recovery, 4 mandatory PRAGMAs.
pub struct Queue { ... }

impl Queue {
    /// Open (or create) a queue at the given SQLite file path.
    pub async fn open(path: &Path) -> Result<Self, QueueError>

    /// Open an in-memory queue (tests / ephemeral use).
    pub async fn open_in_memory() -> Result<Self, QueueError>

    /// Enqueue a job. Returns the job ULID.
    pub async fn enqueue(
        &self,
        job_type: &str,
        payload: &str,
    ) -> Result<String, QueueError>

    /// Claim the next available job with a lease of `lease_ms` milliseconds.
    /// Returns None if no job is available.
    pub async fn claim_one(
        &self,
        lease_ms: u64,
    ) -> Result<Option<Job>, QueueError>

    /// Mark a job as completed (removes from queue).
    pub async fn complete(&self, id: &str) -> Result<(), QueueError>

    /// Mark a job as failed (increments attempts, releases lease).
    pub async fn fail(&self, id: &str, reason: &str) -> Result<(), QueueError>

    /// Recover expired leases (called periodically by gradatum-worker).
    pub async fn recover_expired(&self) -> Result<u64, QueueError>
}

pub struct Job {
    pub id: String,          // ULID
    pub job_type: String,
    pub payload: String,     // JSON
    pub attempts: u32,
    pub created_at: DateTime<Utc>,
    pub leased_until: DateTime<Utc>,
}

pub enum JobStatus {
    Pending,
    Leased { until: DateTime<Utc> },
    Completed,
    Failed { reason: String, attempts: u32 },
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    UlidParse(ulid::DecodeError),
}
```

## Guarantees

- **Atomic claim**: `UPDATE…RETURNING` ensures at-most-one consumer per job under concurrency.
- **Lease recovery**: expired leases automatically become claimable; `attempts` is incremented.
- **4 PRAGMAs on open**: `WAL`, `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0