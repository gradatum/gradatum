//! `gradatum-dto` — DTOs partagés contrats wire HTTP gradatum.
//!
//! Single source of truth pour les `Vault*Request` structs consommés par :
//! - `gradatum-server` : désérialisation des requêtes HTTP `/api/v1/*`
//! - `gradatum-mcp-stub` : génération `inputSchema` MCP via feature `schemars`
//! - `gradatum-sdk-rs` : typage client Rust (Phase 2.1)
//!
//! Niveau DAG : **L0** (0 dep workspace). Aucun couplage avec `gradatum-core`
//! pour préserver la pureté des contrats wire (types `String` plats, pas de
//! ULID/TenantId domaine).
//!
//! Activer la feature `schemars` pour auto-dériver `JsonSchema` sur tous les
//! Request structs. Sans feature : seulement `serde::Serialize` + `Deserialize`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod event_log;
mod vault_classify;
mod vault_context;
mod vault_downgrade;
mod vault_graph;
mod vault_links;
mod vault_list;
mod vault_read;
mod vault_search;
mod vault_trace;
mod vault_write;

pub use event_log::{EventLogResponse, QaEventDto};
pub use vault_classify::VaultClassifyRequest;
pub use vault_context::VaultContextRequest;
pub use vault_downgrade::{NoteStatusPatch, VaultDowngradeRequest, VaultDowngradeResponse};
pub use vault_graph::VaultGraphRequest;
pub use vault_links::VaultLinksRequest;
pub use vault_list::VaultListRequest;
pub use vault_read::VaultReadRequest;
pub use vault_search::VaultSearchRequest;
pub use vault_trace::VaultTraceRequest;
pub use vault_write::VaultWriteRequest;

/// Default tenant ID — `"main"` (parité historique legacy vault v1.6.2).
///
/// Utilisé via `#[serde(default = "default_main")]` sur le champ `tenant_id`.
pub fn default_main() -> String {
    "main".to_string()
}
