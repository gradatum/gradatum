# Privacy sidecar — deployment

The privacy sidecar is an out-of-process PII detector: it loads a
token-classification model once and answers over a Unix socket with the
*positions* of personal data, never with the values. Build, protocol and
verification are documented in
[`tools/privacy-sidecar/README.md`](../tools/privacy-sidecar/README.md); this
page covers running it as a confined service.

## Why a separate process

The detector is a C/C++ GGML binary with its own memory profile, and it is the
only component in the stack that reads the raw text of a note. Running it as its
own unit means it can be given a hard memory ceiling, denied network access
entirely, and restarted or stopped without touching the vault. A crash in the
detector costs detections; it does not cost the process that owns the data.

## Threat model, briefly

| Concern | How it is handled |
|---|---|
| The detector is the component that *sees* the plaintext | It returns offsets and categories only. The matched text is never copied out of the model result, so a client reading the socket learns where an address sits, not what it says. |
| The log becomes a second copy of the data | The log carries identifiers, byte lengths, counts, status codes and durations — never a body, a span value or a refused identifier. |
| A leaked socket means leaked text | The socket is `0660`, owned by the sidecar account and the server's group; admission re-checks the peer credentials with `SO_PEERCRED` and fails closed. |
| Admission and the file mode draw different lines | `0660` is checked against **all** of the connecting process's groups; `SO_PEERCRED` reports a single uid and a single — the **primary** — group, never the supplementary ones. So a peer that opens the node on a secondary membership is refused by the application, and every peer admission accepts has the shared group as its primary group. The unit below makes that true of itself with `Group=`; the server's unit has to do the same. |
| An unvetted binary or weights serve queries | The process hashes its own running image (`/proc/self/exe`) and the model at startup, refuses to start on a mismatch with what the operator pinned, and reports both digests in the handshake. |
| A hostile or broken client parks the only service slot | One client is served at a time, with I/O deadlines on the connection: a peer that connects and stalls is dropped instead of holding the slot. |
| The detector reaches the network | `PrivateNetwork=yes` and `RestrictAddressFamilies=AF_UNIX`: it has no network surface to reach with. |

## Prerequisites

- The binary built and installed under `libexec` by the project's install step
  (`install(... RUNTIME DESTINATION ${CMAKE_INSTALL_LIBEXECDIR}/gradatum-privacy)`).
- A **fixed** service account `gradatum-privacy`. Not `DynamicUser=`: the socket
  is shared with the server through a **stable group**, which a volatile uid/gid
  cannot provide, and a fixed account is auditable.
- A group both units run in — `gradatum` here; substitute whatever your
  deployment uses. The node it creates is `gradatum-privacy:gradatum`, so the
  server reaches it through the group it already belongs to and no second group
  has to exist. Make it the **primary** group of both units, this one and the
  server's, through `Group=`: the file mode is evaluated against every group of
  the connecting process, but admission compares the peer's primary group only,
  so a shared group held as a secondary membership would be refused — see
  *Verifying a running deployment*.
- The weights placed under the service's state directory. Their ownership is
  not the install step's to fix: `StateDirectory=` sets it on the first start —
  see *Installing the weights*.

## Installing the weights

```sh
install -d -m 0750 /var/lib/gradatum-privacy
install -m 0440 privacy-filter-multilingual-q8.gguf \
        /var/lib/gradatum-privacy/model.gguf
sha256sum /var/lib/gradatum-privacy/model.gguf
```

Record the digest: it is what the unit pins below. The same value is reported in
the handshake and by `--selfcheck`, so the artefact in use is identifiable from
the running process rather than from a deployment note.

Nothing here needs a hand-written `chown`, and a hand-written one does not last:
`StateDirectory=gradatum-privacy` in the unit below declares the state
directory, and systemd brings its ownership in line with the unit's `User=` and
`Group=` at start-up — for this unit, `gradatum-privacy:gradatum`. When the
directory already exists with another owner, the change is recursive: the
directory and everything below it are re-owned. The directory's own mode is set
to `StateDirectoryMode=` (`0750` here, the mode the install step already
writes); the modes of the files below it are left as they are. An
`install … -o gradatum-privacy -g gradatum-privacy` typed out before the first
start therefore describes a state that lasts only until that start.

What the operator observes after the first start, whatever ownership the install
step left behind:

```sh
stat -c '%a %U:%G' /var/lib/gradatum-privacy/model.gguf
# 440 gradatum-privacy:gradatum
```

That is the mechanism that makes the weights readable — the process runs as
`gradatum-privacy` and the file belongs to that account — and not a permission
the install step granted. The group in the second field is the one the unit runs
in, so the weights are readable by the server's group as well; that widens
nothing the sidecar is there to protect, since the weights are an artefact the
operator installs, and the plaintext never enters this directory: the process
reads one file and one socket, and writes nothing here.

The read the deployment actually depends on can be confirmed from the sidecar's
own account rather than from root:

