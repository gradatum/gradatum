//! A fake sidecar that reproduces the server's contract.
//!
//! Everything here is derived from `tools/privacy-sidecar/src/protocol.h` and
//! `src/server.cpp`, which are the source of truth for the wire. Where the fake
//! deliberately diverges — a truncated frame, a body that is not JSON — the
//! script asks for it explicitly.

// The module is shared by more than one test crate, and each one exercises a
// different slice of it: what one leaves unused is live in the other, so the
// subset a given test binary happens to touch is not the liveness question.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Hard cap on an inbound frame, from the protocol header.
pub const MAX_FRAME_BYTES: u64 = 16 * 1024 * 1024;

/// Largest identifier the protocol admits, in bytes.
pub const MAX_ID_BYTES: usize = 64;

/// True when `id` is what the server admits: non-empty, at most
/// [`MAX_ID_BYTES`] long, and made only of `[A-Za-z0-9._:-]`.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// The handshake a conformant sidecar announces.
///
/// `mapping_v` is the label-mapping revision the sidecar applies — a signed
/// integer in the protocol — and `labels_v` is a `sha256:`-tagged digest over
/// the model's label vocabulary, not a number.
pub fn handshake(binary_sha256: &str, model_sha256: &str, labels_v: &str, mapping_v: i64) -> Value {
    json!({
        "binary_sha256": binary_sha256,
        "model_sha256": model_sha256,
        "mapping_v": mapping_v,
        "labels_v": labels_v,
        "score_mode": "mean_token_prob",
        "ready": true,
    })
}

/// What the fake answers for one request.
///
/// The sizes and statuses a conformant sidecar produces are *not* scripted:
/// they are produced by the enforcement in [`serve`], so a script cannot
/// accidentally answer `input_too_large` for a body the server would have
/// scanned, nor answer at all for a request the server would have refused.
pub enum Reply {
    /// A scanned window that found these spans.
    Spans(Vec<Value>),
    /// A verbatim JSON frame, for an answer outside the format.
    Json(Value),
    /// A frame whose body is these raw bytes — not valid JSON.
    RawBody(Vec<u8>),
    /// A frame header declaring this many body bytes, with no body behind it.
    DeclaredLength(usize),
    /// Close the connection without answering.
    Close,
    /// Answer a conformant scan after this delay.
    Slow(Duration),
    /// Read the request, then never answer.
    Silent,
}

/// Scripted answers, keyed by connection and by request within it.
///
/// `connection` counts from 1 and `request` counts from 1 within each
/// connection; a reconnection is therefore observable from inside the script.
pub type ReplyFn = Box<dyn Fn(u64, u64, Value) -> Reply + Send + Sync>;

/// Counters the fake keeps about what it was asked.
#[derive(Default)]
struct Counters {
    connections: AtomicU64,
    requests: AtomicU64,
    /// Requests the protocol refused — bad JSON, a missing or non-string
    /// field, or an identifier outside the admitted alphabet.
    refused: AtomicU64,
}

/// A fake sidecar, listening on a socket of its own inside a temporary
/// directory. The listening thread is detached and dies with the test process;
/// the socket path itself is removed when the handle is dropped.
pub struct FakeSidecar {
    _dir: tempfile::TempDir,
    path: PathBuf,
    counters: Arc<Counters>,
    seen: Arc<Mutex<Vec<Value>>>,
    max_input_bytes: usize,
    _thread: JoinHandle<()>,
}

impl FakeSidecar {
    /// Starts a fake that announces `handshake` on every connection and answers
    /// each admitted request with whatever `reply` returns.
    ///
    /// Returns only once the socket is bound and accepting.
    pub fn start<F>(handshake: Value, reply: F) -> Self
    where
        F: Fn(u64, u64, Value) -> Reply + Send + Sync + 'static,
    {
        Self::start_limited(handshake, 262_144, reply)
    }

