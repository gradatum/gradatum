//! Stubs — file kept for git history; all stubs have been removed.
//!
//! Removal history:
//! - `NoopQueue` removed, then the whole legacy queue (F-177): the `jobs_v2`
//!   queue was dropped in 2.1.0 — `AppState` no longer carries a legacy queue.
//! - `VaultRegistryStub` removed: `AppState.vault` = `Arc<dyn Registry>`.
//! - `SearchEngineStub` removed: `AppState.search` = `Arc<SqliteIndex>`.
//!
//! This file is empty — kept for git history.
