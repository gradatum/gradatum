// pf-sidecar — Unix-socket privacy-detection sidecar.
//
// Loads the weights once, then answers classification requests over a local
// socket. It reports the digests of the binary it is running from and of the
// weights it loaded, refuses to start when those diverge from what an operator
// pinned, and never emits or logs the matched values themselves.
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#include "pf_engine.h"
#include "protocol.h"
#include "server.h"
#include "sha256.h"

namespace {

constexpr const char * kSelfExePath = "/proc/self/exe";
constexpr int kDefaultThreads = 4;

// Exit codes. Distinct so an operator reading only the status can tell a
// refused digest from a refused bind.
constexpr int kExitOk = 0;
constexpr int kExitUsage = 2;
constexpr int kExitDigest = 3;
constexpr int kExitLoad = 4;
constexpr int kExitSocket = 5;

struct Options {
    std::string socket_path;
    std::string model_path;
    std::string device = "cpu";
    std::string expect_binary_sha256;
    std::string expect_model_sha256;
    int threads = kDefaultThreads;
    std::uint32_t max_input_bytes = sidecar::kDefaultMaxInputBytes;
    bool selfcheck = false;
};

void usage(std::FILE * out) {
    std::fprintf(out,
                 "usage: pf-sidecar --socket PATH --model MODEL.gguf [options]\n"
                 "       pf-sidecar --selfcheck --model MODEL.gguf\n"
                 "\n"
                 "options:\n"
                 "  --socket PATH                 Unix socket to serve on\n"
                 "  --model PATH                  GGUF weights\n"
                 "  --threads N                   inference threads (default %d)\n"
                 "  --max-input-bytes N           refuse longer bodies (default %u)\n"
                 "  --device NAME                 inference device (default cpu)\n"
                 "  --expect-binary-sha256 HEX    refuse to start unless this binary matches\n"
                 "  --expect-model-sha256 HEX     refuse to start unless the weights match\n"
                 "  --selfcheck                   print the digests and exit\n"
                 "  --help                        this text\n"
                 "\nexit codes: 0 ok, %d usage, %d digest refused, %d model refused, %d socket refused\n",
                 kDefaultThreads, sidecar::kDefaultMaxInputBytes, kExitUsage, kExitDigest,
                 kExitLoad, kExitSocket);
}

bool parse_u32(const std::string & text, std::uint32_t & out) {
    if (text.empty()) {
        return false;
    }
    std::uint64_t value = 0;
    for (char c : text) {
        if (c < '0' || c > '9') {
            return false;
        }
        value = value * 10 + static_cast<std::uint64_t>(c - '0');
        if (value > 0xffffffffull) {
            return false;
        }
    }
    out = static_cast<std::uint32_t>(value);
    return true;
}

bool is_lower_hex_64(const std::string & text) {
    if (text.size() != 64) {
        return false;
    }
    for (char c : text) {
        const bool digit = c >= '0' && c <= '9';
        const bool lower = c >= 'a' && c <= 'f';
        if (!digit && !lower) {
            return false;
        }
    }
    return true;
}

// Digest comparison is case-insensitive on input, strict on output: digests are
// reported lowercase, which is how every operator-facing tool renders them.
std::string lowercase(const std::string & text) {
    std::string out = text;
    for (char & c : out) {
        if (c >= 'A' && c <= 'F') {
            c = static_cast<char>(c - 'A' + 'a');
        }
    }
    return out;
}

bool parse_args(int argc, char ** argv, Options & opt) {
    for (int i = 1; i < argc; i++) {
        const std::string arg = argv[i];
        auto next = [&](std::string & dst) {
            if (i + 1 >= argc) {
                return false;
            }
            dst = argv[++i];
            return true;
        };
        if (arg == "--help" || arg == "-h") {
            usage(stdout);
            std::exit(kExitOk);
        } else if (arg == "--selfcheck") {
            opt.selfcheck = true;
        } else if (arg == "--socket") {
            if (!next(opt.socket_path)) return false;
        } else if (arg == "--model") {
            if (!next(opt.model_path)) return false;
        } else if (arg == "--device") {
            if (!next(opt.device)) return false;
        } else if (arg == "--expect-binary-sha256") {
            if (!next(opt.expect_binary_sha256)) return false;
        } else if (arg == "--expect-model-sha256") {
            if (!next(opt.expect_model_sha256)) return false;
        } else if (arg == "--threads") {
            std::string value;
            if (!next(value)) return false;
            std::uint32_t parsed = 0;
            if (!parse_u32(value, parsed) || parsed == 0) {
                std::fprintf(stderr, "pf-sidecar: --threads expects a positive integer\n");
                return false;
            }
            opt.threads = static_cast<int>(parsed);
        } else if (arg == "--max-input-bytes") {
            std::string value;
            if (!next(value)) return false;
            if (!parse_u32(value, opt.max_input_bytes)) {
                std::fprintf(stderr, "pf-sidecar: --max-input-bytes expects a non-negative integer\n");
                return false;
            }
        } else {
            std::fprintf(stderr, "pf-sidecar: unknown argument: %s\n", arg.c_str());
            return false;
        }
    }
    if (opt.model_path.empty()) {
        std::fprintf(stderr, "pf-sidecar: --model is required\n");
        return false;
    }
    if (!opt.selfcheck && opt.socket_path.empty()) {
        std::fprintf(stderr, "pf-sidecar: --socket is required unless --selfcheck is given\n");
        return false;
    }
    return true;
}

// Digests are computed from the running image and from the weights on disk, then
// checked against whatever the operator pinned. A mismatch is a refusal to
// serve, never a warning: serving under a digest that was not pinned is exactly
// the state the pin exists to prevent.
bool collect_digests(const Options & opt, std::string & binary_sha, std::string & model_sha,
                     std::string & error) {
    if (!sidecar::Sha256::of_path(kSelfExePath, binary_sha, error)) {
        error = "cannot digest the running image: " + error;
        return false;
    }
    if (!sidecar::Sha256::of_path(opt.model_path, model_sha, error)) {
        return false;
    }
    if (!opt.expect_binary_sha256.empty()) {
        const std::string want = lowercase(opt.expect_binary_sha256);
        if (!is_lower_hex_64(want)) {
            error = "--expect-binary-sha256 is not 64 hexadecimal characters";
            return false;
        }
        if (want != binary_sha) {
            error = "binary digest divergence: expected " + want + ", running " + binary_sha;
            return false;
        }
    }
    if (!opt.expect_model_sha256.empty()) {
        const std::string want = lowercase(opt.expect_model_sha256);
        if (!is_lower_hex_64(want)) {
            error = "--expect-model-sha256 is not 64 hexadecimal characters";
            return false;
        }
        if (want != model_sha) {
            error = "model digest divergence: expected " + want + ", found " + model_sha;
            return false;
        }
    }
    return true;
}

}  // namespace

