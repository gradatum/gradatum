# privacy-sidecar

An out-of-process PII detector. It loads a token-classification model once and
answers, over a Unix socket, with the *positions* of the personal data it finds —
never with the values themselves.

**What leaves this process is a category and a byte range.** The upstream model
also returns the matched text; that field is never copied out of the model
result, never serialised into a response and never written to a log. An
operator reading the socket log, or a client reading a response, learns that an
address span exists at offsets 47..59 — not what is written there. The log is
the audit surface, so a client-supplied identifier is admitted by alphabet and
length before it is allowed anywhere near it.

The sidecar is a separate process for a reason: the detector is a C/C++ GGML
binary with its own memory profile, and the server must be able to cap its
memory, confine its network access and kill it without losing the vault.

## Layout

| Path | Role |
|---|---|
| `CMakeLists.txt` | Builds the upstream engine, then `pf-sidecar` (and `pf-ref-cli`, the span comparator) |
| `src/` | The sidecar — protocol, engine wrapper, server loop, self-contained SHA-256 |
| `fetch-upstream.sh` | Acquires the two third-party inputs at pinned revisions, outside the repository |
| `tests/run.sh` | End-to-end harness — starts a real sidecar against a real model, drives it, checks the log |
| `tests/client.py` | Python 3 stdlib protocol client used by the harness |
| `tests/fixtures.txt` | Three fictitious French bodies (email, IP, phone) |
| `tests/dump-labels.sh` | Derives the model's entity groups — the provenance of the client's label fixture |

Nothing third-party is vendored here. No binary and no model weight is
committed: the GGUF is an operator-supplied input, and `.gitignore` keeps the
build tree, the upstream checkout and the weights out of the index.

## Build

The two third-party inputs are fetched at pinned revisions into a directory
**outside** the repository — `$PF_SIDECAR_BUILD_DIR`, or
`$HOME/.cache/gradatum-privacy-sidecar` by default. The script verifies every
revision and every digest it pins, and exits non-zero on any divergence.

```sh
bash tools/privacy-sidecar/fetch-upstream.sh
```

It prints the exact configuration commands to paste:

```sh
cmake -S tools/privacy-sidecar -B "$BUILD_DIR/build" \
      -DPF_UPSTREAM_DIR="$BUILD_DIR/third_party/privacy-filter.cpp" \
      -DPF_JSON_INCLUDE_DIR="$BUILD_DIR/third_party/include"
cmake --build "$BUILD_DIR/build" --target pf-sidecar pf-ref-cli
```

On a memory-capped host, run the build under a scope that bounds it, and run one
build at a time.

### Pinned inputs

| Input | Pin |
|---|---|
| `privacy-filter.cpp` | commit `735a6c28607ee82afc3a670383f41b55266a3b9a` (MIT) |
| `ggml` submodule | commit `3af5f5760e19a96427f5f7a93b79cbdf3d4b265b` |
| nlohmann/json | v3.11.3, verified by content digest (`sha256:9bea4c80…3ea6`) |

Upstream carries no header-only JSON library, so one is fetched rather than a
system package being assumed. Changing any pin changes the digests the sidecar
reports and the spans it emits — it is a deliberate act, not a refresh.

## Run

```sh
pf-sidecar --socket /run/gradatum-privacy/privacy-sidecar.sock \
           --model /var/lib/gradatum-privacy/model.gguf \
           --threads 4 \
           --max-input-bytes 262144 \
           --expect-binary-sha256 <hex> \
           --expect-model-sha256 <hex>
```

| Option | Meaning |
|---|---|
| `--socket <path>` | AF_UNIX path to bind. Refuses to bind if the path already exists — a leftover path needs an operator, not a blind overwrite. |
| `--model <gguf>` | Weights. Loaded once, before the socket is created. |
| `--threads <n>` | Threads handed to the engine (default and deployed value: `4` — it leaves the rest of the host to the server, the worker and the build). |
| `--max-input-bytes <n>` | Refusal threshold for one body (default 262144). |
| `--expect-binary-sha256 <hex>` | Refuse to start if the running image does not match. |
| `--expect-model-sha256 <hex>` | Refuse to start if the weights do not match. |
| `--selfcheck` | Print the digests and exit, without binding a socket. |
| `--device <name>` | Device handed to the engine (default `cpu`). |