```sh
runuser -u gradatum-privacy -- head -c 4 /var/lib/gradatum-privacy/model.gguf
```

## Reference unit

```ini
[Unit]
Description=Gradatum privacy sidecar (out-of-process PII detection)
Documentation=file:/usr/share/doc/gradatum/PRIVACY-SIDECAR.md
After=local-fs.target
Before=gradatum-server.service

[Service]
Type=simple
User=gradatum-privacy
Group=gradatum

# The socket lives in a runtime directory of this unit's own, traversable by the
# server's group so the server can reach the node without gaining anything else.
RuntimeDirectory=gradatum-privacy
RuntimeDirectoryMode=0750
StateDirectory=gradatum-privacy
StateDirectoryMode=0750

ExecStart=/usr/libexec/gradatum-privacy/pf-sidecar \
          --socket /run/gradatum-privacy/privacy-sidecar.sock \
          --model /var/lib/gradatum-privacy/model.gguf \
          --threads 4 \
          --max-input-bytes 262144 \
          --expect-binary-sha256 0000000000000000000000000000000000000000000000000000000000000000 \
          --expect-model-sha256 0000000000000000000000000000000000000000000000000000000000000000

Restart=on-failure
RestartSec=5s

# Confinement: no network surface at all, no writable system paths, no home,
# no new privileges, a private /tmp. The process reads one file and one socket.
PrivateNetwork=yes
RestrictAddressFamilies=AF_UNIX
ProtectSystem=strict
ProtectHome=yes
NoNewPrivileges=yes
PrivateTmp=yes
ProtectKernelTunables=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=no

# Sizing: the detector holds the model in memory and does not shrink. Cap it
# hard, throttle before the kill so a burst is felt as latency, not as a
# restart. Four threads: leave the rest of the host to the server and the worker.
MemoryHigh=2304M
MemoryMax=2560M
CPUQuota=400%
Nice=5

StandardOutput=journal
StandardError=journal
SyslogIdentifier=gradatum-privacy-sidecar

[Install]
WantedBy=multi-user.target
```

Replace both digest placeholders with the values recorded above — the unit as
written will not start, which is the intended failure: an unpinned detector is
not a detector anyone can reason about.

### Why the runtime directory is this unit's own

`RuntimeDirectory=` is a declaration of ownership, not a shared scratch space.
Every unit that names a directory creates it if absent and re-chowns it to its
own `User=` and `Group=` on each start, so a sidecar declaring
`RuntimeDirectory=gradatum` would take `/run/gradatum` from the server and the
worker — the next restart of either would take it back, and whichever unit was
slower to start would find its socket directory owned by a process that did not
create it. Giving the sidecar `gradatum-privacy` keeps the two ownerships from
ever meeting: the server's `/run/gradatum` stays untouched by this unit, and this
unit's `/run/gradatum-privacy` stays outside the server's reach — which still
reaches the node inside it, because both processes run in the group `gradatum`.

`MemoryDenyWriteExecute=no` is deliberate. It is the one hardening switch this
unit does *not* take: the inference kernels may map executable pages at run time,
and denying that turns a working detector into a process that dies at first
inference. The remaining switches are what actually bound the service.

`SocketMode` does not appear above because this is a plain `Type=simple` service
that creates its own socket; the sidecar sets the node to `0660` itself. If you
convert this to socket activation, the corresponding settings are a `.socket`
unit with `SocketMode=0660` and `SocketGroup=gradatum`, with
`RuntimeDirectory=gradatum-privacy` (`0750`) carried by that same `.socket` unit
— the unit that owns a directory is the one that re-chowns it — and the service
must be started with the inherited descriptor rather than `--socket`.

### Why `0660` and not `0600`

`0600` is impossible between two accounts: a socket owned by the service and
readable by nobody else cannot be opened by the server, which is a different
user. The choice is between "world-accessible" and the server's group, and that
group keeps everything that matters — the node is `gradatum-privacy:gradatum`, so
no one outside the group has any access to it, admission is re-checked per
connection against the peer's uid and its primary group (which is why the
server's unit names this group as its own `Group=` rather than relying on a
secondary membership), and there is no network surface. A `0600`
node would only be reachable if both processes ran as the same user, which
defeats the point of isolating the detector.

## Server configuration

The server reaches the sidecar through a block of its own, added by hand next to
the rest of `[privacy]`:

```toml
[privacy]
categories = ["email"]        # the block alone mounts nothing: this is the activation

[privacy.sidecar]
socket_path = "/run/gradatum-privacy/privacy-sidecar.sock"
binary_digest = "…"           # the `binary_sha256` the process reports, 64 hex characters
model_digest = "…"            # the `model_sha256` of the weights it loads
```

`binary_digest` and `model_digest` are read off the artefacts and compared to
what the handshake reports by exact string equality: bare lowercase hexadecimal,
no `sha256:` tag. Both are **required**, and a value of the wrong length, in
another case, or with a non-hexadecimal character is refused when the
configuration is read — naming the field, never the value. The same goes for an
empty `socket_path`.