int main(int argc, char ** argv) {
    Options opt;
    if (!parse_args(argc, argv, opt)) {
        usage(stderr);
        return kExitUsage;
    }
    sidecar::install_process_handlers();

    std::string binary_sha;
    std::string model_sha;
    std::string error;
    if (!collect_digests(opt, binary_sha, model_sha, error)) {
        std::fprintf(stderr, "pf-sidecar: %s\n", error.c_str());
        return kExitDigest;
    }

    std::vector<std::string> labels;
    if (!sidecar::read_model_labels(opt.model_path, labels, error)) {
        std::fprintf(stderr, "pf-sidecar: %s\n", error.c_str());
        return kExitDigest;
    }
    const std::string labels_v = sidecar::labels_digest(labels);
    const std::size_t label_count = labels.size();
    labels.clear();

    if (opt.selfcheck) {
        std::printf("binary_sha256=%s\n", binary_sha.c_str());
        std::printf("model_sha256=%s\n", model_sha.c_str());
        std::printf("labels_v=%s\n", labels_v.c_str());
        std::printf("labels_count=%zu\n", label_count);
        std::printf("mapping_v=%d\n", sidecar::kMappingV);
        std::printf("score_mode=%s\n", sidecar::kScoreMode);
        std::printf("max_frame_bytes=%u\n", sidecar::kMaxFrameBytes);
        std::printf("max_input_bytes=%u\n", opt.max_input_bytes);
        std::printf("threads=%d\n", opt.threads);
        return kExitOk;
    }

    sidecar::PfEngine engine;
    if (!engine.load(opt.model_path, opt.device, opt.threads, error)) {
        std::fprintf(stderr, "pf-sidecar: %s\n", error.c_str());
        return kExitLoad;
    }

    sidecar::ServerConfig cfg;
    cfg.socket_path = opt.socket_path;
    cfg.binary_sha256 = binary_sha;
    cfg.model_sha256 = model_sha;
    cfg.labels_v = labels_v;
    cfg.max_input_bytes = opt.max_input_bytes;
    cfg.threads = opt.threads;

    if (!sidecar::serve(cfg, engine, error)) {
        std::fprintf(stderr, "pf-sidecar: %s\n", error.c_str());
        return kExitSocket;
    }
    return kExitOk;
}
