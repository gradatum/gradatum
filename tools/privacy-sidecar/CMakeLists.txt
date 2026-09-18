cmake_minimum_required(VERSION 3.21)
project(gradatum-privacy-sidecar CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

# ---------------------------------------------------------------------------
# Inputs. Neither the engine nor the JSON header is vendored in this repository:
# fetch-upstream.sh clones or downloads them at pinned revisions into a build
# directory outside the tree, and points the two cache variables below at the
# result.
# ---------------------------------------------------------------------------
set(PF_UPSTREAM_DIR "" CACHE PATH "Checkout of privacy-filter.cpp at the pinned revision")
set(PF_JSON_INCLUDE_DIR "" CACHE PATH "Directory containing nlohmann/json.hpp")

if(NOT PF_UPSTREAM_DIR OR NOT EXISTS "${PF_UPSTREAM_DIR}/CMakeLists.txt")
    message(FATAL_ERROR
        "PF_UPSTREAM_DIR must point at a privacy-filter.cpp checkout "
        "(got '${PF_UPSTREAM_DIR}'). Run fetch-upstream.sh first; it clones the "
        "pinned revision and reports the path to pass here.")
endif()
if(NOT PF_JSON_INCLUDE_DIR OR NOT EXISTS "${PF_JSON_INCLUDE_DIR}/nlohmann/json.hpp")
    message(FATAL_ERROR
        "PF_JSON_INCLUDE_DIR must contain nlohmann/json.hpp "
        "(got '${PF_JSON_INCLUDE_DIR}'). fetch-upstream.sh fetches the pinned "
        "header alongside the checkout.")
endif()

# The upstream tree also provides a CLI and a test suite; neither is wanted in
# this build. The variables are pre-set so the upstream options see them.
set(PF_BUILD_TOOLS OFF CACHE BOOL "upstream CLI" FORCE)
set(PF_BUILD_TESTS OFF CACHE BOOL "upstream tests" FORCE)
set(GGML_BUILD_TESTS OFF CACHE BOOL "ggml tests" FORCE)
set(GGML_BUILD_EXAMPLES OFF CACHE BOOL "ggml examples" FORCE)
add_subdirectory("${PF_UPSTREAM_DIR}" "${CMAKE_CURRENT_BINARY_DIR}/upstream")

find_package(Threads REQUIRED)

# ---------------------------------------------------------------------------
# The sidecar. It links the upstream static library and includes the upstream
# private headers for one reason: the label vocabulary is read from the GGUF
# metadata through the same loader the engine uses. No model weight, no binary
# and no third-party source enters the repository.
# ---------------------------------------------------------------------------
add_executable(pf-sidecar
    src/main.cpp
    src/pf_engine.cpp
    src/protocol.cpp
    src/server.cpp
    src/sha256.cpp
)
target_link_libraries(pf-sidecar PRIVATE pf Threads::Threads)
target_include_directories(pf-sidecar PRIVATE
    "${PF_JSON_INCLUDE_DIR}"
    "${PF_UPSTREAM_DIR}/src"
)
target_compile_options(pf-sidecar PRIVATE -Wall -Wextra)
target_compile_features(pf-sidecar PRIVATE cxx_std_17)

install(TARGETS pf-sidecar RUNTIME DESTINATION "${CMAKE_INSTALL_LIBEXECDIR}/gradatum-privacy")

# ---------------------------------------------------------------------------
# Comparator for the end-to-end harness: the upstream CLI is linked against the
# same static library, so a span produced by pf-sidecar and a span produced by
# this binary must agree offset for offset. Built only when asked for, and it is
# never installed.
# ---------------------------------------------------------------------------
option(PF_SIDECAR_BUILD_REF_CLI
       "Build pf-ref-cli, the span comparator from the same pinned revision" ON)
if(PF_SIDECAR_BUILD_REF_CLI)
    add_executable(pf-ref-cli "${PF_UPSTREAM_DIR}/tools/pf-cli.cpp")
    target_link_libraries(pf-ref-cli PRIVATE pf Threads::Threads)
    target_include_directories(pf-ref-cli PRIVATE "${PF_UPSTREAM_DIR}/src")
    target_compile_options(pf-ref-cli PRIVATE -Wall -Wextra)
endif()
