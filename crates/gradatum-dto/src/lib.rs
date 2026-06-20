//! `gradatum-dto` — shared HTTP wire-contract DTOs for gradatum.
//!
//! Single source of truth for the `Vault*Request` structs consumed by:
//! - `gradatum-server`: deserialization of HTTP requests at `/api/v1/*`
//! - `gradatum-mcp-stub`: `inputSchema` generation for MCP tools via the `schemars` feature
//! - `gradatum-sdk-rs`: typed Rust client
//!
//! DAG level: **L0** (zero workspace dependencies). No coupling with `gradatum-core`
//! to preserve wire-contract purity (flat `String` types, no domain `ULID`/`TenantId`).
//!
//! Enable the `schemars` feature to auto-derive `JsonSchema` on all request structs.
//! Without the feature: only `serde::Serialize` + `Deserialize`.
//!
//! The `schemars` feature also enables the `mcp_schema` module — SSOT pour la
//! construction des schémas JSON des outils MCP (anti-duplication 34e70eb).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod code_scope;
mod event_log;
mod internal;
mod lessons_recall;
mod session_trace;
mod vault_classify;
mod vault_context;
mod vault_downgrade;
mod vault_forget;
mod vault_graph;
mod vault_history;
mod vault_links;
mod vault_list;
mod vault_read;
mod vault_search;
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
pub use event_log::{EventLogResponse, QaEventDto};
pub use lessons_recall::{
    LESSON_CLASSES, LessonHit, LessonsRecallRequest, LessonsRecallResponse, is_valid_lesson_class,
};
pub use session_trace::{SessionTraceRequest, SessionTraceResponse};
pub use vault_classify::{VaultClassifyRequest, VaultClassifyResponse};
pub use vault_context::VaultContextRequest;
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
pub use vault_timeline::VaultTimelineRequest;
pub use vault_trace::VaultTraceRequest;
pub use vault_write::VaultWriteRequest;
pub use vault_write_conflict::WriteConflictDto;

/// Default tenant ID — `"main"` (legacy vault v1.6.2 parity).
///
/// Used via `#[serde(default = "default_main")]` on the `tenant_id` field.
pub fn default_main() -> String {
    "main".to_string()
}

pub use internal::{
    EmbeddingOkResponse, LinkDto, PersistCuratedRequest, PersistDistillRequest,
    PersistEmbeddingRequest, PersistForgetRequest, PersistOkResponse, TemporalEntryDto,
};
