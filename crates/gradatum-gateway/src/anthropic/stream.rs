//! SSE state machine: converts an OpenAI `ChatCompletionStream` into Anthropic SSE events.
//!
//! This module is **purely functional**: it transforms a `ChatCompletionStream`
//! (a stream of OpenAI chunks) into a stream of `Bytes` in the Anthropic SSE format.
//!
//! # Anthropic SSE format
//! Each event has the form:
//! ```text
//! event: <type>\ndata: <json compact>\n\n
//! ```
//!
//! # Event sequence
//! 1. `message_start` — opens the message
//! 2. `ping` — keep-alive
//! 3. For each content block:
//!    - `content_block_start` — opens the block (type `text` or `tool_use`)
//!    - `content_block_delta` — delta fragment (`text_delta` or `input_json_delta`)
//!    - `content_block_stop` — closes the block
//! 4. `message_delta` — stop_reason + output usage
//! 5. `message_stop` — end of message
//!
//! # State machine
//! - Text block state: may be open or closed. If a tool_call arrives while a text
//!   block is open, the text block is closed first (`content_block_stop`).
//! - Each `tool_calls[].index` from OpenAI is mapped to a monotonically increasing
//!   Anthropic block index.
//! - The `openai_tool_index → anthropic_block_index` map tracks open blocks.
//!
//! # Error handling
//! - Any backend stream error produces an Anthropic `error` event followed by a clean close.
//! - No `.unwrap()` outside tests; no panics in production.

use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::time::MissedTickBehavior;

use crate::anthropic::translate::map_stop_reason;
use crate::commons::provider::ChatCompletionStream;

/// Item of an Anthropic SSE stream: infallible `Bytes`, compatible with `Body::from_stream`.
type SseItem = Result<Bytes, std::convert::Infallible>;

// ── Helpers SSE ──────────────────────────────────────────────────────────────

/// Formats one Anthropic SSE event: `event: <type>\ndata: <json>\n\n`.
///
/// The JSON payload is compact (no indentation), as required by the Anthropic format.
///
/// On a serialization failure, logs a warning and returns empty `Bytes` — the event is
/// skipped rather than propagated. Never panics.
fn format_sse_event(event_type: &str, data: &Value) -> Bytes {
    match serde_json::to_string(data) {
        Ok(json) => {
            let line = format!("event: {}\ndata: {}\n\n", event_type, json);
            Bytes::from(line)
        }
        Err(e) => {
            tracing::warn!(
                event_type = %event_type,
                error = %e,
                "Anthropic SSE event serialization failed — event skipped"
            );
            Bytes::new()
        }
    }
}

/// Builds the `content_block_stop` event for the given block index.
fn block_stop_event(index: u32) -> Bytes {
    format_sse_event(
        "content_block_stop",
        &json!({ "type": "content_block_stop", "index": index }),
    )
}

/// Builds the Anthropic `message_start` event that opens the message.
fn message_start_event(model: &str, message_id: &str) -> Bytes {
    format_sse_event(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            }
        }),
    )
}

/// Builds the Anthropic `ping` keep-alive event.
fn ping_event() -> Bytes {
    format_sse_event("ping", &json!({ "type": "ping" }))
}

/// Builds the Anthropic `message_stop` event that ends the message.
fn message_stop_event() -> Bytes {
    format_sse_event("message_stop", &json!({ "type": "message_stop" }))
}

/// Builds the Anthropic `message_delta` event carrying `stop_reason` and output usage.
fn message_delta_event(stop_reason: &str, output_tokens: u32) -> Bytes {
    format_sse_event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason, "stop_sequence": null },
            "usage": { "output_tokens": output_tokens }
        }),
    )
}

/// Returns `true` when the SSE item is the terminal `message_stop` event.
///
/// Used by [`keepalive_anthropic_sse`] during its content phase to know whether the
/// content stream has already closed the Anthropic message. That stream only emits
/// `message_stop` on the last chunk's `finish_reason` (or on a backend error); an abnormal
/// termination — a TCP FIN with no `finish_reason`, for instance when the backend restarts
/// mid-prefill — leaves it absent, in which case the keep-alive wrapper must emit a safety
/// close itself.
///
/// Detection matches on the event prefix because the content stream yields opaque `Bytes`
/// (one event per item); that prefix is the only signal available without restructuring the
/// content stream shared with [`chunks_to_anthropic_sse`].
fn is_message_stop(item: &SseItem) -> bool {
    matches!(item, Ok(bytes) if bytes.starts_with(b"event: message_stop\n"))
}

/// Builds an Anthropic `error` event.
///
/// # Security — information disclosure
/// `message` MUST be a generic label supplied by the caller. No internal detail
/// (provider name, IP, port, backend failure reason) may travel through it.
fn error_event(error_type: &str, message: &str) -> Bytes {
    format_sse_event(
        "error",
        &json!({
            "type": "error",
            "error": { "type": error_type, "message": message }
        }),
    )
}

// ── Machine à états ──────────────────────────────────────────────────────────

/// State of the Anthropic SSE state machine.
///
/// Tracks which content blocks are open, so that `content_block_stop` is emitted at the
/// right moment.
struct StreamState {
    /// Open text block: `Some(anthropic_index)` while a text block is in progress.
    text_block_index: Option<u32>,
    /// Map `openai_tool_index → (anthropic_block_index, started)`.
    ///
    /// `started = false` means `content_block_start` has not been emitted yet — the
    /// machine waits until both `id` and `name` are known before opening the block.
    tool_blocks: HashMap<u32, (u32, bool)>,
    /// Anthropic block counter, incremented for every newly opened block.
    next_block_index: u32,
    /// `finish_reason` of the last chunk received, used to build `message_delta`.
    finish_reason: Option<String>,
    /// Output token counter, taken from the usage carried by chunks when available.
    output_tokens: u32,
}

impl StreamState {
    fn new() -> Self {
        Self {
            text_block_index: None,
            tool_blocks: HashMap::new(),
            next_block_index: 0,
            finish_reason: None,
            output_tokens: 0,
        }
    }

    /// Allocates and returns the next Anthropic block index.
    ///
    /// Uses `saturating_add` so that a pathological stream with 2³² or more blocks
    /// cannot panic on overflow in a debug build.
    fn alloc_block(&mut self) -> u32 {
        let idx = self.next_block_index;
        self.next_block_index = self.next_block_index.saturating_add(1);
        idx
    }
}

