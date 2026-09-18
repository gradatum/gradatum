// Model engine wrapper.
//
// Loads the weights once through the upstream C API and classifies repeatedly
// on the same context. What leaves this file is a category label and a byte
// range: the model's own `text` field — the matched value in clear — is never
// copied, returned, or logged.
#pragma once

#include <cstdint>
#include <string>
#include <vector>

#include "pf.h"

namespace sidecar {

// One detected span. Deliberately carries no text.
struct Span {
    std::string label;  // model entity group, e.g. its uppercase category name
    std::int32_t start = 0;
    std::int32_t end = 0;
    float score = 0.0f;
};

class PfEngine {
public:
    PfEngine() = default;
    ~PfEngine();
    PfEngine(const PfEngine &) = delete;
    PfEngine & operator=(const PfEngine &) = delete;

    // Loads the weights once. `device` is passed to the upstream loader. The
    // loader reads the label vocabulary itself; the digest reported to clients
    // is computed by the caller from the same file, so the engine keeps no
    // second copy of it.
    bool load(const std::string & model_path, const std::string & device, int threads,
              std::string & error);

    // Classifies one body. The upstream context serialises classification, so
    // this must be called from a single thread — which the single-client server
    // loop guarantees.
    bool classify(const std::string & body, std::vector<Span> & out, std::string & error);

    bool loaded() const { return ctx_ != nullptr; }

private:
    pf_ctx * ctx_ = nullptr;
};

// Reads the label vocabulary out of the GGUF metadata without loading weights.
bool read_model_labels(const std::string & model_path, std::vector<std::string> & labels,
                       std::string & error);

// Digest over a label vocabulary, rendered as "sha256:<hex>". Two models that
// share weights but disagree on labels are told apart by this value.
std::string labels_digest(const std::vector<std::string> & labels);

}  // namespace sidecar
