#!/usr/bin/env python3
"""Protocol-level client for the privacy sidecar (stdlib only).

Speaks the documented wire protocol against a running ``pf-sidecar``:
length-prefixed (u32 big-endian) JSON frames on a Unix stream socket. It checks
the handshake, classifies three fictitious bodies, checks the oversize path,
cross-checks every span against the reference comparator built from the same
pinned upstream revision, and verifies that no body byte reached the log.

Usage:
    python3 client.py --socket PATH --model PATH.gguf --ref-cli PATH \\
        --log PATH [--max-input-bytes N]

Exit code 0 when every check holds, 1 otherwise (each failure is printed).
"""

import argparse
import hashlib
import json
import os
import socket
import struct
import subprocess
import sys

MAX_FRAME = 16 * 1024 * 1024
HANDSHAKE_KEYS = {
    "binary_sha256",
    "model_sha256",
    "mapping_v",
    "labels_v",
    "score_mode",
    "ready",
}
SPAN_KEYS = {"label", "start", "end", "score"}
# The sidecar fixes the emission threshold; the comparator must use the same one.
THRESHOLD = "0.0"

# Distinctive substrings of the fictitious fixtures. None of them may appear in
# the sidecar log: a span value or a body byte in a log line is a leak.
LEAK_NEEDLES = [
    "alice.martin.fictif@example.invalid",
    "203.0.113.42",
    "01 23 45 67 89",
]

FAILURES = []


def check(cond, msg):
    if cond:
        return True
    FAILURES.append(msg)
    print("FAIL: %s" % msg)
    return False


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def send_frame(sock, payload):
    sock.sendall(struct.pack(">I", len(payload)) + payload)


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def recv_frame(sock):
    """Return (raw_bytes, obj) or (None, None) on a clean EOF."""
    head = recv_exact(sock, 4)
    if head is None:
        return None, None
    (length,) = struct.unpack(">I", head)
    if length > MAX_FRAME:
        raise RuntimeError("server sent an oversized frame: %d" % length)
    body = recv_exact(sock, length)
    if body is None:
        return None, None
    return body, json.loads(body.decode("utf-8"))


def read_fixtures(path):
    with open(path, "r", encoding="utf-8") as fh:
        bodies = [line.rstrip("\n") for line in fh if line.strip()]
    if len(bodies) != 3:
        raise RuntimeError("expected 3 fixture bodies, got %d" % len(bodies))
    return bodies


def ref_cli_spans(ref_cli, model, body):
    """Spans from the reference comparator, as (label, start, end) triples."""
    proc = subprocess.run(
        [ref_cli, "--classify", model, THRESHOLD],
        input=body.encode("utf-8"),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            "reference comparator failed (rc=%d): %s"
            % (proc.returncode, proc.stderr.decode("utf-8", "replace")[:400])
        )
    out = proc.stdout.decode("utf-8")
    # The comparator refuses over-long inputs by emitting nothing usable; a
    # non-JSON prefix is a comparator contract break, not a sidecar finding.
    start = out.find("[")
    if start < 0:
        raise RuntimeError("reference comparator emitted no JSON array: %r" % out[:200])
    ents = json.loads(out[start:])
    return [(e["entity_group"], e["start"], e["end"]) for e in ents], ents