// ── Fonction principale ───────────────────────────────────────────────────────

/// Converts an OpenAI `ChatCompletionStream` into a stream of Anthropic SSE events.
///
/// Returns a `Stream<Item = Result<Bytes, std::convert::Infallible>>`, directly usable
/// with `axum::body::Body::from_stream`.
///
/// # Arguments
/// - `stream` — the stream of OpenAI chunks produced by the dispatch layer;
/// - `model` — the Anthropic model name to echo back in `message_start`;
/// - `message_id` — the unique message identifier (for example `msg_<ulid>`).
///
/// # Errors
/// No error is ever propagated: a backend failure becomes an Anthropic `error` event and
/// the stream then terminates cleanly.
pub fn chunks_to_anthropic_sse(
    stream: ChatCompletionStream,
    model: String,
    message_id: String,
) -> impl Stream<Item = SseItem> {
    // Préfixe d'ouverture : message_start + ping keep-alive.
    let prefix = futures::stream::iter(vec![
        Ok(message_start_event(&model, &message_id)),
        Ok(ping_event()),
    ]);
    prefix.chain(chunks_to_anthropic_content_sse(stream))
}

/// Converts a `ChatCompletionStream` into **content-only** Anthropic SSE events.
///
/// Runs the content state machine (`content_block_*`, `message_delta`, `message_stop`, and
/// the `error` event on a backend failure) **without** the `message_start` / `ping` prefix.
///
/// This lets the content conversion be reused when `message_start` has already been emitted
/// upstream — see [`keepalive_anthropic_sse`], which opens the stream immediately and
/// interleaves `ping` events during prefill before splicing in this content stream.
fn chunks_to_anthropic_content_sse(
    stream: ChatCompletionStream,
) -> impl Stream<Item = SseItem> + Send {
    // Machine à états sur le flux de chunks. Les events de clôture (`message_delta`,
    // `message_stop`) sont émis par `process_chunk` au `finish_reason` du dernier chunk.
    let state = StreamState::new();
    stream
        .scan(state, |state, chunk_result| {
            let events = process_chunk(state, chunk_result);
            futures::future::ready(Some(events))
        })
        .flat_map(futures::stream::iter)
}

// ── Ouverture immédiate + keep-alive pendant le prefill ────────────────────────

/// Outcome of a deferred streaming dispatch, consumed by [`keepalive_anthropic_sse`].
///
/// Decouples the purely functional SSE module from the dispatch and error layers: the
/// handler resolves the dispatch, records its metrics, and hands over either the backend
/// chunk stream or an already-neutralized error descriptor.
pub enum StreamDispatch {
    /// The backend returned a chunk stream — its content is spliced into the SSE stream.
    Ready(ChatCompletionStream),
    /// The dispatch failed before producing a stream — an `error` event is emitted.
    Failed {
        /// Anthropic error type label carried in the `error` event.
        error_type: &'static str,
        /// Client-facing message. MUST be generic: no provider name, host, port, or
        /// backend failure detail may travel through it (information disclosure).
        message: String,
    },
}

/// Internal phase of the [`keepalive_anthropic_sse`] state machine.
enum KeepAlivePhase {
    /// Emit `message_start`, then move to the waiting phase.
    Start {
        message_start: Bytes,
        dispatch: BoxFuture<'static, StreamDispatch>,
        ticker: tokio::time::Interval,
    },
    /// Race the dispatch resolution against the next periodic `ping`.
    Waiting {
        dispatch: BoxFuture<'static, StreamDispatch>,
        ticker: tokio::time::Interval,
    },
    /// Splice in the backend content stream (content events up to `message_stop`).
    ///
    /// The `ticker` is carried over from the `Waiting` phase: `provider.stream()` resolves
    /// as soon as the response headers arrive, but the **first token** — the end of the
    /// backend prefill — only lands on the first poll of this stream. Without pings during
    /// that `headers → first token` window, the client would idle out despite the keep-alive
    /// of the `Waiting` phase. The content stream is therefore raced against a periodic
    /// ping; an Anthropic SSE `ping` is valid at any point in the stream.
    ///
    /// `stop_emitted` becomes `true` as soon as the content stream has emitted its
    /// `message_stop` (through `finish_reason` or an error). If it is still `false` when the
    /// stream ends — an abnormal termination — a safety close is emitted.
    Content {
        stream: Pin<Box<dyn Stream<Item = SseItem> + Send>>,
        ticker: tokio::time::Interval,
        stop_emitted: bool,
    },
    /// Emit the `error` event, then `message_stop`.
    Error {
        error_type: &'static str,
        message: String,
    },
    /// Emit the final `message_stop` after an error.
    Stop,
    /// Terminal phase.
    Done,
}

