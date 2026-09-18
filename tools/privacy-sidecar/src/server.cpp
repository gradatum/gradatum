#include "server.h"

#include <cerrno>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstring>
#include <poll.h>
#include <string>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>
#include <vector>

#include <nlohmann/json.hpp>

#include "protocol.h"

namespace sidecar {
namespace {

volatile sig_atomic_t g_stop = 0;

void on_stop_signal(int) { g_stop = 1; }

// A peer that connects and then stalls must not park the accept loop: one
// client is served at a time, so an unbounded read would hold the only slot
// forever and delay the stop signal with it. Either direction timing out is
// treated as a broken peer and closes the connection.
constexpr int kPeerIoTimeoutSec = 120;

using json = nlohmann::json;

// Everything the sidecar logs goes through here, and the format is the audit
// surface: event name, identifiers, byte lengths, status codes, durations. No
// request body, no span field and no model output ever reaches this function —
// the call sites have nothing else to pass.
std::chrono::steady_clock::time_point g_start;

void log_event(const std::string & event, const std::string & fields = std::string()) {
    const auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                        std::chrono::steady_clock::now() - g_start)
                        .count();
    if (fields.empty()) {
        std::fprintf(stderr, "sidecar t_ms=%lld ev=%s\n", static_cast<long long>(ms),
                     event.c_str());
    } else {
        std::fprintf(stderr, "sidecar t_ms=%lld ev=%s %s\n", static_cast<long long>(ms),
                     event.c_str(), fields.c_str());
    }
    std::fflush(stderr);
}

// Peer admission. The socket permissions already restrict who can connect; this
// is the second lock, and it fails closed. Same-user peers are accepted, and so
// are peers whose effective group is the group this process runs as — which is
// the shared group that carries the socket's group permission.
bool peer_admitted(int fd, std::string & detail) {
    struct ucred cred;
    socklen_t len = sizeof(cred);
    if (::getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &cred, &len) != 0) {
        detail = "so_peercred_unavailable";
        return false;
    }
    if (cred.uid != ::getuid() && cred.gid != ::getgid()) {
        detail = "uid=" + std::to_string(cred.uid) + ",gid=" + std::to_string(cred.gid);
        return false;
    }
    return true;
}

json spans_to_json(const std::vector<Span> & spans, std::size_t take) {
    json arr = json::array();
    for (std::size_t i = 0; i < take; i++) {
        arr.push_back(json{{"label", spans[i].label},
                           {"start", spans[i].start},
                           {"end", spans[i].end},
                           {"score", spans[i].score}});
    }
    return arr;
}