def one_connection(args, bodies, oversize):
    """Run every check that needs a live connection. Returns True when all hold."""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(300)
    sock.connect(args.socket)
    ok = True

    raw, hs = recv_frame(sock)
    ok &= check(isinstance(hs, dict), "handshake is not a JSON object")
    if not isinstance(hs, dict):
        sock.close()
        return False
    ok &= check(
        set(hs) == HANDSHAKE_KEYS,
        "handshake keys are %s, expected %s" % (sorted(hs), sorted(HANDSHAKE_KEYS)),
    )
    ok &= check(hs.get("ready") is True, "handshake ready is not true")
    ok &= check(hs.get("mapping_v") == 0, "handshake mapping_v is not 0")
    ok &= check(
        isinstance(hs.get("score_mode"), str) and hs["score_mode"],
        "handshake score_mode is empty",
    )
    labels_v = hs.get("labels_v") or ""
    ok &= check(
        labels_v.startswith("sha256:") and len(labels_v) == len("sha256:") + 64,
        "handshake labels_v is not a sha256-tagged digest: %r" % labels_v,
    )
    # The digests are the operator's only handle on "is this really the artefact
    # I pinned"; check them against the files on disk, not against themselves.
    ok &= check(
        hs.get("binary_sha256") == sha256_file(args.sidecar_bin),
        "handshake binary_sha256 does not match the binary on disk",
    )
    ok &= check(
        hs.get("model_sha256") == sha256_file(args.model),
        "handshake model_sha256 does not match the model on disk",
    )

    for idx, body in enumerate(bodies):
        rid = "fix-%d" % idx
        send_frame(sock, json.dumps({"id": rid, "body": body}).encode("utf-8"))
        raw, resp = recv_frame(sock)
        if not check(isinstance(resp, dict), "response %s is not a JSON object" % rid):
            ok = False
            continue
        ok &= check(resp.get("id") == rid, "response id mismatch for %s" % rid)
        ok &= check(resp.get("status") == "ok", "response status for %s: %r" % (rid, resp.get("status")))
        ok &= check(b'"text"' not in raw, "response for %s carries a text field" % rid)
        spans = resp.get("spans")
        ok &= check(isinstance(spans, list) and len(spans) > 0, "no span for %s" % rid)
        if not isinstance(spans, list):
            continue
        nbytes = len(body.encode("utf-8"))
        for sp in spans:
            ok &= check(set(sp) == SPAN_KEYS, "span keys are %s" % sorted(sp))
            ok &= check(
                isinstance(sp.get("start"), int) and 0 <= sp["start"] < sp["end"] <= nbytes,
                "span offsets out of range for %s: %r" % (rid, sp),
            )
            ok &= check(
                isinstance(sp.get("score"), (int, float)) and 0.0 <= sp["score"] <= 1.0,
                "span score out of range for %s: %r" % (rid, sp),
            )
        # Same pinned revision, same threshold, same text: the spans must agree
        # with the reference comparator, offsets and labels included.
        want, ents = ref_cli_spans(args.ref_cli, args.model, body)
        got = [(sp["label"], sp["start"], sp["end"]) for sp in spans]
        ok &= check(
            got == want,
            "span triples differ for %s:\n  sidecar=%r\n  ref-cli=%r" % (rid, got, want),
        )
        if got == want:
            for sp, ent in zip(spans, ents):
                ok &= check(
                    abs(float(sp["score"]) - float(ent["score"])) <= 1e-3,
                    "score differs for %s at %s: %r vs %r"
                    % (rid, sp["label"], sp["score"], ent["score"]),
                )

    # A body one byte over the cap must be refused without invoking the model.
    rid = "oversize"
    send_frame(sock, json.dumps({"id": rid, "body": oversize}).encode("utf-8"))
    raw, resp = recv_frame(sock)
    if check(isinstance(resp, dict), "oversize response is not a JSON object"):
        ok &= check(resp.get("id") == rid, "oversize response id mismatch")
        ok &= check(
            resp.get("status") == "input_too_large",
            "oversize status is %r, expected input_too_large" % resp.get("status"),
        )
        ok &= check(resp.get("spans") == [], "oversize response carries spans")
        ok &= check(b'"text"' not in raw, "oversize response carries a text field")

    # A malformed request is a protocol error: the sidecar closes, it does not
    # invent a status outside the documented set.
    send_frame(sock, b'{"id": "broken", ')
    closed = recv_exact(sock, 1) is None
    ok &= check(closed, "malformed request did not close the connection")
    sock.close()
    return bool(ok)


def refused_connection(args, label, payload):
    """Send one request expected to be refused, on its own connection: a refusal
    closes the socket, so it cannot share the connection under test."""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(300)
    sock.connect(args.socket)
    recv_frame(sock)  # handshake
    send_frame(sock, payload)
    closed = recv_exact(sock, 1) is None
    ok = check(closed, "%s was not refused" % label)
    sock.close()
    return bool(ok)


