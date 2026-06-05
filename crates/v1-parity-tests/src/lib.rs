//! v1-parity-tests — crate test-only pour la parité fonctionnelle v1.6.x → Phase 1.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §0.4 A3.
//!
//! ## Objectif
//!
//! Suite d'intégration representative (~30 tests) couvrant les 8 domaines fonctionnels
//! du prédécesseur legacy-vault-v1 v1.6.x.
//!
//! ## Structure
//!
//! - `tests/common/mod.rs` : helpers partagés (builders Frontmatter, vault temp)
//! - `tests/vault_crud.rs` : 5 tests Vault CRUD + lifecycle
//! - `tests/curator_workflow.rs` : 4 tests Curator heuristic + LLM gating
//! - `tests/drift_e2e.rs` : 3 tests drift end-to-end
//! - `tests/cache_concurrency.rs` : 3 tests cache checksum + TTL
//! - `tests/index_search.rs` : 3 tests FTS5 + list_by_status
//! - `tests/audit_trail.rs` : 2 tests AuditEvent sérialisation
//! - `tests/markdown_roundtrip.rs` : 3 tests parse ↔ write idempotence
//! - `tests/persistence_reopen.rs` : 2 tests vault close+reopen
//!
//! ## Status Phase 2+ déféré
//!
//! Les tests bloqués par des stubs Phase 1 sont marqués `#[ignore]`
//! avec un commentaire `// Phase 2+ : blocked by <stub>`.
//!
//! Voir `docs/superpowers/notes/v1-test-inventory.md` pour l'inventaire complet.
