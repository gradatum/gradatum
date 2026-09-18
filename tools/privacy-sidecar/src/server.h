// Unix-socket server: one client at a time, sequential requests.
//
// The upstream context serialises classification, so a single connection
// carrying one request at a time is the shape the engine dictates. Additional
// clients wait in the listen backlog rather than being served concurrently.
#pragma once

#include <cstdint>
#include <string>

#include "pf_engine.h"

namespace sidecar {

struct ServerConfig {
    std::string socket_path;
    // Digests reported to every client at connection time.
    std::string binary_sha256;
    std::string model_sha256;
    std::string labels_v;
    std::uint32_t max_input_bytes = 0;
    int threads = 4;
};

// Serves until a stop signal arrives. Returns false and fills `error` on a
// setup failure; the caller decides the exit code.
bool serve(const ServerConfig & cfg, PfEngine & engine, std::string & error);

// Installs the termination handlers read by the accept loop, and ignores
// SIGPIPE so a client that vanishes mid-write cannot kill the process.
void install_process_handlers();

}  // namespace sidecar
