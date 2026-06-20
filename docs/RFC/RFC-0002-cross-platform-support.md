# RFC-0002 — Cross-platform support: Linux primary + Windows secondary

> **Status: SUPERSEDED / DEFERRED 2026-06-05** — gradatum targets Linux as the sole
> supported platform; Windows/cross-platform support is deferred indefinitely (no committed
> timeline). The body of this document is preserved for historical reference. The tiered
> support model described here is no longer active; RFC-0002 is superseded by the
> Linux-only stance adopted as of 2026-06-05.

| Field | Value |
|---|---|
| **RFC number** | 0002 |
| **Status** | `superseded` (was `accepted`) |
| **Started** | 2026-05-04 |
| **Resolved** | 2026-05-04 |
| **Tracking issue** | — (Sprint X-1 — pre-public release) |
| **Affected crates** | `gradatum-storage` (`nix` dep `cfg(unix)`), `gradatum-chat` (feature `windows-native-tls`), `gradatum-embed` (feature `windows-native-tls`); applies workspace-wide via portability rules. |
| **Authors** | Gradatum maintainers |

---

## 1. Status

Accepted via maintainer review on 2026-05-04. Three GO-WITH-CAVEATS verdicts; 17 caveats absorbed into the source design spec v2 prior to ratification. No P0 blockers for Sprint X-1; one P0 deferred to Phase 2+ design (toolchain choice for `gradatum-engine` LLM local — see §10 Open questions).

## 2. Motivation

Phase 1 of Gradatum (released 2026-05-04 as `v0.1.0-alpha`, commit `cd11edb`) was scoped explicitly to "Linux dev-only" during design (see Phase 1 spec §0 and `nfs_check.rs:80`). That scoping was correct for the initial self-hosted Forgejo environment, but no formal decision had been made about long-term Windows support.

Audit of Phase 1 code reveals that the workspace is in fact **near-portable**: a single dependency declaration (`nix` in `gradatum-storage/Cargo.toml`) prevents Windows compilation; the actual runtime code is already gated `#[cfg(target_os = "linux")]` with a graceful fallback. Without an explicit policy, Phase 2+ would accumulate Linux-only debt by silent drift, and the pre-release Windows port would degenerate into a rewrite sprint.

This RFC adopts a **B-light tiered support model** for Gradatum: Linux is the primary platform, Windows is a secondary platform anticipated continuously, macOS is a future roadmap item. The constraint is encoded as portability rules, validated by mingw-w64 cross-compile in CI, and exercised by manual runtime testing before each major tag.

## 3. Tiered support matrix

| Tier | Platform | CI compile | CI runtime tests | Pre-release manual validation | Guarantees |
|---|---|---|---|---|---|
| **Primary** | Linux x86_64 (`x86_64-unknown-linux-gnu`) | ✅ Forgejo self-hosted runner | ✅ all tests + benches | — | Full feature set, performance targets per Phase 1 spec, official support, priority issue resolution. |
| **Secondary** | Windows x86_64 (`x86_64-pc-windows-gnu`) | ✅ Cross-compile via mingw-w64 (separate non-blocking job, `continue-on-error: true`) | ❌ No Windows runner | ✅ Manual VM/host test before each major tag (`v0.1.0-beta`, `v0.1.0`, `v0.2.0`+) | Compiles cleanly, core tests pass, portable executable shipped, NFS check warn-skipped, `gradatum-engine` optional/feature-gated. |
| **Future roadmap** | macOS (Apple Silicon + Intel x86_64) | — | — | — | Possible v0.2.0+. The codebase remains portable by design (preference for `cfg(unix)` over strict `cfg(target_os = "linux")` where applicable). No promises, no validation today. |

## 4. Exit criteria — Secondary → Primary tier promotion

No predefined timeline. Promotion to primary tier requires all three:

- **(a)** A dedicated Windows maintainer active over two consecutive release cycles.
- **(b)** A Windows CI runner provisioned (self-hosted or GitHub Actions) financing the runtime matrix.
- **(c)** Zero open `windows-bug`-labeled P0/P1 issues over two consecutive release cycles.

## 5. Portability rules (mandatory for all PRs from Phase 2+)

Conceptual fault line: **Linux ≈ macOS at 90%** (POSIX); **Windows differs at 100%** (path separators, native ACLs, registry, line endings, dynamic linker, syscalls). Most useful `cfg` gates in practice are `cfg(unix)` (Linux + macOS + BSD) rather than strict `cfg(target_os = "linux")`.