// Serves one connection to completion. Returns when the peer closes, when the
// protocol is broken, or when a stop signal arrives.
void handle_connection(int fd, const ServerConfig & cfg, PfEngine & engine) {
    const json handshake{{"binary_sha256", cfg.binary_sha256},
                         {"model_sha256", cfg.model_sha256},
                         {"mapping_v", kMappingV},
                         {"labels_v", cfg.labels_v},
                         {"score_mode", kScoreMode},
                         {"ready", true}};
    std::string err;
    if (!write_frame(fd, handshake.dump(), err)) {
        log_event("handshake_failed", "reason=write_error");
        return;
    }
    log_event("handshake", "ready=true labels_v=" + cfg.labels_v);

    for (;;) {
        std::string payload;
        std::string detail;
        const FrameStatus status = read_frame(fd, payload, detail);
        if (status == FrameStatus::Eof) {
            log_event("peer_closed", "");
            return;
        }
        if (status == FrameStatus::TooLarge) {
            // Announce the length that was refused, then close: the peer is not
            // speaking the protocol and its stream cannot be resynchronised.
            log_event("reject id=- reason=frame_too_large bytes=" + detail +
                      " cap=" + std::to_string(kMaxFrameBytes));
            return;
        }
        if (status == FrameStatus::Invalid) {
            log_event("reject id=- reason=" + detail);
            return;
        }

        // The depth gate runs before the parser: the parser recurses on nesting,
        // so a frame of nothing but brackets would exhaust the stack before any
        // schema check could reject it.
        if (!json_depth_within(payload, kMaxJsonDepth)) {
            log_event("reject id=- reason=json_too_deep bytes=" +
                      std::to_string(payload.size()));
            return;
        }
        const json req = json::parse(payload, nullptr, /*allow_exceptions=*/false);
        if (req.is_discarded() || !req.is_object()) {
            log_event("reject id=- reason=bad_json bytes=" + std::to_string(payload.size()));
            return;
        }
        if (!req.contains("id") || !req["id"].is_string() || !req.contains("body") ||
            !req["body"].is_string()) {
            log_event("reject id=- reason=bad_request bytes=" + std::to_string(payload.size()));
            return;
        }
        const std::string id = req["id"].get<std::string>();
        // The identifier goes into log lines, so it is admitted by alphabet and
        // by length first. A refused identifier is measured, never echoed: the
        // value that failed the check is exactly the one that must not be
        // printed.
        if (!is_valid_id(id)) {
            log_event("reject id=- reason=bad_id id_bytes=" + std::to_string(id.size()));
            return;
        }
        const std::string body = req["body"].get<std::string>();

        // Refusal happens before the model is touched, so an oversized body
        // costs one length comparison and nothing else.
        if (body.size() > cfg.max_input_bytes) {
            json resp{{"id", id}, {"spans", json::array()}, {"status", "input_too_large"}};
            log_event("reject id=" + id + " status=input_too_large in_bytes=" +
                      std::to_string(body.size()) + " max_bytes=" +
                      std::to_string(cfg.max_input_bytes));
            if (!write_frame(fd, resp.dump(), err)) {
                return;
            }
            continue;
        }

        std::vector<Span> spans;
        std::string cls_err;
        const auto t0 = std::chrono::steady_clock::now();
        const bool ok = engine.classify(body, spans, cls_err);
        const auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                            std::chrono::steady_clock::now() - t0)
                            .count();
        if (!ok) {
            // The failure text comes from the upstream loader and carries no
            // input data; it is a diagnostic, not a payload.
            log_event("classify_failed id=" + id + " in_bytes=" + std::to_string(body.size()) +
                      " ms=" + std::to_string(ms));
            return;
        }

        const std::size_t total = spans.size();
        const std::size_t take = total > kMaxResponseSpans ? kMaxResponseSpans : total;
        json resp{{"id", id}, {"spans", spans_to_json(spans, take)}, {"status", "ok"}};
        const std::string rendered = resp.dump();
        if (take < total) {
            log_event("spans_dropped id=" + id + " spans_total=" + std::to_string(total) +
                      " spans_kept=" + std::to_string(take));
        }
        log_event("classify id=" + id + " in_bytes=" + std::to_string(body.size()) +
                  " spans=" + std::to_string(take) + " ms=" + std::to_string(ms));
        if (!write_frame(fd, rendered, err)) {
            log_event("write_failed id=" + id + " reason=" + err);
            return;
        }
    }
}

}  // namespace

void install_process_handlers() {
    g_start = std::chrono::steady_clock::now();
    struct sigaction sa;
    std::memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_stop_signal;
    ::sigaction(SIGTERM, &sa, nullptr);
    ::sigaction(SIGINT, &sa, nullptr);
    ::sigaction(SIGHUP, &sa, nullptr);
    // A peer that disconnects mid-write must not take the process down.
    struct sigaction ignore;
    std::memset(&ignore, 0, sizeof(ignore));
    ignore.sa_handler = SIG_IGN;
    ::sigaction(SIGPIPE, &ignore, nullptr);
}