    /// As [`start`](Self::start), with the refusal threshold the server applies
    /// to one body.
    pub fn start_limited<F>(handshake: Value, max_input_bytes: usize, reply: F) -> Self
    where
        F: Fn(u64, u64, Value) -> Reply + Send + Sync + 'static,
    {
        let dir = tempfile::TempDir::new().expect("a temporary directory");
        let path = dir.path().join("sidecar.sock");
        let counters = Arc::new(Counters::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        let thread = {
            let path = path.clone();
            let counters = Arc::clone(&counters);
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a tokio runtime");
                runtime.block_on(async move {
                    let Ok(listener) = UnixListener::bind(&path) else {
                        // Nothing can be served; releasing the starter is still
                        // required or the test would hang waiting for a socket
                        // that will never exist.
                        let _ = ready_tx.send(());
                        return;
                    };
                    let _ = ready_tx.send(());
                    let reply: Arc<ReplyFn> = Arc::new(Box::new(reply));
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            continue;
                        };
                        let connection = counters.connections.fetch_add(1, Ordering::SeqCst) + 1;
                        let reply = Arc::clone(&reply);
                        let counters = Arc::clone(&counters);
                        let seen = Arc::clone(&seen);
                        let handshake = handshake.clone();
                        tokio::spawn(async move {
                            serve(
                                stream,
                                handshake,
                                reply,
                                connection,
                                max_input_bytes,
                                counters,
                                seen,
                            )
                            .await;
                        });
                    }
                });
            })
        };

        ready_rx.recv().expect("the fake sidecar starts");

        Self {
            _dir: dir,
            path,
            counters,
            seen,
            max_input_bytes,
            _thread: thread,
        }
    }

    /// Path of the socket the fake listens on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of connections the fake has accepted.
    pub fn connections(&self) -> u64 {
        self.counters.connections.load(Ordering::SeqCst)
    }

    /// Number of requests the fake has admitted, across all connections.
    pub fn requests(&self) -> u64 {
        self.counters.requests.load(Ordering::SeqCst)
    }

    /// Number of requests the protocol refused, across all connections.
    ///
    /// A client that speaks the protocol leaves this at zero: the counter is
    /// how a request the server would have dropped becomes visible in a test
    /// rather than as a mysterious disconnect.
    pub fn refused(&self) -> u64 {
        self.counters.refused.load(Ordering::SeqCst)
    }

    /// The refusal threshold this fake applies.
    pub fn max_input_bytes(&self) -> usize {
        self.max_input_bytes
    }

    /// Every admitted request payload, in arrival order.
    pub fn seen(&self) -> Vec<Value> {
        self.seen
            .lock()
            .expect("the request log is not poisoned")
            .clone()
    }
}

/// Serves one connection the way the server loop does.
async fn serve(
    mut stream: UnixStream,
    handshake: Value,
    reply: Arc<ReplyFn>,
    connection: u64,
    max_input_bytes: usize,
    counters: Arc<Counters>,
    seen: Arc<Mutex<Vec<Value>>>,
) {
    if write_frame(
        &mut stream,
        &serde_json::to_vec(&handshake).expect("a JSON handshake"),
    )
    .await
    .is_err()
    {
        return;
    }

    let mut index = 0u64;
    loop {
        // The frame reader mirrors the server's: an announced length over the
        // cap, a zero-length frame and a short read all close the connection
        // instead of being answered.
        let Ok(frame) = read_frame(&mut stream).await else {
            return;
        };
        let request: Value = match serde_json::from_slice(&frame) {
            Ok(value) => value,
            Err(_) => {
                counters.refused.fetch_add(1, Ordering::SeqCst);
                return;
            }
        };
        // The two fields are taken as owned strings so that the parsed request
        // can be handed to the script — the reply closure reads the body.
        let (Some(id), Some(body)) = (
            request.get("id").and_then(Value::as_str).map(str::to_owned),
            request
                .get("body")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ) else {
            counters.refused.fetch_add(1, Ordering::SeqCst);
            return;
        };
        // The identifier is admitted by alphabet and length before anything
        // else: a refused one is never echoed, and never reaches the model.
        if !is_valid_id(&id) {
            counters.refused.fetch_add(1, Ordering::SeqCst);
            return;
        }
        index += 1;
        counters.requests.fetch_add(1, Ordering::SeqCst);
        seen.lock()
            .expect("the request log is not poisoned")
            .push(request.clone());

        // The size refusal happens before the model is touched. The script is
        // not consulted: a conformant sidecar answers `input_too_large` for an
        // oversized body whatever the script would have said.
        if body.len() > max_input_bytes {
            let resp = json!({"id": id, "spans": [], "status": "input_too_large"});
            if write_frame(
                &mut stream,
                &serde_json::to_vec(&resp).expect("a JSON reply"),
            )
            .await
            .is_err()
            {
                return;
            }
            continue;
        }

        match reply(connection, index, request) {
            Reply::Spans(spans) => {
                let resp = json!({"id": id, "spans": spans, "status": "ok"});
                if write_frame(
                    &mut stream,
                    &serde_json::to_vec(&resp).expect("a JSON reply"),
                )
                .await
                .is_err()
                {
                    return;
                }
            }
            Reply::Json(value) => {
                let body = serde_json::to_vec(&value).expect("a JSON reply");
                if write_frame(&mut stream, &body).await.is_err() {
                    return;
                }
            }
            Reply::RawBody(body) => {
                if write_frame(&mut stream, &body).await.is_err() {
                    return;
                }
            }
            Reply::DeclaredLength(len) => {
                let _ = stream.write_all(&(len as u32).to_be_bytes()).await;
                let _ = stream.flush().await;
                return;
            }
            Reply::Close => return,
            Reply::Slow(delay) => {
                tokio::time::sleep(delay).await;
                let resp = json!({"id": id, "spans": [], "status": "ok"});
                if write_frame(
                    &mut stream,
                    &serde_json::to_vec(&resp).expect("a JSON reply"),
                )
                .await
                .is_err()
                {
                    return;
                }
            }
            Reply::Silent => {
                tokio::time::sleep(Duration::from_secs(3_600)).await;
                return;
            }
        }
    }
}

async fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).await?;
    let len = u32::from_be_bytes(prefix) as u64;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(std::io::Error::other("frame refused"));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

async fn write_frame(stream: &mut UnixStream, body: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}