| # | Rule | Rationale |
|---|---|---|
| R1 | **Unix-general native deps** (also support macOS+BSD) → `[target.'cfg(unix)'.dependencies]`. Examples: `nix` (POSIX wrapper), direct `libc`. | Macroadmap preparation without Linux cost. |
| R2 | **Strict Linux-only deps** → `[target.'cfg(target_os = "linux")'.dependencies]`. Examples: `inotify`, `fanotify`, `caps`, `cgroups-rs`, `landlock`. | Values absent or incompatible on macOS+Windows. |
| R3 | **Linux-only constants/syscalls in code** (`NFS_SUPER_MAGIC`, `EPOLL*`, `IORING_OP_*`) → function gated `#[cfg(target_os = "linux")]` with fallback `#[cfg(not(target_os = "linux"))]`. Selection criterion for the fallback: return `Ok(())` with `tracing::debug!` if the function is purely informative/diagnostic (e.g. NFS check); return a typed `Error` if the functionality is expected by the caller (e.g. an exclusive syscall that materializes a guarantee). Document the choice in the `//! Behavior on non-Linux:` comment (R10). | Compiles everywhere, degrades cleanly without ambiguity for the reviewer. |
| R4 | **No hardcoded absolute paths**: `/tmp`, `/etc`, `/proc`, `/dev`, `/var`. Use `std::env::temp_dir()`, `dirs::config_dir()`, `dirs::data_dir()`. | Windows lacks these paths; macOS has different conventions (`~/Library/`). |
| R5 | **Path joining**: `Path::join` / `PathBuf::push` exclusively. Never format strings with `/`. | Windows partial POSIX-compatibility on `/` is fragile; norm is `\`. |
| R6 | **Unix file permissions**: portable `std::fs::Permissions` + `PermissionsExt` (mode `0o600`) under `#[cfg(unix)]` with Windows fallback. | Windows uses ACLs, not `mode_t`. |
| R7 | **Markdown / YAML / JSON line endings**: emit LF by default. Reading tolerates CRLF. | Obsidian + cross-platform standard editors compliance. |
| R8 | **No invoked shell host (without exception)**: `bash -c`, `sh -c`, `cmd.exe /c`, `pwsh.exe -c` are absolutely forbidden. Allowed: `std::process::Command` with a **fixed, named target binary** (e.g., `Command::new("git")`, never `Command::new("sh")`). If OS-aware logic is needed (different binary path), use explicit `cfg(unix)` / `cfg(windows)` branches and an R10 comment. | Minimal shell compatibility + impossible argument injection. |
| R9 | **Test tempdirs**: `tempfile::TempDir` (already dev-dep in `gradatum-storage`). No hardcoded absolute path in tests. | Portable natively. |
| R10 | **Documentation**: `cfg` comments + `//! Behavior on non-Linux: ...` in any module containing OS-aware branches. | Auditors can trace portability via grep. |
| R11 | **Read TOML/JSON/YAML: silently strip UTF-8 BOM**. Editors like MSVC, Notepad, VS Code (on Windows with auto-detected encoding) emit BOM `\xEF\xBB\xBF` at the head of files. `toml::from_str` and `serde_json::from_str` may panic or fail silently. Implementation: a `strip_bom(s: &str) -> &str` helper called before parsing any external configuration file (vault config, override TOML, YAML frontmatter for notes edited on Windows). | Tolerates Windows-edited files without breaking parse. |
| R12 | **PRAGMA `busy_timeout=5000ms` mandatory on every SQLite `Connection`**. Linux AND Windows. SQLITE_BUSY is more frequent on Windows (mandatory locking + AV scanning WAL/SHM) → 5s wait before error. **Phase 1 status**: already applied in `gradatum-queue/src/queue.rs:74` and `gradatum-index/src/sqlite.rs:85` + `pragmas.rs` test assertion. R12 documents the rule for any new `Connection` introduced Phase 2+. | Prevents spurious SQLITE_BUSY in multi-process scenarios on Windows. |
| R13 | **Runtime audit of Windows PATH_MAX 260 chars**. Windows without opt-in `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled = 1` silently truncates paths > 260 chars (`ERROR_PATH_NOT_FOUND` without `ENAMETOOLONG`). Implementation: at Vault boot under `cfg(windows)`, check the registry key + `tracing::warn!` if disabled or if vault root path approaches 200 chars. | Prevents silent failures on deeply nested Obsidian vaults. |

Special case — `nfs_check`: existing Phase 1 code (`crates/gradatum-storage/src/nfs_check.rs:28-89`) is already R3-compliant: `cfg(target_os = "linux")` on the magic constant + `tracing::warn!` fallback on other OSes. The only change required (R1) is the dependency declaration `nix` migrated to `[target.'cfg(unix)'.dependencies]` in `crates/gradatum-storage/Cargo.toml`.

## 6. Validation strategy

### 6.1 Continuous compile-time validation (CI)