bool serve(const ServerConfig & cfg, PfEngine & engine, std::string & error) {
    struct sockaddr_un addr;
    std::memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    if (cfg.socket_path.size() + 1 > sizeof(addr.sun_path)) {
        error = "socket path too long for AF_UNIX (" + std::to_string(cfg.socket_path.size()) +
                " bytes, limit " + std::to_string(sizeof(addr.sun_path) - 1) + ")";
        return false;
    }
    std::memcpy(addr.sun_path, cfg.socket_path.c_str(), cfg.socket_path.size());

    // Start-up guard: never unlink a path we did not create. A leftover path is
    // either another service's socket or the debris of an unclean exit, and both
    // deserve an operator rather than a blind overwrite.
    struct stat st;
    if (::lstat(cfg.socket_path.c_str(), &st) == 0) {
        error = "refusing to bind: path already exists: " + cfg.socket_path;
        return false;
    }

    const int listen_fd = ::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (listen_fd < 0) {
        error = std::string("socket(): ") + std::strerror(errno);
        return false;
    }
    // Restrictive umask while the node is created, then widen to exactly the
    // documented 0660: readable and writable by the shared group, by nobody else.
    const mode_t previous = ::umask(0077);
    const int bind_rc = ::bind(listen_fd, reinterpret_cast<struct sockaddr *>(&addr), sizeof(addr));
    const int bind_errno = errno;
    ::umask(previous);
    if (bind_rc != 0) {
        error = std::string("bind(") + cfg.socket_path + "): " + std::strerror(bind_errno);
        ::close(listen_fd);
        return false;
    }
    if (::chmod(cfg.socket_path.c_str(), 0660) != 0) {
        error = std::string("chmod 0660 on ") + cfg.socket_path + ": " + std::strerror(errno);
        ::close(listen_fd);
        ::unlink(cfg.socket_path.c_str());
        return false;
    }
    if (::listen(listen_fd, 8) != 0) {
        error = std::string("listen(): ") + std::strerror(errno);
        ::close(listen_fd);
        ::unlink(cfg.socket_path.c_str());
        return false;
    }
    log_event("listen", "mode=0660 max_input_bytes=" + std::to_string(cfg.max_input_bytes) +
                            " threads=" + std::to_string(cfg.threads) +
                            " score_mode=" + kScoreMode +
                            " mapping_v=" + std::to_string(kMappingV));

    bool clean = true;
    while (!g_stop) {
        struct pollfd pfd;
        pfd.fd = listen_fd;
        pfd.events = POLLIN;
        pfd.revents = 0;
        const int ready = ::poll(&pfd, 1, 500);
        if (ready < 0) {
            if (errno == EINTR) {
                continue;
            }
            error = std::string("poll(): ") + std::strerror(errno);
            clean = false;
            break;
        }
        if (ready == 0) {
            continue;
        }
        const int conn = ::accept4(listen_fd, nullptr, nullptr, SOCK_CLOEXEC);
        if (conn < 0) {
            if (errno == EINTR) {
                continue;
            }
            log_event("accept_failed", std::string("reason=") + std::strerror(errno));
            continue;
        }
        std::string peer_detail;
        if (!peer_admitted(conn, peer_detail)) {
            log_event("reject id=- reason=peer_not_admitted detail=" + peer_detail);
            ::close(conn);
            continue;
        }
        struct timeval io_timeout;
        io_timeout.tv_sec = kPeerIoTimeoutSec;
        io_timeout.tv_usec = 0;
        ::setsockopt(conn, SOL_SOCKET, SO_RCVTIMEO, &io_timeout, sizeof(io_timeout));
        ::setsockopt(conn, SOL_SOCKET, SO_SNDTIMEO, &io_timeout, sizeof(io_timeout));
        handle_connection(conn, cfg, engine);
        ::close(conn);
    }

    ::close(listen_fd);
    // Leaving the node behind would make the next start refuse to bind, so a
    // clean shutdown removes it.
    ::unlink(cfg.socket_path.c_str());
    log_event("shutdown", clean ? "reason=signal" : "reason=error");
    return clean;
}

}  // namespace sidecar
