#!/usr/bin/env bash
#
# dump-labels.sh — the entity groups of a privacy-filter model, one per line.
#
#   PF_CLI_BIN=/path/to/pf-cli PF_MODEL_PATH=/path/to/model.gguf \
#     bash tools/privacy-sidecar/tests/dump-labels.sh
#
# WHY THIS SCRIPT EXISTS
#   The client (`crates/gradatum-privacy`) checks its label-to-category table
#   against a fixture of the model's entity groups
#   (`crates/gradatum-privacy/tests/fixtures/model-labels.txt`). A fixture nobody
#   can reproduce drifts: the table is then checked against a list that no longer
#   describes the model, and the check goes green while the mapping is wrong. This
#   script is the provenance — run it, compare its output to the fixture, and the
#   fixture is the model's vocabulary or visibly is not.
#
# WHAT IT READS, AND WHY `--info` IS THE CROSS-CHECK AND NOT THE SOURCE
#   The groups are the labels of the model's own vocabulary with the BIOES tag
#   (`B-`, `I-`, `E-`, `S-`) stripped, minus the outside label `O`. That vocabulary
#   is the GGUF metadata field `*.classifier.output_labels` — the very field
#   `pf-cli --info` reads. But `--info` prints only the COUNT of labels and the two
#   boundaries (`labels 217 (O ... S-ZIPCODE)`); it does not print the list. What
#   `--info` does print is therefore asserted against the array read here: the
#   count and the two boundaries must agree. A truncated array, or another field
#   taken for this one, does not survive that comparison.
#
#   Reading the list needs a GGUF parser; `python3` (standard library) is the only
#   dependency of this provenance tool, and nothing else is required.
#
# EXIT CODES
#   0  the groups were derived and cross-checked by `pf-cli --info`
#   1  a check failed (a diverging count or boundary, a label carrying no tag)
#   2  the script could not run (an artefact or a tool is missing) — not a pass
set -euo pipefail

PF_CLI_BIN="${PF_CLI_BIN:-}"
PF_MODEL_PATH="${PF_MODEL_PATH:-}"

if [ -z "$PF_CLI_BIN" ] || [ ! -x "$PF_CLI_BIN" ]; then
  echo "PF_CLI_BIN must name an executable pf-cli" >&2
  exit 2
fi
if [ -z "$PF_MODEL_PATH" ] || [ ! -f "$PF_MODEL_PATH" ]; then
  echo "PF_MODEL_PATH must name the model weights (a .gguf file)" >&2
  exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 not found: the GGUF label field cannot be read" >&2
  exit 2
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
INFO="$TMP/info.txt"
RAW="$TMP/labels-raw.txt"

# --- What pf-cli announces about the vocabulary -----------------------------
if ! "$PF_CLI_BIN" --info "$PF_MODEL_PATH" >"$INFO" 2>&1; then
  echo "pf-cli --info failed on $PF_MODEL_PATH" >&2
  exit 1
fi
INFO_LINE="$(grep -E '^labels[[:space:]]+[0-9]+' "$INFO" | head -n 1 || true)"
if [ -z "$INFO_LINE" ]; then
  echo "pf-cli --info announced no label vocabulary: unexpected output" >&2
  exit 1
fi
INFO_COUNT="$(printf '%s\n' "$INFO_LINE" | sed -E 's/^labels[[:space:]]+([0-9]+).*/\1/')"
# `labels 217 (O ... S-ZIPCODE)`: the two boundaries are what make the count
# falsifiable — two models can carry 217 labels and share not one of them.
INFO_BOUNDS="$(printf '%s\n' "$INFO_LINE" | sed -E 's/^labels[[:space:]]+[0-9]+[[:space:]]+\((.*)\)[[:space:]]*$/\1/')"
INFO_FIRST="${INFO_BOUNDS%% ... *}"
INFO_LAST="${INFO_BOUNDS##* ... }"

# --- The vocabulary as the model carries it ---------------------------------
set +e
python3 - "$PF_MODEL_PATH" "$RAW" <<'PY'
"""Reads the label field out of a GGUF file's metadata.

The format is public and short: a header, then key/value pairs up to the tensors.
Parsing stops at the field this looks for — what follows carries the tokenizer,
whose arrays are long and of no interest here.
"""
import struct
import sys