Cross-compile from Linux runner to `x86_64-pc-windows-gnu` via mingw-w64. Setup (one-shot, on the build runner): `apt install -y gcc-mingw-w64-x86-64` + `rustup target add x86_64-pc-windows-gnu`. Job `windows-build` separate, non-blocking (`continue-on-error: true`), default features only (`fastembed-cpu` excluded). Detects ~80% of portability problems (compile-time). Ships an `*.exe` artifact uploaded automatically.

Dependency caveats (audited Sprint X-1 X1.5):

- `rusqlite` (in `gradatum-index`, `gradatum-queue`) — feature `bundled` already active (workspace `Cargo.toml:90`). Cross-mingw OK.
- `reqwest` (in `gradatum-chat::HttpChat`, `gradatum-embed::HttpEmbedder`) — `default-features = false, features = ["rustls-tls", "json"]` already set (workspace `Cargo.toml:96`). Cross-mingw OK.
- `ort-sys` (T08 `fastembed-cpu`) — feature-gated off, excluded from cross-compile (private registry ureq v3 vs v2 bug).

### 6.2 Pre-release manual runtime validation

Triggered before each major tag (`v0.1.0-beta`, `v0.1.0`, `v0.2.0`+). Procedure documented in `docs/WINDOWS-GUIDE.md` (separate document). Minimum hardware: 4 cores / 8 GB RAM / 50 GB free / Windows 11 22H2 / Tailscale or VPN access to a Linux LLM server.

If runtime test reveals bugs: fix-pack + re-cross-compile + retest. Not a blocker on the general process, just pre-release discipline.

### 6.3 Build & release

`scripts/release-windows.sh` (delivered Sprint Phase 2 PH2.Y) wraps `cargo build --target x86_64-pc-windows-gnu --release --workspace` + strip + `dist/gradatum-windows-x86_64.zip` (exe + LICENSE-APACHE + README.md + CHANGELOG.md) + SHA256 checksums. Out-of-scope: signed packaging (MSI, Authenticode) — post-v0.1.0 roadmap.

## 7. Build & supplementary documents

- **`docs/WINDOWS-GUIDE.md`** — operational user/contributor guide for Windows: Windows-friendly TOML defaults, hardware test checklist, rustls cert store troubleshooting, `KNOWN_ISSUES-WINDOWS.md` pointer.
- **Workspace feature `windows-native-tls`** (opt-in OFF default) on `gradatum-chat` and `gradatum-embed`: switches `reqwest` from `rustls-tls` to `native-tls` (= `schannel` on Windows, accesses Windows native root store). Activated explicitly by Windows users in corporate environments with proxy/custom certs not injected into rustls.

## 8. What is **not** modified

- **Phase 1 specification** (internal design document, not published) — frozen, historical fidelity. RFC-0002 opens the new era starting Phase 2.
- **`nfs_check.rs` historical comment** "Phase 1 = Linux local development only" — preserved as historical witness, plus a mandatory pointer line to RFC-0002 added in the same commit (`X1.4`).
- **`ARCHITECTURE.md` and `DEPENDENCIES.md`** were updated in Sprint X-1 (commit `d078068`) to reflect the tiered support model and the conditional dependencies (`nix` `cfg(unix)`, optional `windows-native-tls` feature). No architectural breaking change; documentation alignment only.
- **`MAINTAINERS.md` / `GOVERNANCE.md`** — not affected (no governance change).

## 9. Rollback strategy

If Windows debt becomes unmanageable (unbalanced fix Linux/Windows ratio, excessive cfg-gate complexity), an explicit RFC-0002 amendment downgrades Windows to `experimental` tier (compile only, no pre-release validation). No silent removal.

## 10. Open questions (Phase 2+)

- **D-X6 — Windows toolchain for `gradatum-engine` LLM local** (council review caveat C-1, P0 deferred): choice between `x86_64-pc-windows-gnu` (mingw, current) and `x86_64-pc-windows-msvc`. llama.cpp produces `.lib` MSVC files incompatible with mingw linkage. Three options to arbitrate in Phase 2+ design: (a) MSVC pivot for Windows toolchain; (b) candle CPU-only on Windows; (c) `gradatum-engine` feature `local-llm` OFF Windows by default (HTTP-only against remote Linux server). Decision target: Phase 2+ design (~Sept-Nov 2026).

## 11. References

- Source design spec v2: internal design document (not published), commit `ad33b80`.
- Phase 1 specification (frozen): internal design document (not published), commit `0616d98`.
- Phase 1 backend plan: internal plan document (not published).
- Sprint X-1 backend plan: internal plan document (not published).
- RFC-0001 trait stability tiers: [`RFC-0001-versioning-gradatum-core.md`](RFC-0001-versioning-gradatum-core.md).
- Windows operational guide: [`../WINDOWS-GUIDE.md`](../WINDOWS-GUIDE.md).
- Phase 1 LIVE tag `v0.1.0-alpha` commit `cd11edb` — 14/14 deliverables, 244 tests PASS Linux.
