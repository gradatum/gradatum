#include "protocol.h"

#include <cerrno>
#include <cstring>
#include <unistd.h>

namespace sidecar {
namespace {

// Reads exactly `len` bytes. Returns 1 on success, 0 on a clean EOF before any
// byte, -1 on a truncated read or an I/O error.
int read_full(int fd, char * dst, std::size_t len) {
    std::size_t done = 0;
    while (done < len) {
        const ssize_t got = ::read(fd, dst + done, len - done);
        if (got == 0) {
            return done == 0 ? 0 : -1;
        }
        if (got < 0) {
            // A signal interrupts the read rather than restarting it. The only
            // signals this process handles are stop requests, so the connection
            // is abandoned instead of resumed: that is what keeps shutdown
            // bounded by the peer timeout, not by the peer's willingness to
            // speak.
            return -1;
        }
        done += static_cast<std::size_t>(got);
    }
    return 1;
}

bool write_full(int fd, const char * src, std::size_t len) {
    std::size_t done = 0;
    while (done < len) {
        const ssize_t put = ::write(fd, src + done, len - done);
        if (put < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        done += static_cast<std::size_t>(put);
    }
    return true;
}

}  // namespace

FrameStatus read_frame(int fd, std::string & out, std::string & detail) {
    unsigned char head[4];
    const int got = read_full(fd, reinterpret_cast<char *>(head), sizeof(head));
    if (got == 0) {
        return FrameStatus::Eof;
    }
    if (got < 0) {
        detail = "header_read_failed";
        return FrameStatus::Invalid;
    }
    const std::uint32_t len = (std::uint32_t) head[0] << 24 | (std::uint32_t) head[1] << 16 |
                              (std::uint32_t) head[2] << 8 | (std::uint32_t) head[3];
    if (len > kMaxFrameBytes) {
        detail = std::to_string(len);
        return FrameStatus::TooLarge;
    }
    if (len == 0) {
        detail = "zero_length_frame";
        return FrameStatus::Invalid;
    }
    out.assign(len, '\0');
    if (read_full(fd, out.data(), len) != 1) {
        detail = "truncated_body";
        return FrameStatus::Invalid;
    }
    return FrameStatus::Ok;
}

bool write_frame(int fd, const std::string & payload, std::string & error) {
    if (payload.size() > kMaxResponseBytes) {
        error = "response_too_large";
        return false;
    }
    const std::uint32_t len = static_cast<std::uint32_t>(payload.size());
    unsigned char head[4] = {
        static_cast<unsigned char>((len >> 24) & 0xffu),
        static_cast<unsigned char>((len >> 16) & 0xffu),
        static_cast<unsigned char>((len >> 8) & 0xffu),
        static_cast<unsigned char>(len & 0xffu),
    };
    if (!write_full(fd, reinterpret_cast<const char *>(head), sizeof(head))) {
        error = "write_failed_header";
        return false;
    }
    if (!write_full(fd, payload.data(), payload.size())) {
        error = "write_failed_body";
        return false;
    }
    return true;
}

bool is_valid_id(const std::string & id) {
    if (id.empty() || id.size() > kMaxIdBytes) {
        return false;
    }
    for (std::size_t i = 0; i < id.size(); i++) {
        const unsigned char c = static_cast<unsigned char>(id[i]);
        const bool safe = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
                          (c >= '0' && c <= '9') || c == '.' || c == '_' || c == ':' || c == '-';
        if (!safe) {
            return false;
        }
    }
    return true;
}

bool json_depth_within(const std::string & payload, std::size_t max_depth) {
    std::size_t depth = 0;
    bool in_string = false;
    bool escaped = false;
    for (std::size_t i = 0; i < payload.size(); i++) {
        const char c = payload[i];
        if (in_string) {
            // A backslash escapes the next byte; inside a string no bracket
            // opens or closes a level.
            if (escaped) {
                escaped = false;
            } else if (c == '\\') {
                escaped = true;
            } else if (c == '"') {
                in_string = false;
            }
            continue;
        }
        if (c == '"') {
            in_string = true;
        } else if (c == '{' || c == '[') {
            depth++;
            if (depth > max_depth) {
                return false;
            }
        } else if ((c == '}' || c == ']') && depth > 0) {
            depth--;
        }
    }
    return true;
}

}  // namespace sidecar