/// Opens an Anthropic SSE stream **immediately** and keeps the connection alive with
/// periodic `ping` events while `dispatch` — the backend prefill — proceeds in the
/// background.
///
/// # Motivation
/// A self-hosted backend such as `llama-server` emits neither its HTTP response headers
/// nor its first token until prefill completes, which can take well over a minute on a
/// large context. If the `Response` were only built once the dispatch resolved, **not a
/// single byte** would reach the client during prefill; the client would hit its idle
/// timeout, cancel the request, and the retry would land on a degraded fallback.
///
/// Here, `message_start` is sent at t=0 and a `ping` follows every `ping_period` until
/// `dispatch` resolves. The dispatch-internal fallback decision is unchanged: it keys on
/// connection and header failures, **not** on the first token — a long prefill is not an
/// error.
///
/// # Sequence
/// `message_start` → `ping`* (both while waiting for the dispatch and during backend
/// prefill, i.e. the `headers → first token` window) → then either the backend content
/// (`content_block_*` … `message_stop`) or `error` + `message_stop` if the dispatch failed.
/// If the backend stream ends without a `finish_reason` (abnormal termination), a safety
/// close (`message_delta` + `message_stop`) is emitted.
///
/// # Robustness
/// No error is ever propagated: a dispatch failure becomes an Anthropic `error` event
/// carrying a generic message, followed by `message_stop`. This function never panics.
pub fn keepalive_anthropic_sse(
    dispatch: BoxFuture<'static, StreamDispatch>,
    model: String,
    message_id: String,
    ping_period: Duration,
) -> impl Stream<Item = SseItem> + Send {
    let message_start = message_start_event(&model, &message_id);
    let mut ticker = tokio::time::interval(ping_period);
    // Pas de rafale de pings rétroactive si le flux n'est pas polled pendant un temps.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let init = KeepAlivePhase::Start {
        message_start,
        dispatch,
        ticker,
    };

    futures::stream::unfold(init, |phase| async move {
        // Boucle pour enchaîner les transitions sans output (Start→Waiting, Ready→Content…).
        let mut phase = phase;
        loop {
            match phase {
                KeepAlivePhase::Start {
                    message_start,
                    dispatch,
                    ticker,
                } => {
                    return Some((
                        Ok(message_start),
                        KeepAlivePhase::Waiting { dispatch, ticker },
                    ));
                }
                KeepAlivePhase::Waiting {
                    mut dispatch,
                    mut ticker,
                } => {
                    tokio::select! {
                        // `biased` : privilégie la résolution du dispatch sur un tick de ping
                        // (évite un ping superflu quand les deux sont prêts simultanément).
                        biased;
                        outcome = &mut dispatch => match outcome {
                            StreamDispatch::Ready(stream) => {
                                phase = KeepAlivePhase::Content {
                                    stream: Box::pin(chunks_to_anthropic_content_sse(stream)),
                                    ticker,
                                    stop_emitted: false,
                                };
                                continue;
                            }
                            StreamDispatch::Failed { error_type, message } => {
                                phase = KeepAlivePhase::Error { error_type, message };
                                continue;
                            }
                        },
                        _ = ticker.tick() => {
                            return Some((
                                Ok(ping_event()),
                                KeepAlivePhase::Waiting { dispatch, ticker },
                            ));
                        }
                    }
                }
                KeepAlivePhase::Content {
                    mut stream,
                    mut ticker,
                    stop_emitted,
                } => {
                    tokio::select! {
                        // `biased` : privilégie le contenu engine sur un tick de ping
                        // (évite un ping superflu quand contenu et tick sont prêts ensemble).
                        // `StreamExt::next()` et `Interval::tick()` sont cancel-safe : la
                        // branche perdante est simplement re-polled au tour suivant.
                        biased;
                        item = stream.next() => match item {
                            Some(item) => {
                                let stop_emitted = stop_emitted || is_message_stop(&item);
                                return Some((
                                    item,
                                    KeepAlivePhase::Content { stream, ticker, stop_emitted },
                                ));
                            }
                            None => {
                                if stop_emitted {
                                    // Le flux de contenu a déjà émis `message_stop` — terminer.
                                    phase = KeepAlivePhase::Done;
                                    continue;
                                }
                                // Terminaison anormale : le flux engine s'est terminé (TCP FIN)
                                // sans `finish_reason` (crash/restart mid-prefill). Émettre une
                                // clôture de sécurité (`message_delta` + `message_stop`) pour ne
                                // pas laisser le client sur un message Anthropic non terminé.
                                return Some((
                                    Ok(message_delta_event("end_turn", 0)),
                                    KeepAlivePhase::Stop,
                                ));
                            }
                        },
                        _ = ticker.tick() => {
                            return Some((
                                Ok(ping_event()),
                                KeepAlivePhase::Content { stream, ticker, stop_emitted },
                            ));
                        }
                    }
                }
                KeepAlivePhase::Error {
                    error_type,
                    message,
                } => {
                    return Some((Ok(error_event(error_type, &message)), KeepAlivePhase::Stop));
                }
                KeepAlivePhase::Stop => {
                    return Some((Ok(message_stop_event()), KeepAlivePhase::Done));
                }
                KeepAlivePhase::Done => return None,
            }
        }
    })
}

/// Processes one OpenAI chunk and returns the Anthropic events it produces.
///
/// Mutates `state` to track which blocks are open. Returns a `Vec` of infallible items so
/// the result can be fed straight into `flat_map`.
fn process_chunk(
    state: &mut StreamState,
    chunk_result: crate::commons::error::LlmResult<crate::commons::streaming::ChatCompletionChunk>,
) -> Vec<Result<Bytes, std::convert::Infallible>> {
    let chunk = match chunk_result {
        Ok(c) => c,
        Err(e) => {
            // Logguer le détail côté serveur, PAS côté client (information disclosure V3).
            tracing::error!(error = %e, "Anthropic backend stream error");
            // Fermer les blocs ouverts proprement.
            let mut events = close_open_blocks(state);
            // Émettre un event `error` Anthropic avec message générique —
            // le détail de l'erreur backend ne doit PAS être exposé au client.
            events.push(Ok(error_event("api_error", "internal backend error")));
            // Clôture de sécurité.
            events.push(Ok(message_stop_event()));
            return events;
        }
    };

    let mut events: Vec<Result<Bytes, std::convert::Infallible>> = Vec::new();

    let choice = match chunk.choices.first() {
        Some(c) => c,
        None => return events,
    };

    // Capture finish_reason pour les events de clôture.
    if let Some(ref reason) = choice.finish_reason {
        state.finish_reason = Some(reason.clone());
    }

    let delta = &choice.delta;

    // ── Traitement du texte ──────────────────────────────────────────────────
    if let Some(ref text) = delta.content
        && !text.is_empty()
    {
        // Estimation output_tokens par accumulation chars/4 (heuristique identique à
        // count_tokens_inner). llama-server n'émet pas toujours un chunk usage final,
        // cette estimation garantit un résultat non-nul dans tous les cas.
        // Arrondi inférieur par delta — acceptable pour un usage budgétaire.
        state.output_tokens = state.output_tokens.saturating_add((text.len() / 4) as u32);

        // Ouvrir le bloc texte si pas encore ouvert.
        if state.text_block_index.is_none() {
            let idx = state.alloc_block();
            state.text_block_index = Some(idx);
            events.push(Ok(format_sse_event(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": { "type": "text", "text": "" }
                }),
            )));
        }

        let idx = state
            .text_block_index
            .expect("text_block_index was just initialized — cannot be None");
        events.push(Ok(format_sse_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": { "type": "text_delta", "text": text }
            }),
        )));
    }

    // ── Traitement des tool_calls ────────────────────────────────────────────
    if let Some(ref tool_calls) = delta.tool_calls {
        for tc in tool_calls {
            let openai_idx = tc.index;

            // Premier fragment de cet outil : id + name arrivent.
            if !state.tool_blocks.contains_key(&openai_idx) {
                // Fermer le bloc texte ouvert si présent.
                if let Some(text_idx) = state.text_block_index.take() {
                    events.push(Ok(block_stop_event(text_idx)));
                }

                let anthropic_idx = state.alloc_block();
                state.tool_blocks.insert(openai_idx, (anthropic_idx, false));
            }

            let (anthropic_idx, started) = state
                .tool_blocks
                .get_mut(&openai_idx)
                .expect("tool_blocks was just initialized for openai_idx");

            // Émettre content_block_start si on a id + name (premier fragment).
            if !*started
                && let (Some(id), Some(func)) = (tc.id.as_deref(), tc.function.as_ref())
                && let Some(ref name) = func.name
            {
                *started = true;
                let idx = *anthropic_idx;
                events.push(Ok(format_sse_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
                    }),
                )));
            }

            // Émettre input_json_delta pour les fragments d'arguments.
            if let Some(ref func) = tc.function
                && let Some(ref args_fragment) = func.arguments
                && !args_fragment.is_empty()
            {
                // Accumulation output_tokens pour les args d'outil (même heuristique chars/4).
                state.output_tokens = state
                    .output_tokens
                    .saturating_add((args_fragment.len() / 4) as u32);

                let idx = *anthropic_idx;
                events.push(Ok(format_sse_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "input_json_delta", "partial_json": args_fragment }
                    }),
                )));
            }
        }
    }

    // ── Events de clôture sur finish_reason ─────────────────────────────────
    if choice.finish_reason.is_some() {
        // Fermer tous les blocs ouverts dans l'ordre.
        events.extend(close_open_blocks(state));

        // message_delta avec stop_reason mappé.
        let stop_reason = state
            .finish_reason
            .as_deref()
            .map(map_stop_reason)
            .unwrap_or("end_turn");

        events.push(Ok(message_delta_event(stop_reason, state.output_tokens)));

        events.push(Ok(message_stop_event()));
    }

    events
}