At startup the process computes its own digest from `/proc/self/exe` — the
running image, not a path that may have been replaced — and the model's digest.
A pinned digest that diverges stops the process; it never warns and serves on.
The process does not create the directory it binds into: that directory belongs
to the unit that runs it (`RuntimeDirectory=`), and a path whose parent does not
exist fails at `bind`. The node itself is created under a restrictive umask and
then widened to exactly `0660` — owned by the sidecar account and the server's
group, so the server reaches it and nobody else does. Start-up refuses on an
existing path, and a clean shutdown removes the node it created.

### Protocol

One client at a time; further connections wait in the listen backlog. Frames are
a `u32` big-endian byte length followed by that many bytes of UTF-8 JSON. A frame
above 16 MiB closes the connection rather than being answered.

On connect the sidecar sends its handshake:

```json
{"binary_sha256": "…", "model_sha256": "…", "mapping_v": 0,
 "labels_v": "sha256:…", "score_mode": "mean_token_prob", "ready": true}
```

`labels_v` is a digest over the model's label vocabulary: two models sharing
weights but disagreeing on labels are told apart by it. `mapping_v` is the
label-mapping revision *this* process applies — `0` means it maps nothing and
reports the model's own labels; the mapping belongs to the caller, where it can
be versioned and reviewed alongside the categories it feeds.

A request is `{"id": "<identifier>", "body": "<text>"}` and the response is
`{"id": …, "spans": [{"label": …, "start": …, "end": …, "score": …}], "status": …}`
with `status` either `ok` or `input_too_large`. A body over the cap is refused by
a length comparison, before the model is touched — the refusal is cheap and the
model never sees the text. A malformed request, a refused identifier or an
over-nested document closes the connection instead of inventing a status outside
the documented set. A body that a caller must window is the caller's problem:
the sidecar answers about the text it was given, and merging windows is done
where the offsets are known to mean something.

### Logs

`stderr`, one line per event: `sidecar t_ms=<n> ev=<event> <k=v>…`. Field values
are identifiers, byte lengths, counts, status codes and durations. A request
body, a span value and a refused identifier never appear — the identifier is
logged as a length (`id_bytes=58`). This is what makes the log safe to keep and
worth keeping.

## Verify

```sh
PF_MODEL=/path/to/model.gguf bash tools/privacy-sidecar/tests/run.sh
```

The harness checks the digests the binary reports against the ones the host
computes, asserts that a *divergent* pin is refused and a matching one is
accepted, starts the sidecar, asserts the socket mode is `660`, drives the
protocol, compares every span triple against `pf-cli` on the same text, checks
that the oversized body is refused without a `classify` event, and greps the log
for the fixture values. Exit codes: `0` all checks hold, `1` a check failed,
`2` the harness could not run because an artefact is missing — which is not a
pass.

| Variable | Default |
|---|---|
| `PF_MODEL` | required — path to the GGUF weights |
| `PF_SIDECAR_BUILD_DIR` | `$HOME/.cache/gradatum-privacy-sidecar` |
| `PF_REF_CLI` | the comparator built above; override it to compare against any other `pf-cli` exposing text mode |
| `PF_SOCKET_DIR` | a fresh temporary directory (never `/run`) |
| `PF_MAX_INPUT_BYTES` | `262144` |
| `PF_TIMEOUT_S` | `300` |

Deployment — the confined systemd unit, the socket ownership model and the
memory caps — is documented in [`docs/PRIVACY-SIDECAR.md`](../../docs/PRIVACY-SIDECAR.md).

## Licence

The sidecar's own source is covered by the repository licence. It links against
two third-party works that are fetched, not vendored: `privacy-filter.cpp` (MIT)
and `nlohmann/json` (MIT). See [`LICENSE-NOTICE`](LICENSE-NOTICE).
