//! Anthropic Messages API ↔ internal translation layer.
//!
//! This module contains only pure functions with no I/O.
//! The Axum handler lives in `handlers::messages`.
//!
//! Modules:
//! - `translate`: `anthropic_to_chat`, `chat_to_anthropic`, `map_stop_reason`
//! - `stream`: SSE state machine — converts OpenAI chunks to Anthropic SSE events

pub mod stream;
pub mod translate;
