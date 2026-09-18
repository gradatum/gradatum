#include "sha256.h"

#include <cstdio>
#include <cstring>

namespace sidecar {
namespace {

constexpr std::uint32_t kK[64] = {
    0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u, 0x3956c25bu, 0x59f111f1u, 0x923f82a4u,
    0xab1c5ed5u, 0xd807aa98u, 0x12835b01u, 0x243185beu, 0x550c7dc3u, 0x72be5d74u, 0x80deb1feu,
    0x9bdc06a7u, 0xc19bf174u, 0xe49b69c1u, 0xefbe4786u, 0x0fc19dc6u, 0x240ca1ccu, 0x2de92c6fu,
    0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau, 0x983e5152u, 0xa831c66du, 0xb00327c8u, 0xbf597fc7u,
    0xc6e00bf3u, 0xd5a79147u, 0x06ca6351u, 0x14292967u, 0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu,
    0x53380d13u, 0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u, 0xa2bfe8a1u, 0xa81a664bu,
    0xc24b8b70u, 0xc76c51a3u, 0xd192e819u, 0xd6990624u, 0xf40e3585u, 0x106aa070u, 0x19a4c116u,
    0x1e376c08u, 0x2748774cu, 0x34b0bcb5u, 0x391c0cb3u, 0x4ed8aa4au, 0x5b9cca4fu, 0x682e6ff3u,
    0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u, 0x90befffau, 0xa4506cebu, 0xbef9a3f7u,
    0xc67178f2u,
};

inline std::uint32_t rotr(std::uint32_t x, unsigned n) { return (x >> n) | (x << (32 - n)); }

}  // namespace

Sha256::Sha256() : total_len_(0), buffered_(0) {
    state_[0] = 0x6a09e667u;
    state_[1] = 0xbb67ae85u;
    state_[2] = 0x3c6ef372u;
    state_[3] = 0xa54ff53au;
    state_[4] = 0x510e527fu;
    state_[5] = 0x9b05688cu;
    state_[6] = 0x1f83d9abu;
    state_[7] = 0x5be0cd19u;
    std::memset(buffer_, 0, sizeof(buffer_));
}

void Sha256::compress(const unsigned char block[64]) {
    std::uint32_t w[64];
    for (int i = 0; i < 16; i++) {
        w[i] = (std::uint32_t) block[i * 4] << 24 | (std::uint32_t) block[i * 4 + 1] << 16 |
               (std::uint32_t) block[i * 4 + 2] << 8 | (std::uint32_t) block[i * 4 + 3];
    }
    for (int i = 16; i < 64; i++) {
        const std::uint32_t s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >> 3);
        const std::uint32_t s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    std::uint32_t a = state_[0], b = state_[1], c = state_[2], d = state_[3];
    std::uint32_t e = state_[4], f = state_[5], g = state_[6], h = state_[7];
    for (int i = 0; i < 64; i++) {
        const std::uint32_t s1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25);
        const std::uint32_t ch = (e & f) ^ (~e & g);
        const std::uint32_t t1 = h + s1 + ch + kK[i] + w[i];
        const std::uint32_t s0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22);
        const std::uint32_t maj = (a & b) ^ (a & c) ^ (b & c);
        const std::uint32_t t2 = s0 + maj;
        h = g;
        g = f;
        f = e;
        e = d + t1;
        d = c;
        c = b;
        b = a;
        a = t1 + t2;
    }
    state_[0] += a;
    state_[1] += b;
    state_[2] += c;
    state_[3] += d;
    state_[4] += e;
    state_[5] += f;
    state_[6] += g;
    state_[7] += h;
}

void Sha256::update(const void * data, std::size_t len) {
    const unsigned char * p = static_cast<const unsigned char *>(data);
    total_len_ += len;
    if (buffered_ > 0) {
        const std::size_t want = 64 - buffered_;
        const std::size_t take = len < want ? len : want;
        std::memcpy(buffer_ + buffered_, p, take);
        buffered_ += take;
        p += take;
        len -= take;
        if (buffered_ == 64) {
            compress(buffer_);
            buffered_ = 0;
        }
    }
    while (len >= 64) {
        compress(p);
        p += 64;
        len -= 64;
    }
    if (len > 0) {
        std::memcpy(buffer_, p, len);
        buffered_ = len;
    }
}

void Sha256::finish(unsigned char out[32]) {
    // Padding and the 64-bit big-endian bit length are written straight into the
    // block buffer: routing them through update() would corrupt total_len_,
    // which is read here to build the length field.
    const std::uint64_t bit_len = total_len_ * 8;
    buffer_[buffered_++] = 0x80;
    if (buffered_ > 56) {
        while (buffered_ < 64) {
            buffer_[buffered_++] = 0x00;
        }
        compress(buffer_);
        buffered_ = 0;
    }
    while (buffered_ < 56) {
        buffer_[buffered_++] = 0x00;
    }
    for (int i = 0; i < 8; i++) {
        buffer_[56 + i] = static_cast<unsigned char>((bit_len >> (56 - 8 * i)) & 0xffu);
    }
    compress(buffer_);
    for (int i = 0; i < 8; i++) {
        out[i * 4] = static_cast<unsigned char>((state_[i] >> 24) & 0xffu);
        out[i * 4 + 1] = static_cast<unsigned char>((state_[i] >> 16) & 0xffu);
        out[i * 4 + 2] = static_cast<unsigned char>((state_[i] >> 8) & 0xffu);
        out[i * 4 + 3] = static_cast<unsigned char>(state_[i] & 0xffu);
    }
    *this = Sha256();
}

std::string Sha256::hex(const unsigned char digest[32]) {
    static const char * digits = "0123456789abcdef";
    std::string out(64, '0');
    for (int i = 0; i < 32; i++) {
        out[i * 2] = digits[digest[i] >> 4];
        out[i * 2 + 1] = digits[digest[i] & 0x0f];
    }
    return out;
}

std::string Sha256::of_buffer(const void * data, std::size_t len) {
    Sha256 h;
    h.update(data, len);
    unsigned char digest[32];
    h.finish(digest);
    return hex(digest);
}

bool Sha256::of_path(const std::string & path, std::string & out_hex, std::string & error) {
    FILE * f = std::fopen(path.c_str(), "rb");
    if (f == nullptr) {
        error = "cannot open for hashing: " + path;
        return false;
    }
    Sha256 h;
    unsigned char buf[1 << 16];
    std::size_t got = 0;
    while ((got = std::fread(buf, 1, sizeof(buf), f)) > 0) {
        h.update(buf, got);
    }
    const bool failed = std::ferror(f) != 0;
    std::fclose(f);
    if (failed) {
        error = "read error while hashing: " + path;
        return false;
    }
    unsigned char digest[32];
    h.finish(digest);
    out_hex = hex(digest);
    return true;
}

}  // namespace sidecar