/// Closes every open content block (text and tool calls), in index order.
///
/// Returns the matching `content_block_stop` events, sorted by ascending index.
fn close_open_blocks(state: &mut StreamState) -> Vec<Result<Bytes, std::convert::Infallible>> {
    let mut to_close: Vec<u32> = Vec::new();

    if let Some(text_idx) = state.text_block_index.take() {
        to_close.push(text_idx);
    }

    for (anthropic_idx, started) in state.tool_blocks.values() {
        if *started {
            to_close.push(*anthropic_idx);
        }
    }

    // Tri par index croissant pour un ordre déterministe.
    to_close.sort_unstable();
    // Dédupliquer (ne devrait pas se produire mais défensif).
    to_close.dedup();

    to_close
        .into_iter()
        .map(|idx| Ok(block_stop_event(idx)))
        .collect()
}

// ── Tests unitaires ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commons::streaming::{
        ChatCompletionChunk, ChunkChoice, ChunkDelta, ChunkToolCall, ChunkToolCallFunction,
    };
    use futures::StreamExt;

    // ─── Helpers test ─────────────────────────────────────────────────────────

    /// Construit un `ChatCompletionStream` depuis une liste de chunks (succès).
    fn stream_from_chunks(
        chunks: Vec<ChatCompletionChunk>,
    ) -> crate::commons::provider::ChatCompletionStream {
        Box::pin(futures::stream::iter(
            chunks
                .into_iter()
                .map(Ok::<_, crate::commons::error::LlmError>),
        ))
    }

    /// Collecte tous les bytes d'un flux SSE Anthropic et retourne les lignes non-vides.
    async fn collect_sse_lines(
        stream: impl Stream<Item = Result<Bytes, std::convert::Infallible>>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        futures::pin_mut!(stream);
        while let Some(Ok(bytes)) = stream.next().await {
            if bytes.is_empty() {
                continue;
            }
            let text = String::from_utf8(bytes.to_vec()).expect("SSE doit être UTF-8");
            for line in text.lines() {
                if !line.is_empty() {
                    lines.push(line.to_string());
                }
            }
        }
        lines
    }

    /// Parse les events depuis les lignes SSE collectées.
    /// Retourne une `Vec<(event_type, Value)>`.
    fn parse_events(lines: &[String]) -> Vec<(String, Value)> {
        let mut events = Vec::new();
        let mut current_event: Option<String> = None;

        for line in lines {
            if let Some(event_type) = line.strip_prefix("event: ") {
                current_event = Some(event_type.to_string());
            } else if let Some(ref evt) = current_event.clone()
                && let Some(json_str) = line.strip_prefix("data: ")
            {
                let data: Value =
                    serde_json::from_str(json_str).expect("event data doit être JSON valide");
                events.push((evt.clone(), data));
                current_event = None;
            }
        }

        events
    }

    /// Chunk texte simple.
    fn text_chunk(id: &str, text: &str, finish: Option<&str>) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "qwen3".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: Some(text.to_string()),
                    tool_calls: None,
                },
                finish_reason: finish.map(str::to_string),
            }],
        }
    }

    /// Chunk avec finish_reason uniquement (final).
    fn finish_chunk(id: &str, finish: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "qwen3".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: None,
                },
                finish_reason: Some(finish.to_string()),
            }],
        }
    }

    /// Premier chunk d'un tool_call (id + name).
    fn tool_call_start_chunk(
        chunk_id: &str,
        tc_index: u32,
        tc_id: &str,
        name: &str,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: chunk_id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "qwen3".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![ChunkToolCall {
                        index: tc_index,
                        id: Some(tc_id.to_string()),
                        tool_type: Some("function".to_string()),
                        function: Some(ChunkToolCallFunction {
                            name: Some(name.to_string()),
                            arguments: None,
                        }),
                    }]),
                },
                finish_reason: None,
            }],
        }
    }

    /// Chunk d'arguments partiels d'un tool_call.
    fn tool_call_args_chunk(
        chunk_id: &str,
        tc_index: u32,
        args_fragment: &str,
        finish: Option<&str>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: chunk_id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "qwen3".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![ChunkToolCall {
                        index: tc_index,
                        id: None,
                        tool_type: None,
                        function: Some(ChunkToolCallFunction {
                            name: None,
                            arguments: Some(args_fragment.to_string()),
                        }),
                    }]),
                },
                finish_reason: finish.map(str::to_string),
            }],
        }
    }

    // ─── Tests ────────────────────────────────────────────────────────────────

    /// SC1 — Texte seul : séquence complète d'events Anthropic.
    ///
    /// Entrée : 2 chunks texte + 1 chunk finish.
    /// Attendu : message_start → ping → content_block_start(text) →
    ///           content_block_delta(text_delta)×2 → content_block_stop →
    ///           message_delta → message_stop.
    #[tokio::test]
    async fn stream_text_only_correct_event_sequence() {
        let chunks = vec![
            text_chunk("c1", "Bonjour ", None),
            text_chunk("c2", "monde.", None),
            finish_chunk("c3", "stop"),
        ];

        let stream = stream_from_chunks(chunks);
        let sse = chunks_to_anthropic_sse(
            stream,
            "claude-3-5-sonnet-20241022".to_string(),
            "msg_001".to_string(),
        );

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);

        // Extraire les types d'events dans l'ordre.
        let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

        assert!(
            types.starts_with(&["message_start", "ping"]),
            "doit commencer par message_start puis ping, obtenu: {:?}",
            types
        );

        // Vérifier la présence et l'ordre des events de contenu.
        let content_start_pos = types
            .iter()
            .position(|&t| t == "content_block_start")
            .expect("content_block_start doit être présent");
        let message_delta_pos = types
            .iter()
            .position(|&t| t == "message_delta")
            .expect("message_delta doit être présent");
        let message_stop_pos = types
            .iter()
            .position(|&t| t == "message_stop")
            .expect("message_stop doit être présent");

        assert!(
            content_start_pos < message_delta_pos,
            "content_block_start doit précéder message_delta"
        );
        assert!(
            message_delta_pos < message_stop_pos,
            "message_delta doit précéder message_stop"
        );

        // Vérifier le contenu de message_start.
        let (_, msg_start_data) = &events[0];
        assert_eq!(msg_start_data["message"]["id"], "msg_001");
        assert_eq!(
            msg_start_data["message"]["model"],
            "claude-3-5-sonnet-20241022"
        );
        assert_eq!(msg_start_data["message"]["role"], "assistant");

        // Vérifier content_block_start type=text.
        let (_, block_start_data) = &events[content_start_pos];
        assert_eq!(block_start_data["content_block"]["type"], "text");

        // Vérifier les text_delta.
        let text_deltas: Vec<_> = events
            .iter()
            .filter(|(t, d)| t == "content_block_delta" && d["delta"]["type"] == "text_delta")
            .collect();
        assert_eq!(
            text_deltas.len(),
            2,
            "doit avoir 2 text_delta (un par chunk texte non vide)"
        );
        assert_eq!(text_deltas[0].1["delta"]["text"], "Bonjour ");
        assert_eq!(text_deltas[1].1["delta"]["text"], "monde.");

        // Vérifier content_block_stop présent.
        assert!(
            types.contains(&"content_block_stop"),
            "content_block_stop doit être présent"
        );

        // Vérifier stop_reason dans message_delta.
        let (_, msg_delta_data) = &events[message_delta_pos];
        assert_eq!(
            msg_delta_data["delta"]["stop_reason"], "end_turn",
            "stop→end_turn via map_stop_reason"
        );
    }

    /// SC2 — Un tool_call : content_block_start(tool_use) + input_json_delta + stop.
    ///
    /// Entrée : chunk start (id+name) + chunk args + chunk finish.
    /// Attendu : message_start → ping → content_block_start(tool_use) →
    ///           content_block_delta(input_json_delta) → content_block_stop →
    ///           message_delta(stop_reason:tool_use) → message_stop.
    #[tokio::test]
    async fn stream_single_tool_call_correct_events() {
        let chunks = vec![
            tool_call_start_chunk("c1", 0, "call_abc", "get_weather"),
            tool_call_args_chunk("c2", 0, r#"{"location":"#, None),
            tool_call_args_chunk("c3", 0, r#""Paris"}"#, None),
            finish_chunk("c4", "tool_calls"),
        ];

        let stream = stream_from_chunks(chunks);
        let sse = chunks_to_anthropic_sse(
            stream,
            "claude-3-5-sonnet-20241022".to_string(),
            "msg_002".to_string(),
        );

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);
        let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

        // content_block_start doit être de type tool_use.
        let start_pos = types
            .iter()
            .position(|&t| t == "content_block_start")
            .expect("content_block_start doit être présent");
        assert_eq!(
            events[start_pos].1["content_block"]["type"], "tool_use",
            "le bloc doit être tool_use"
        );
        assert_eq!(events[start_pos].1["content_block"]["id"], "call_abc");
        assert_eq!(events[start_pos].1["content_block"]["name"], "get_weather");

        // input_json_delta présents.
        let json_deltas: Vec<_> = events
            .iter()
            .filter(|(t, d)| t == "content_block_delta" && d["delta"]["type"] == "input_json_delta")
            .collect();
        assert_eq!(json_deltas.len(), 2, "doit avoir 2 input_json_delta");
        assert_eq!(json_deltas[0].1["delta"]["partial_json"], r#"{"location":"#);
        assert_eq!(json_deltas[1].1["delta"]["partial_json"], r#""Paris"}"#);

        // content_block_stop présent.
        assert!(
            types.contains(&"content_block_stop"),
            "content_block_stop doit être présent"
        );

        // stop_reason = tool_use.
        let delta_pos = types
            .iter()
            .position(|&t| t == "message_delta")
            .expect("message_delta doit être présent");
        assert_eq!(
            events[delta_pos].1["delta"]["stop_reason"], "tool_use",
            "tool_calls→tool_use via map_stop_reason"
        );
    }

    /// SC3 — Texte puis tool_call : ordre correct des blocs (texte index 0, tool index 1).
    ///
    /// Quand un tool_call arrive après du texte, le bloc texte doit être fermé avant
    /// l'ouverture du bloc tool_use.
    #[tokio::test]
    async fn stream_text_then_tool_call_correct_block_order() {
        let chunks = vec![
            text_chunk("c1", "Je vais chercher.", None),
            tool_call_start_chunk("c2", 0, "call_xyz", "get_weather"),
            tool_call_args_chunk("c3", 0, r#"{"location":"Lyon"}"#, None),
            finish_chunk("c4", "tool_calls"),
        ];

        let stream = stream_from_chunks(chunks);
        let sse = chunks_to_anthropic_sse(stream, "m".to_string(), "msg_003".to_string());

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);
        let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

        // Deux content_block_start : texte (index 0) puis tool_use (index 1).
        let starts: Vec<_> = events
            .iter()
            .filter(|(t, _)| t == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 2, "doit avoir 2 content_block_start");
        assert_eq!(starts[0].1["content_block"]["type"], "text");
        assert_eq!(starts[0].1["index"], 0);
        assert_eq!(starts[1].1["content_block"]["type"], "tool_use");
        assert_eq!(starts[1].1["index"], 1);

        // Un content_block_stop intermédiaire (fermeture du texte avant le tool_use).
        // Puis un content_block_stop final (fermeture du tool_use).
        let stops = types.iter().filter(|&&t| t == "content_block_stop").count();
        assert_eq!(
            stops, 2,
            "doit avoir 2 content_block_stop (texte + tool_use)"
        );

        // Vérifier que le content_block_stop du texte arrive AVANT le content_block_start
        // du tool_use (invariant machine à états).
        let text_stop_pos = types
            .iter()
            .position(|&t| t == "content_block_stop")
            .expect("premier content_block_stop");
        let tool_start_pos = types
            .iter()
            .rposition(|&t| t == "content_block_start")
            .expect("deuxième content_block_start");
        assert!(
            text_stop_pos < tool_start_pos,
            "fermeture bloc texte ({}) doit précéder ouverture bloc tool_use ({})",
            text_stop_pos,
            tool_start_pos
        );
    }

    /// SC4 — finish_reason "length" → stop_reason "max_tokens".
    #[tokio::test]
    async fn stream_finish_reason_length_becomes_max_tokens() {
        let chunks = vec![
            text_chunk("c1", "Texte tronqué.", None),
            finish_chunk("c2", "length"),
        ];

        let stream = stream_from_chunks(chunks);
        let sse = chunks_to_anthropic_sse(stream, "m".to_string(), "msg_004".to_string());

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);

        let delta = events
            .iter()
            .find(|(t, _)| t == "message_delta")
            .expect("message_delta doit être présent");
        assert_eq!(
            delta.1["delta"]["stop_reason"], "max_tokens",
            "length→max_tokens via map_stop_reason"
        );
    }

    /// SC5 — Erreur backend dans le flux → event error + message_stop, pas de panic.
    #[tokio::test]
    async fn stream_backend_error_emits_error_event_then_stops() {
        use crate::commons::error::LlmError;

        let error_stream: crate::commons::provider::ChatCompletionStream =
            Box::pin(futures::stream::iter(vec![
                Ok(text_chunk("c1", "Début...", None)),
                Err(LlmError::ProviderUnavailable {
                    provider: "test".to_string(),
                    reason: "connexion perdue".to_string(),
                }),
            ]));

        let sse = chunks_to_anthropic_sse(error_stream, "m".to_string(), "msg_005".to_string());

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);
        let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

        assert!(
            types.contains(&"error"),
            "doit contenir un event error, obtenu: {:?}",
            types
        );
        // L'event error contient le type correct + message générique (V3 : pas de détail LlmError).
        let error_event = events.iter().find(|(t, _)| t == "error").unwrap();
        assert_eq!(error_event.1["error"]["type"], "api_error");
        // Message doit être générique et ne PAS exposer le détail interne "connexion perdue".
        let msg = error_event.1["error"]["message"].as_str().unwrap_or("");
        assert!(
            !msg.contains("connexion perdue"),
            "le détail LlmError ne doit pas être exposé dans le message SSE"
        );
        assert!(!msg.is_empty(), "message ne doit pas être vide");

        // message_stop doit être émis même en cas d'erreur.
        assert!(
            types.contains(&"message_stop"),
            "message_stop doit être émis même après une erreur"
        );
    }

    /// SC6 — message_start contient bien l'id et le modèle.
    #[tokio::test]
    async fn stream_message_start_contains_id_and_model() {
        let chunks = vec![text_chunk("c1", "ok", None), finish_chunk("c2", "stop")];
        let stream = stream_from_chunks(chunks);
        let sse = chunks_to_anthropic_sse(
            stream,
            "my-model-alias".to_string(),
            "msg_custom_id".to_string(),
        );

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);

        let (_, start_data) = events
            .first()
            .expect("message_start doit être le premier event");
        assert_eq!(start_data["message"]["id"], "msg_custom_id");
        assert_eq!(start_data["message"]["model"], "my-model-alias");
    }

    /// V3 (security-reviewer) — Erreur backend : le message SSE `error` doit être générique.
    ///
    /// Le détail de l'erreur interne (ex: `LlmError::ProviderUnavailable` avec raison et
    /// provider) ne doit pas être transmis au client Anthropic pour éviter la fuite
    /// d'informations internes.
    ///
    /// Le message doit être `"internal backend error"` (ou équivalent générique) —
    /// PAS le contenu de `LlmError::to_string()` qui exposerait provider/reason.
    #[tokio::test]
    async fn stream_error_event_message_is_generic_not_internal_detail() {
        use crate::commons::error::LlmError;

        let sensitive_detail =
            "provider=my-secret-backend,reason=connexion refused port 8080,ip=10.0.0.5";

        let error_stream: crate::commons::provider::ChatCompletionStream = Box::pin(
            futures::stream::iter(vec![Err(LlmError::ProviderUnavailable {
                provider: sensitive_detail.to_string(),
                reason: "connexion refused".to_string(),
            })]),
        );

        let sse = chunks_to_anthropic_sse(error_stream, "m".to_string(), "msg_v3".to_string());

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);

        let error_event = events
            .iter()
            .find(|(t, _)| t == "error")
            .expect("event 'error' doit être émis");

        let msg = error_event.1["error"]["message"].as_str().unwrap_or("");

        // Le message doit être générique : NE PAS contenir le détail LlmError.
        assert!(
            !msg.contains(sensitive_detail),
            "le message SSE error ne doit pas exposer le détail interne LlmError, obtenu: '{}'",
            msg
        );
        // Et NE PAS contenir d'adresse IP ou de port interne.
        assert!(
            !msg.contains("10.0.0"),
            "le message SSE error ne doit pas exposer d'adresse IP interne, obtenu: '{}'",
            msg
        );
        // Le type reste api_error.
        assert_eq!(error_event.1["error"]["type"], "api_error");
    }

    /// C2 — output_tokens non-nul après streaming multi-deltas.
    ///
    /// Régression : `state.output_tokens` était toujours 0 (jamais incrémenté),
    /// ce qui causait `"usage": {"output_tokens": 0}` dans le `message_delta` final.
    ///
    /// Fix : accumulation chars/4 à chaque `text_delta` et `input_json_delta`.
    /// Heuristique documentée (pas un tokenizer réel — même logique que count_tokens_inner).
    ///
    /// Entrée : 3 text_delta ("Hello " = 6 chars, "world" = 5 chars, "!" = 1 char).
    /// Total = 12 chars → ~3 tokens (12/4). La valeur exacte peut varier (arrondi inférieur).
    #[tokio::test]
    async fn streaming_message_delta_reports_nonzero_output_tokens() {
        // 3 text_delta de contenu connu.
        let chunks = vec![
            text_chunk("c1", "Hello ", None),
            text_chunk("c2", "world", None),
            text_chunk("c3", "!", None),
            finish_chunk("c4", "stop"),
        ];

        let stream = stream_from_chunks(chunks);
        let sse =
            chunks_to_anthropic_sse(stream, "claude-test".to_string(), "msg_c2test".to_string());

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);

        // Extraire l'event message_delta.
        let delta_event = events
            .iter()
            .find(|(t, _)| t == "message_delta")
            .expect("message_delta doit être présent");

        let output_tokens = delta_event.1["usage"]["output_tokens"]
            .as_u64()
            .expect("usage.output_tokens doit être un entier");

        // output_tokens doit être > 0 (régression critique).
        assert!(
            output_tokens > 0,
            "output_tokens doit être > 0 après 3 text_delta, obtenu: {}",
            output_tokens
        );

        // Vérifier la cohérence approximative : 12 chars / 4 = 3 tokens.
        // Tolérance ±1 pour arrondi inférieur par delta.
        // "Hello " = 6/4 = 1, "world" = 5/4 = 1, "!" = 1/4 = 0 → total accumulé = 2.
        // On vérifie juste que c'est dans une plage raisonnable [1, 5].
        assert!(
            output_tokens <= 5,
            "output_tokens doit être dans une plage raisonnable [1, 5], obtenu: {}",
            output_tokens
        );
    }

    /// V6 (security-reviewer P2) — `alloc_block` ne doit pas paniquer à `u32::MAX`.
    ///
    /// L'overflow arithmétique en debug Rust panique. `saturating_add` doit être
    /// utilisé pour éviter toute panique sur un flux pathologique (≥ 2³² blocs).
    #[test]
    fn alloc_block_saturates_at_u32_max_no_panic() {
        let mut state = StreamState::new();
        state.next_block_index = u32::MAX;
        // Premier appel : retourne u32::MAX lui-même.
        let idx = state.alloc_block();
        assert_eq!(idx, u32::MAX);
        // Deuxième appel : ne doit PAS paniquer (overflow arithmétique en debug).
        // Avec saturating_add, la valeur reste à u32::MAX indéfiniment.
        let idx2 = state.alloc_block();
        assert_eq!(
            idx2,
            u32::MAX,
            "alloc_block doit saturer silencieusement à u32::MAX"
        );
    }

    // ─── Tests keep-alive (ouverture immédiate du stream) ──────────────────────

    /// KA1 — `message_start` + `ping` périodiques sont émis AVANT tout chunk de contenu,
    /// pendant qu'un dispatch lent (prefill simulé) progresse.
    ///
    /// Régression incident b9780 : sans keep-alive, aucun byte n'atteignait le client
    /// pendant ~110s de prefill → idle-timeout → annulation → fallback → 400.
    #[tokio::test]
    async fn keepalive_emits_start_and_pings_before_engine_content() {
        // Flux engine simulé : 1 chunk texte + finish.
        let engine_stream: ChatCompletionStream = Box::pin(futures::stream::iter(vec![
            Ok(text_chunk("c1", "Salut", None)),
            Ok(finish_chunk("c2", "stop")),
        ]));

        // Dispatch qui tarde 120 ms avant de livrer le flux (simule un prefill long).
        let dispatch: BoxFuture<'static, StreamDispatch> = Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            StreamDispatch::Ready(engine_stream)
        });

        let sse = keepalive_anthropic_sse(
            dispatch,
            "claude-test".to_string(),
            "msg_ka".to_string(),
            Duration::from_millis(20), // ping toutes les 20 ms
        );

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);
        let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

        // 1. Premier event = message_start (stream ouvert dès t=0).
        assert_eq!(
            types.first().copied(),
            Some("message_start"),
            "le premier event doit être message_start, obtenu: {:?}",
            types
        );

        // 2. Le contenu engine arrive bien.
        let first_content = types
            .iter()
            .position(|&t| t == "content_block_start")
            .expect("content_block_start doit être présent après le prefill");

        // 3. Plusieurs pings périodiques précèdent le contenu (keep-alive pendant le prefill).
        let pings_before_content = types[..first_content]
            .iter()
            .filter(|&&t| t == "ping")
            .count();
        assert!(
            pings_before_content >= 2,
            "des pings périodiques doivent précéder le contenu engine (prefill 120ms / ping 20ms), \
             obtenu {} : {:?}",
            pings_before_content,
            types
        );

        // 4. Le contenu suit et se termine par message_stop.
        assert!(
            types.contains(&"content_block_delta"),
            "le contenu engine doit être présent, obtenu: {:?}",
            types
        );
        assert_eq!(
            types.last().copied(),
            Some("message_stop"),
            "le dernier event doit être message_stop, obtenu: {:?}",
            types
        );
    }

    /// KA2 — Dispatch en échec : `message_start` est déjà parti, puis `error` + `message_stop`.
    ///
    /// Une fois le stream ouvert (200 + message_start), une erreur dispatch ne peut plus être
    /// un code HTTP — elle devient un event SSE `error` (type conservé, message générique).
    #[tokio::test]
    async fn keepalive_failed_dispatch_emits_error_then_stop_after_start() {
        let dispatch: BoxFuture<'static, StreamDispatch> = Box::pin(async {
            StreamDispatch::Failed {
                error_type: "overloaded_error",
                message: "internal backend error".to_string(),
            }
        });

        let sse = keepalive_anthropic_sse(
            dispatch,
            "m".to_string(),
            "msg_err".to_string(),
            Duration::from_millis(50),
        );

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);
        let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

        assert_eq!(
            types.first().copied(),
            Some("message_start"),
            "le stream doit avoir été ouvert (message_start) avant l'erreur, obtenu: {:?}",
            types
        );
        assert!(
            types.contains(&"error"),
            "un event error doit être émis, obtenu: {:?}",
            types
        );
        assert_eq!(
            types.last().copied(),
            Some("message_stop"),
            "message_stop doit clôturer le stream après l'erreur, obtenu: {:?}",
            types
        );

        let error_evt = events
            .iter()
            .find(|(t, _)| t == "error")
            .expect("event error présent");
        assert_eq!(
            error_evt.1["error"]["type"], "overloaded_error",
            "le error.type fourni par le dispatch doit être préservé"
        );
        assert_eq!(
            error_evt.1["error"]["message"], "internal backend error",
            "le message doit rester générique (V3 information disclosure)"
        );
    }

    /// KA3 (P1-A) — Les `ping` couvrent la fenêtre de PREFILL (`headers→premier token`),
    /// pas seulement l'attente du dispatch.
    ///
    /// Régression durcissement b9780 : le dispatch résout sur l'arrivée des **headers**, mais
    /// `provider.stream()` est paresseux → le premier token n'arrive qu'au premier poll du flux
    /// de contenu (le prefill, ~70-110s). Sans ticker dans la phase `Content`, aucun ping
    /// n'est émis pendant cette fenêtre → idle-timeout client malgré le keep-alive de `Waiting`.
    ///
    /// Ici le dispatch résout **immédiatement** (zéro ping en phase `Waiting`), mais le premier
    /// chunk de contenu tarde 120 ms → tous les pings observés avant le contenu proviennent
    /// nécessairement de la phase `Content` (couverture du prefill).
    #[tokio::test]
    async fn keepalive_pings_during_prefill_after_dispatch_ready() {
        // Flux engine : premier chunk différé de 120 ms (simule le prefill), puis finish.
        let engine_stream: ChatCompletionStream = Box::pin(
            futures::stream::once(async {
                tokio::time::sleep(Duration::from_millis(120)).await;
                Ok(text_chunk("c1", "Salut", None))
            })
            .chain(futures::stream::iter(vec![Ok(finish_chunk("c2", "stop"))])),
        );

        // Dispatch qui résout IMMÉDIATEMENT (headers prêts, mais prefill encore en cours).
        let dispatch: BoxFuture<'static, StreamDispatch> =
            Box::pin(async move { StreamDispatch::Ready(engine_stream) });

        let sse = keepalive_anthropic_sse(
            dispatch,
            "claude-test".to_string(),
            "msg_ka3".to_string(),
            Duration::from_millis(20), // ping toutes les 20 ms
        );

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);
        let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

        assert_eq!(
            types.first().copied(),
            Some("message_start"),
            "le premier event doit être message_start, obtenu: {:?}",
            types
        );

        let first_content = types
            .iter()
            .position(|&t| t == "content_block_start")
            .expect("content_block_start doit arriver après le prefill");

        // Des pings doivent précéder le contenu — émis pendant la phase Content (prefill),
        // car le dispatch a résolu sans délai (donc aucun ping en phase Waiting).
        let pings_before_content = types[..first_content]
            .iter()
            .filter(|&&t| t == "ping")
            .count();
        assert!(
            pings_before_content >= 2,
            "des pings doivent couvrir la fenêtre de prefill post-dispatch (120ms / ping 20ms), \
             obtenu {} : {:?}",
            pings_before_content,
            types
        );

        // Le contenu suit et le stream se clôt proprement.
        assert!(
            types.contains(&"content_block_delta"),
            "le contenu engine doit être présent, obtenu: {:?}",
            types
        );
        assert_eq!(
            types.last().copied(),
            Some("message_stop"),
            "le dernier event doit être message_stop, obtenu: {:?}",
            types
        );
    }

    /// KA4 (P2) — Terminaison anormale du flux engine (TCP FIN sans `finish_reason`) →
    /// clôture de sécurité : `message_delta` + `message_stop` sont tout de même émis.
    ///
    /// Sans ce filet, un crash/restart engine mid-prefill laisse le client Anthropic sur un
    /// message non terminé (pas de `message_stop`).
    #[tokio::test]
    async fn keepalive_abnormal_end_without_finish_emits_safety_stop() {
        // Flux engine qui émet un chunk de contenu PUIS se termine sans finish_reason.
        let engine_stream: ChatCompletionStream = Box::pin(futures::stream::iter(vec![Ok(
            text_chunk("c1", "Salut", None),
        )]));

        let dispatch: BoxFuture<'static, StreamDispatch> =
            Box::pin(async move { StreamDispatch::Ready(engine_stream) });

        let sse = keepalive_anthropic_sse(
            dispatch,
            "m".to_string(),
            "msg_ka4".to_string(),
            Duration::from_millis(50),
        );

        let lines = collect_sse_lines(sse).await;
        let events = parse_events(&lines);
        let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

        // Le contenu partiel est bien passé.
        assert!(
            types.contains(&"content_block_delta"),
            "le contenu partiel doit être présent, obtenu: {:?}",
            types
        );

        // Clôture de sécurité : message_delta puis message_stop.
        let delta_pos = types
            .iter()
            .position(|&t| t == "message_delta")
            .expect("message_delta de sécurité doit être émis malgré l'absence de finish_reason");
        let stop_pos = types
            .iter()
            .position(|&t| t == "message_stop")
            .expect("message_stop de sécurité doit être émis");
        assert!(
            delta_pos < stop_pos,
            "message_delta doit précéder message_stop, obtenu: {:?}",
            types
        );
        assert_eq!(
            types.last().copied(),
            Some("message_stop"),
            "le dernier event doit être message_stop, obtenu: {:?}",
            types
        );

        // stop_reason de la clôture de sécurité = end_turn.
        let (_, delta_data) = &events[delta_pos];
        assert_eq!(
            delta_data["delta"]["stop_reason"], "end_turn",
            "la clôture de sécurité doit utiliser stop_reason=end_turn"
        );

        // Pas de double message_stop.
        let stop_count = types.iter().filter(|&&t| t == "message_stop").count();
        assert_eq!(
            stop_count, 1,
            "exactement un message_stop (pas de double clôture), obtenu: {:?}",
            types
        );
    }
}
