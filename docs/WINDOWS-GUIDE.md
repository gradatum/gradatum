# Windows User & Contributor Guide

> **DEFERRED (2026-06-05)**: Windows is no longer a supported target. gradatum is
> Linux-only. This document is archived for historical reference.

> ~~Status: Windows is a **secondary-tier** platform for Gradatum. See [RFC-0002](RFC/RFC-0002-cross-platform-support.md) for the full support model. This guide is the operational reference for users running Gradatum on Windows and contributors testing Windows compatibility.~~

---

## 1. Prerequisites

| Requirement | Detail |
|---|---|
| **OS** | Windows 11 22H2 or later (Windows 10 21H2+ tolerated, untested in pre-release) |
| **Hardware (minimum)** | 4 cores, 8 GB RAM, 50 GB free disk (SSD recommended for SQLite WAL) |
| **Long paths** | `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled = 1` recommended (PATH_MAX otherwise capped at 260 chars — see RFC-0002 §5 R13) |
| **Antivirus** | Windows Defender or third-party AV: **exclude the vault root directory and the SQLite WAL/SHM sidecar files from real-time scanning** (mitigates SQLITE_IOERR_LOCK — see RFC-0002 §5 R12 + risks §7.1 R-X7) |
| **Toolchain (contributors)** | Rust 1.75+ stable. For cross-compile from Linux: `apt install -y gcc-mingw-w64-x86-64` + `rustup target add x86_64-pc-windows-gnu`. For native Windows build: MSVC Build Tools or mingw-w64 via MSYS2. |
| **Network (LLM backends)** | Tailscale or VPN access to a Linux LLM server (see §3 below) |

---

## 2. Installation

### 2.1 Pre-built executable (recommended)

Download `gradatum-windows-x86_64.zip` from [Gradatum releases](https://github.com/gradatum/gradatum/releases). Extract to a folder of your choice (e.g., `%LOCALAPPDATA%\gradatum\`). Add the folder to `PATH` if you want `gradatum` available globally.

> **Note**: Pre-built Windows executables ship from `v0.1.0-beta` onwards. Earlier versions require building from source (§2.2).

### 2.2 Build from source

```cmd
git clone https://github.com/gradatum/gradatum.git
cd gradatum
cargo build --release
```

The executable will be at `target\release\gradatum.exe`.

### 2.3 Cross-compile from Linux (contributors)

```bash
sudo apt install -y gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
cargo build --target x86_64-pc-windows-gnu --workspace --release
ls target/x86_64-pc-windows-gnu/release/*.exe
```

---

## 3. Configuration — TOML defaults for Windows

Windows users typically run Gradatum as a client connecting to a Linux LLM server (a self-hosted inference server, any OpenAI-compatible gateway, or a public LLM provider). Below is the recommended starting configuration.

```toml
# %APPDATA%\gradatum\config.toml (or a path you pass via CLI)

[vault]
root = "C:\\Users\\<you>\\Documents\\gradatum-vault"   # or any path < 200 chars

[embed]
# Phase 1 default Windows: HTTP backend against a remote Linux LLM server.
# Avoids the fastembed-cpu local backend (feature-gated off, ort-sys issue with private registry).
backend = "http"
http_url = "http://your-llm-host.local:8432"
fallback_backend = "noop"

[curator]
# Use the LLM gating path for low-confidence heuristic notes.
llm_review_enabled = true
llm_review_backend = "http"
http_url = "http://your-llm-host.local:8080"
confidence_threshold = 0.7
```

> **Note**: `gradatum-engine` (LLM local backend, candle/llama.cpp) is **feature-gated off by default on Windows** in Phase 1. Phase 2+ may enable a candle CPU-only build on Windows; llama.cpp on Windows requires a toolchain pivot (see RFC-0002 §10 — D-X6).

---

## 4. Cert store troubleshooting (rustls vs native-tls)

### Symptom

HTTP requests to a custom LAN hostname (or any custom-cert endpoint) fail with a TLS handshake error: *"invalid peer certificate: unknown issuer"* or similar, even though the same endpoint is reachable from a Linux machine.

### Root cause

Gradatum's HTTP clients (`HttpChat`, `HttpEmbedder`) use `reqwest` with the `rustls-tls` backend by default. `rustls` ships its own root certificate store and **does not** read the Windows native root store. If your Tailscale-injected certificate is only present in the Windows store (and not in `rustls`'s WebPKI store), the handshake fails.

### Workaround

Activate the Cargo feature `windows-native-tls` on `gradatum-chat` and `gradatum-embed`. This switches `reqwest` to `native-tls` (= `schannel` on Windows), which reads the Windows native cert store.

```cmd
cargo install --git https://github.com/gradatum/gradatum --features gradatum-chat/windows-native-tls,gradatum-embed/windows-native-tls
```

Or, if building from source:

```cmd
cargo build --release --features gradatum-chat/windows-native-tls,gradatum-embed/windows-native-tls
```

> **Caveat**: `native-tls` adds a transitive dependency on the Windows DLL `schannel` (already shipped with Windows). The default `rustls-tls` keeps a minimal-DLL footprint preferred for portable executables. Activate `windows-native-tls` only when you've identified a cert store mismatch.

---

## 5. Hardware test checklist (pre-release)

Before each major tag (`v0.1.0-beta`, `v0.1.0`, `v0.2.0`+), the following manual validation is run on a Windows machine.

```text
[ ] gradatum.exe --version -> prints version string.
[ ] gradatum.exe init C:\vault-test -> creates vault directory + config files.
[ ] HTTP sanity: curl -I http://your-llm-host:8432/health (or equivalent endpoint) -> returns 200.
[ ] cargo test --workspace --target x86_64-pc-windows-gnu (skip gradatum-engine, fastembed-cpu features) -> expected core tests PASS.
[ ] No tracing::warn! "nfs_check skipped" pollutes WARN-level logs. (Acceptable in DEBUG.)
[ ] PATH_MAX warning visible at vault boot if LongPathsEnabled = 0 OR vault path > 200 chars.
[ ] SQLite operations under load (insert 1000 notes via CLI) complete without SQLITE_BUSY errors. (R12 + R-X7 mitigation.)
```

If any item fails: open an issue with label `windows-bug` and severity, fix-pack + re-test before tag.

---

## 6. Known issues

See [`KNOWN_ISSUES-WINDOWS.md`](KNOWN_ISSUES-WINDOWS.md) (created upon first reported issue). Empty as of `v0.1.0-alpha` Sprint X-1 closure (2026-05-04).

---

## 7. References

- [`RFC/RFC-0002-cross-platform-support.md`](RFC/RFC-0002-cross-platform-support.md) — full tiered support model and portability rules.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — cross-platform PR checklist.
- [`superpowers/specs/2026-05-04-cross-platform-design.md`](superpowers/specs/2026-05-04-cross-platform-design.md) — internal design spec v2 (post-council).

---

*Maintained by Gradatum maintainers. Pull requests improving this guide are welcome.*
