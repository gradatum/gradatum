// Wire protocol of the sidecar: one length-prefixed JSON message per frame.
//
// A frame is a u32 big-endian byte length followed by that many bytes of UTF-8
// JSON. Everything that bounds a frame lives here, so a client and the server
// cannot drift on the caps.
#pragma once

#include <cstddef>
#include <cstdint>
#include <string>

namespace sidecar {

// Hard cap on an inbound frame. Above it the connection is closed rather than
// answered: a peer sending more than this is not speaking the protocol.
constexpr std::uint32_t kMaxFrameBytes = 16u * 1024u * 1024u;

// Hard cap on an outbound frame. Structurally unreachable while the span count
// is bounded: kMaxResponseSpans spans of ~40 bytes sit far below it.
constexpr std::uint32_t kMaxResponseBytes = 64u * 1024u;
constexpr std::size_t kMaxResponseSpans = 256;

// Default refusal threshold for one request body.
constexpr std::uint32_t kDefaultMaxInputBytes = 262144u;

// Emitted in the handshake. `mapping_v` is the label-mapping revision this
// process applies — 0 means it applies none and reports the model's own labels.
// `score_mode` names how the emitted score is produced.
constexpr int kMappingV = 0;
constexpr const char * kScoreMode = "mean_token_prob";

// Emission threshold handed to the model. Spans below it are dropped.
constexpr float kEmitThreshold = 0.0f;

// Bounds on a request identifier. The identifier is copied into log lines, and
// the log is the audit surface: an identifier able to carry a newline could
// forge an event, and an unbounded one could bury the log. Only an identifier
// matching kMaxIdBytes of the safe alphabet is accepted.
constexpr std::size_t kMaxIdBytes = 64;

// Maximum nesting depth accepted in a request document. The request schema is
// two levels deep; the cap exists because the parser recurses on nesting, so a
// frame of nothing but brackets would otherwise exhaust the stack.
constexpr std::size_t kMaxJsonDepth = 32;

enum class FrameStatus {
    Ok,        // a complete frame was read into the output buffer
    Eof,       // the peer closed cleanly between frames
    TooLarge,  // the announced length exceeds kMaxFrameBytes
    Invalid,   // I/O error, truncated frame, or a zero-length frame
};

// Reads one frame. On TooLarge the announced length is reported in `detail`.
FrameStatus read_frame(int fd, std::string & out, std::string & detail);

// Writes one frame. Returns false and fills `error` on a short write.
bool write_frame(int fd, const std::string & payload, std::string & error);

// True when `id` is non-empty, at most kMaxIdBytes long, and made only of
// [A-Za-z0-9._:-]. Those are the identifier characters the protocol documents,
// and they are safe to place in a single-token log field.
bool is_valid_id(const std::string & id);

// True when the document nests no deeper than `max_depth`. Scans bytes, so an
// unbalanced document is not an error here: the parser is what rejects it.
bool json_depth_within(const std::string & payload, std::size_t max_depth);

}  // namespace sidecar
