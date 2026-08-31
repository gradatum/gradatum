//! `gradatum-dto` — shared HTTP wire-contract DTOs for gradatum.
//!
//! Single source of truth for the `Vault*Request` structs consumed by:
//! - `gradatum-server`: deserialization of HTTP requests at `/api/v1/*`
//! - `gradatum-mcp-stub`: `inputSchema` generation for MCP tools via the `schemars` feature
//!
//! `gradatum-sdk-rs` does **not** depend on this crate — it is a placeholder with no client
//! surface in `2.0.0`.
//!
//! This crate depends only on `gradatum-core` for the `TenantId` and `VaultId`
//! newtypes. Both are `#[serde(transparent)]`, so the wire stays a bare `String` (a plain
//! JSON string); only the compile-time type is strengthened, separating the
//! *principal* axis (`TenantId`) from the *namespace* axis (`VaultId`). Other domain
//! values — ULIDs, section names — remain flat `String`s.
//!
//! Enable the `schemars` feature to auto-derive `JsonSchema` on all request structs.
//! Without the feature: only `serde::Serialize` + `Deserialize`.
//!
//! The `schemars` feature also enables the `mcp_schema` module, the single source of truth
//! for building the JSON schemas of the MCP tools.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use gradatum_core::scope::VaultId;

mod code_scope;
mod create_feature_card;
mod event_log;
mod internal;
mod job_status;
mod lessons_recall;
mod proactive_recall;
mod session_trace;
mod vault_admin;
mod vault_archives;
mod vault_classify;
mod vault_context;
mod vault_delete;
mod vault_downgrade;
mod vault_forget;
mod vault_graph;
mod vault_history;
mod vault_links;
mod vault_list;
mod vault_read;
mod vault_search;
mod vault_tags;
mod vault_timeline;
mod vault_trace;
mod vault_write;
mod vault_write_conflict;

#[cfg(feature = "schemars")]
mod mcp_schema;
#[cfg(feature = "schemars")]
pub use mcp_schema::{mcp_empty_params_schema, mcp_tool_schema};

pub use code_scope::{
    CodeScopeEntry, CodeScopeRequest, CodeScopeResponse, CodeSelectorDto,
    DEFAULT_BODY_BUDGET_TOKENS, DEFAULT_BUDGET_TOKENS, MAX_CALLERS_PER_ENTRY, SELECTOR_KINDS,
    is_valid_selector_kind,
};
pub use create_feature_card::{CreateFeatureCardRequest, CreateFeatureCardResponse};
pub use event_log::{EventLogResponse, QaEventDto};
pub use job_status::JobStatusRequest;
pub use lessons_recall::{
    LESSON_CLASSES, LessonHit, LessonsRecallRequest, LessonsRecallResponse, RankMode,
    is_valid_lesson_class,
};
pub use proactive_recall::{
    ProactiveHit, ProactiveRecallFeedbackRequest, ProactiveRecallRequest, ProactiveRecallResponse,
};
pub use session_trace::{SessionTraceRequest, SessionTraceResponse};
pub use vault_admin::{
    VaultLifecycleRequest, VaultLifecycleResponse, VaultPurgeRequest, VaultPurgeResponse,
};
pub use vault_archives::{
    ArchiveEntryDto, VaultArchivesListRequest, VaultArchivesListResponse,
    VaultArchivesPurgeRequest, VaultArchivesPurgeResult, VaultArchivesRestoreRequest,
    VaultArchivesRestoreResult,
};
pub use vault_classify::{VaultClassifyRequest, VaultClassifyResponse};
pub use vault_context::{ContextMode, ScoringWeights, VaultContextRequest};
pub use vault_delete::{DeletePreview, DeleteResult, DeletedNoteBackup, VaultDeleteRequest};
pub use vault_downgrade::{NoteStatusPatch, VaultDowngradeRequest, VaultDowngradeResponse};
pub use vault_forget::{
    ExcludedNote, ForgetPreview, ForgetScopeDto, ForgottenListResponse, ForgottenNoteEntry,
    MAX_FORGOTTEN_BY_LEN, UnforgotResponse, VaultForgetRequest,
};
pub use vault_graph::VaultGraphRequest;
pub use vault_history::{
    VaultDiffRequest, VaultDiffResponse, VaultHistoryGetRequest, VaultHistoryGetResponse,
    VaultHistoryRequest, VaultHistoryResponse, VaultRestoreRequest, VaultRestoreResponse,
};
pub use vault_links::VaultLinksRequest;
pub use vault_list::VaultListRequest;
pub use vault_read::VaultReadRequest;
pub use vault_search::{VaultSearchRequest, escape_like};
pub use vault_tags::VaultTagsRequest;
pub use vault_timeline::VaultTimelineRequest;
pub use vault_trace::VaultTraceRequest;
pub use vault_write::VaultWriteRequest;
pub use vault_write_conflict::WriteConflictDto;

/// Default namespace vault — [`VaultId`] `"main"`.
///
/// Used via `#[serde(default = "default_main_vault")]` on the `vault_id` field of
/// `ArchiveEntryDto` (archive registry row mirror, whose `vault_id` echoes
/// `notes.vault_id`, defaulting to `"main"` for pre-multi-vault rows).
pub fn default_main_vault() -> VaultId {
    VaultId::new("main")
}

pub use internal::{
    CuratorDecisionDto, EmbeddingOkResponse, LinkDto, PersistCuratedRequest, PersistDistillRequest,
    PersistEmbeddingRequest, PersistForgetRequest, PersistOkResponse, TemporalEntryDto,
};