SIZES = {0: 1, 1: 1, 2: 2, 3: 2, 4: 4, 5: 4, 6: 4, 7: 1, 10: 8, 11: 8, 12: 8}
FORMATS = {0: "<B", 1: "<b", 2: "<H", 3: "<h", 4: "<I", 5: "<i", 6: "<f",
           7: "<?", 10: "<Q", 11: "<q", 12: "<d"}


def read(fmt, fh):
    size = struct.calcsize(fmt)
    data = fh.read(size)
    if len(data) != size:
        raise EOFError("the file ends inside a metadata value")
    return struct.unpack(fmt, data)[0]


def read_string(fh):
    length = read("<Q", fh)
    data = fh.read(length)
    if len(data) != length:
        raise EOFError("the file ends inside a metadata string")
    return data.decode("utf-8")


def skip_value(kind, fh):
    if kind == 8:
        read_string(fh)
    elif kind == 9:
        element = read("<I", fh)
        for _ in range(read("<Q", fh)):
            skip_value(element, fh)
    elif kind in SIZES:
        fh.read(SIZES[kind])
    else:
        raise ValueError("unknown metadata value type: %d" % kind)


def main():
    with open(sys.argv[1], "rb") as fh:
        if fh.read(4) != b"GGUF":
            raise ValueError("this is not a GGUF file")
        read("<I", fh)  # version
        read("<Q", fh)  # tensor count
        pairs = read("<Q", fh)
        for _ in range(pairs):
            key = read_string(fh)
            kind = read("<I", fh)
            if key.endswith("classifier.output_labels"):
                if kind != 9:
                    raise ValueError("the label field is not an array")
                element = read("<I", fh)
                if element != 8:
                    raise ValueError("the label field is not an array of strings")
                labels = [read_string(fh) for _ in range(read("<Q", fh))]
                with open(sys.argv[2], "w", encoding="utf-8") as out:
                    out.write("\n".join(labels) + "\n")
                return
            skip_value(kind, fh)
        raise ValueError("no label field in the metadata")


main()
PY
READ_RC=$?
set -e
if [ "$READ_RC" -ne 0 ]; then
  echo "could not read the model's label field (python3 rc=$READ_RC)" >&2
  exit 1
fi

RAW_COUNT="$(wc -l <"$RAW" | tr -d ' ')"
RAW_FIRST="$(head -n 1 "$RAW")"
RAW_LAST="$(tail -n 1 "$RAW")"

if [ "$RAW_COUNT" != "$INFO_COUNT" ]; then
  echo "label count disagrees: pf-cli --info says $INFO_COUNT," \
    "the metadata carries $RAW_COUNT" >&2
  exit 1
fi
if [ "$RAW_FIRST" != "$INFO_FIRST" ] || [ "$RAW_LAST" != "$INFO_LAST" ]; then
  echo "boundaries disagree: pf-cli --info says [$INFO_FIRST ... $INFO_LAST]," \
    "the metadata carries [$RAW_FIRST ... $RAW_LAST]" >&2
  exit 1
fi

# --- BIOES tag stripped, deduplicated, sorted -------------------------------
groups=()
while IFS= read -r label; do
  case "$label" in
    O) continue ;;
    B-* | I-* | E-* | S-*) groups+=("${label:2}") ;;
    *)
      # A label carrying no recognised tag means the shape of the vocabulary moved.
      # Skipping it in silence would yield a short list that nothing reports.
      echo "label with no recognised BIOES tag: $label" >&2
      exit 1
      ;;
  esac
done <"$RAW"

if [ "${#groups[@]}" -eq 0 ]; then
  echo "no group derived: the vocabulary carries only the outside label" >&2
  exit 1
fi

echo "$INFO_COUNT labels, ${#groups[@]} of them tagged, in $PF_MODEL_PATH" >&2
# LC_ALL=C: the order of the fixture must not depend on the locale of the host that
# produced it, or an exact comparison would fail from one machine to the next.
LC_ALL=C printf '%s\n' "${groups[@]}" | LC_ALL=C sort -u