The block must be present **and** `categories` non-empty for the sidecar to be
mounted. A block over an empty activation is admitted and mounts nothing: it
names a detector no write would ask for. Without the block, a non-empty
activation mounts the in-process detector exactly as before.

**Startup refuses a sidecar that does not answer.** Mounting opens the socket and
takes the handshake before the process begins serving; an unreachable socket, a
timeout, or a handshake announcing a different binary, model or score mode makes
the server **fail to start**, with an error naming the cause. There is no
fallback to the in-process detector, by design: a deployment that wrote the block
down asked for a model that runs outside the server, and a quiet substitution
would leave it scanning with the engine it did not choose — the only trace being
a detector name in a health report nobody had a reason to read. The repair is to
start the sidecar, or to remove the block.

The two byte budgets stay in `[privacy]` (`max_input_bytes`,
`window_overlap_bytes`) and are handed to the client by the server. Repeating
them here would make two keys authoritative for one ceiling, and a body could be
cut into windows neither value describes. The remaining keys —
`score_mode`, `timeout_ms_sync_per_kib`, `breaker_n`, `breaker_cooldown_s` —
default to the values the client itself defines, and are set only when a
deployment has a reason to.

The activation is not moved by this block: the vault still mounts the policy it
is handed, applies the regex floor under the model, compares the **fused** score
to `[privacy.thresholds]`, and drops what `[privacy.exceptions]` covers, last.
Switching the engine changes who classifies, not where the decision is taken.
One sidecar serves the whole deployment: the socket, the announced handshake and
the circuit breaker live in the mounted detector, shared by every vault of the
process.

## Verifying a running deployment

```sh
systemctl status gradatum-privacy-sidecar
systemctl show -p MemoryMax -p MemoryHigh -p CPUQuotaPerSecUSec gradatum-privacy-sidecar
stat -c '%a %U:%G' /run/gradatum-privacy/privacy-sidecar.sock
```

Two admission checks belong in this pass, because the file mode and the
application check do not draw the same line:

- **A uid outside the group is refused by the mode.** A connection from an
  account that is neither the sidecar account nor a member of the shared group
  never reaches the process: the kernel refuses it on the `0660` node, and the
  journal stays silent — the refusal happened before the socket was handed over.
- **A secondary member of the group is refused by admission.** A process whose
  only claim to the shared group is a secondary membership passes the mode — that
  check covers every group the process has — and is then rejected before any
  handshake: the client sees the connection close with no handshake frame, and
  the journal carries `ev=reject reason=peer_not_admitted detail=uid=…,gid=…`,
  with, as for every rejection, no body, no span and no identifier. This is the
  check that separates a working deployment from one that merely looks like one:
  the server is admitted because the shared group is the *primary* group of its
  unit, so an account that reaches the node and is refused here holds the group
  as a secondary membership only.

```sh
journalctl -u gradatum-privacy-sidecar -g peer_not_admitted
```

The digests the process reports can be read without serving anything:

```sh
runuser -u gradatum-privacy -- /usr/libexec/gradatum-privacy/pf-sidecar \
    --selfcheck --model /var/lib/gradatum-privacy/model.gguf
```

`--selfcheck` prints `binary_sha256`, `model_sha256`, `labels_v`,
`labels_count`, `mapping_v`, `score_mode`, the frame and input caps and the
thread count, then exits without binding a socket. `labels_v` is a digest over
the model's label vocabulary: if the weights are swapped for a model that
disagrees on labels, that value changes even when the file digest would not
have been checked. The server compares all of these against what its own
configuration expects before it treats a detection as trustworthy.

An end-to-end check against real weights runs the same way in production as it
does in development:

```sh
PF_MODEL=/var/lib/gradatum-privacy/model.gguf \
PF_SOCKET_DIR=/run/gradatum-check \
    bash tools/privacy-sidecar/tests/run.sh
```

Point `PF_SOCKET_DIR` at a path the invoking user may write; the harness never
touches `/run` by default, and it removes the socket it creates.

## Memory

The detector's resident set is dominated by the model and settles once loaded:
expect roughly 1.6 GiB for the multilingual q8 weights, which is why the cap
above sits at 2560M — high enough to load and serve, low enough that a runaway
child is contained before it pressures the host. Consolidate with the
measurements on your own host rather than trusting this number: run the harness
and read the peak it reports (`peak_rss_kb=`), which is the kernel's own
high-water mark for the process.

## Not covered

Paths and filenames. The model carries no category for them and no regex engine
covers them here, so a filesystem path in a note is not detected as personal
data. This is a known gap, stated rather than papered over with speculative
regexes: a path detector built on pattern matching alone would trade a real
false-negative for a large volume of false positives, and the result would be
distrusted and switched off.
