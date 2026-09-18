// SHA-256 (FIPS 180-4) — self-contained, streaming.
//
// The sidecar digests the artefact it is running from and the weights it was
// pointed at, so that both can be pinned by an operator and reported to a
// client. Carrying an implementation here keeps the build free of a crypto
// dependency it would otherwise need for two hashes at start-up.
#pragma once

#include <cstddef>
#include <cstdint>
#include <string>

namespace sidecar {

class Sha256 {
public:
    Sha256();

    void update(const void * data, std::size_t len);
    // Writes the 32-byte digest and resets the object.
    void finish(unsigned char out[32]);

    // Lowercase hexadecimal rendering of a digest.
    static std::string hex(const unsigned char digest[32]);
    static std::string of_buffer(const void * data, std::size_t len);
    // Returns false and fills `error` when the path cannot be read.
    static bool of_path(const std::string & path, std::string & out_hex, std::string & error);

private:
    void compress(const unsigned char block[64]);

    std::uint32_t state_[8];
    std::uint64_t total_len_;
    unsigned char buffer_[64];
    std::size_t buffered_;
};

}  // namespace sidecar