def injection_connection(args):
    """An `id` must not be able to forge a log line, and a document must not be
    able to nest its way through the parser's stack."""
    forged = "inj\nsidecar t_ms=0 ev=classify id=oversize in_bytes=999999"
    ok = refused_connection(
        args, "an identifier carrying a newline",
        json.dumps({"id": forged, "body": "court"}).encode("utf-8"),
    )
    ok &= refused_connection(
        args, "a document nested past the depth cap",
        (b"[" * 4096) + b"1" + (b"]" * 4096),
    )
    return bool(ok)


def check_log(args, out_dir):
    """0 body byte and 0 span value in the log; and the log is not empty."""
    ok = True
    log_path = args.log
    if not check(os.path.exists(log_path), "sidecar log %s not found" % log_path):
        return False
    text = open(log_path, "r", encoding="utf-8", errors="replace").read()
    ok &= check(len(text) > 0, "sidecar log is empty")
    # Non-vacuity: the absence checks below only mean something if the events
    # that would carry a leak actually fired.
    for event in ("listen", "handshake", "classify", "reject"):
        ok &= check("ev=%s" % event in text, "log carries no ev=%s line" % event)
    for needle in LEAK_NEEDLES:
        ok &= check(needle not in text, "log leaks a fixture value: %r" % needle)
    for body in read_fixtures(args.fixtures):
        ok &= check(body not in text, "log leaks a full fixture body")
    # The log is the audit surface: a client-supplied identifier must not be able
    # to forge an event line inside it. The marker below exists only in the
    # injection request, so its presence means the identifier reached the log
    # verbatim.
    ok &= check("in_bytes=999999" not in text, "log carries a client-forged event line")
    # Each refusal above must be attributable to the gate that produced it, and a
    # refused identifier must be reported as a length only.
    for reason in ("bad_id", "json_too_deep"):
        lines = [ln for ln in text.splitlines() if "ev=reject" in ln and "reason=%s" % reason in ln]
        ok &= check(
            len(lines) == 1,
            "expected exactly 1 reject line with reason=%s, got %d" % (reason, len(lines)),
        )
    for line in text.splitlines():
        if "reason=bad_id" in line:
            ok &= check("id_bytes=" in line, "a bad_id rejection does not report a length")
            ok &= check("id=inj" not in line, "a bad_id rejection echoes the refused identifier")
    # The oversize request must be visible as a refusal and absent from the
    # classify events: that is how "the model was not called" is observed.
    rej = [ln for ln in text.splitlines() if "ev=reject" in ln and "id=oversize" in ln]
    ok &= check(len(rej) == 1, "expected exactly 1 reject line for the oversize id, got %d" % len(rej))
    if len(rej) == 1:
        ok &= check("status=input_too_large" in rej[0], "reject line lacks the status code")
        ok &= check(
            "in_bytes=%d" % (args.max_input_bytes + 1) in rej[0],
            "reject line lacks the observed input length: %s" % rej[0],
        )
    cls = [ln for ln in text.splitlines() if "ev=classify" in ln and "id=oversize" in ln]
    ok &= check(cls == [], "the model was invoked on the oversize body: %r" % cls)
    return bool(ok)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--ref-cli", required=True, dest="ref_cli")
    ap.add_argument("--sidecar-bin", required=True, dest="sidecar_bin")
    ap.add_argument("--log", required=True)
    ap.add_argument("--fixtures", required=True)
    ap.add_argument("--max-input-bytes", type=int, default=262144, dest="max_input_bytes")
    ap.add_argument("--out-dir", default=".", dest="out_dir")
    args = ap.parse_args()

    bodies = read_fixtures(args.fixtures)
    oversize = "A" * (args.max_input_bytes + 1)

    rc = 0
    if not one_connection(args, bodies, oversize):
        rc = 1
    if not injection_connection(args):
        rc = 1
    if not check_log(args, args.out_dir):
        rc = 1
    if FAILURES:
        print("client: %d check(s) failed" % len(FAILURES))
        rc = 1
    else:
        print("client: all checks passed")
    return rc


if __name__ == "__main__":
    sys.exit(main())
