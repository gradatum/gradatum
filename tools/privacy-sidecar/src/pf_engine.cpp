#include "pf_engine.h"

#include "gguf_loader.h"
#include "protocol.h"
#include "sha256.h"

namespace sidecar {

PfEngine::~PfEngine() {
    if (ctx_ != nullptr) {
        pf_free(ctx_);
        ctx_ = nullptr;
    }
}

bool PfEngine::load(const std::string & model_path, const std::string & device, int threads,
                    std::string & error) {
    ctx_ = pf_load(model_path.c_str(), device.c_str(), threads);
    const char * last = pf_last_error(ctx_);
    if (last != nullptr) {
        error = std::string("model load refused: ") + last;
        pf_free(ctx_);
        ctx_ = nullptr;
        return false;
    }
    return true;
}

bool PfEngine::classify(const std::string & body, std::vector<Span> & out, std::string & error) {
    out.clear();
    if (ctx_ == nullptr) {
        error = "classification requested before a successful load";
        return false;
    }
    pf_entity * ents = nullptr;
    std::size_t n_ents = 0;
    const int rc = pf_classify(ctx_, body.data(), body.size(), kEmitThreshold, &ents, &n_ents);
    if (rc != 0) {
        const char * last = pf_last_error(ctx_);
        error = std::string("classification failed: ") + (last != nullptr ? last : "unknown");
        return false;
    }
    out.reserve(n_ents);
    for (std::size_t i = 0; i < n_ents; i++) {
        Span s;
        // Only the category and the geometry are taken. `ents[i]` also carries
        // the matched bytes; copying them here is what must never happen.
        s.label = ents[i].label != nullptr ? ents[i].label : "";
        s.start = ents[i].start;
        s.end = ents[i].end;
        s.score = ents[i].score;
        out.push_back(s);
    }
    pf_entities_free(ents, n_ents);
    return true;
}

bool read_model_labels(const std::string & model_path, std::vector<std::string> & labels,
                       std::string & error) {
    pf::model_file mf;
    if (!mf.open(model_path, /*with_data=*/false)) {
        error = "cannot read model metadata: " + mf.error;
        return false;
    }
    labels = mf.labels;
    if (labels.empty()) {
        error = "model metadata carries no label vocabulary";
        return false;
    }
    return true;
}

std::string labels_digest(const std::vector<std::string> & labels) {
    std::string joined;
    for (std::size_t i = 0; i < labels.size(); i++) {
        if (i > 0) {
            joined.push_back('\n');
        }
        joined += labels[i];
    }
    return "sha256:" + Sha256::of_buffer(joined.data(), joined.size());
}

}  // namespace sidecar
