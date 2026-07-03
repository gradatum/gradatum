//! Inline-vendored shared types (OSS-compatible, no private Cargo registry dependency).
//!
//! Contains the required types ported from the private shared library.
//! By vendoring here, the `gradatum-gateway` crate has no dependency on a
//! private Cargo registry.
//!
//! Only types actually used in this crate are included.

pub mod anthropic;
pub mod chat;
pub mod circuit_breaker;
pub mod embeddings;
pub mod error;
pub mod provider;
pub mod streaming;
