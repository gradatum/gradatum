# Guide C — crates.io & build from source

Two related paths: pulling individual crates as a library dependency, and building the
workspace binaries yourself. Use this guide if you're on arm64 (not covered by
[Guide B](B-install-binaries.md)'s pre-built binaries), need the test suite, or want to embed
gradatum crates in your own Rust project. Platform support in general (macOS out of scope,
Windows via Docker): [docs/DEPLOYMENT.md § Platform support](../DEPLOYMENT.md#platform-support).

---

## Option — crates.io (library use)

The gradatum workspace has **31 members**, of which **26 are publishable** to crates.io (5 carry
`publish = false`: `gradatum-bench`, `gradatum-cli`, `gradatum-mcp-stub`, `index-parity-tests`,
`v1-parity-tests`). These are **source releases**; from `1.0.0` their public APIs follow SemVer
strict.

```bash
cargo add gradatum-core     # core types, note model, ACL
cargo add gradatum-vault    # vault read/write/lifecycle
cargo add gradatum-search   # hybrid search (BM25 + semantic)
cargo add gradatum-embed    # dense embeddings
cargo add gradatum-ingest   # code index (tree-sitter)
```

`cargo add gradatum` gives you the meta-crate (re-exports). Individual crates
(`gradatum-core`, `gradatum-vault`, …) are the intended entry points. Full list:
[crates.io/crates/gradatum](https://crates.io/crates/gradatum).

> **`gradatum-cli` is not part of the `2.0.0` release.** It was published once at `0.7.6` —
> that version remains on crates.io and is installable — but it has not been republished
> since and has no implementation (`main.rs` is a stub that prints an error and exits). The
> GitHub release archives don't ship it either (see [Guide B](B-install-binaries.md)). A real
> CLI is expected to ship alongside a future agent runtime; no version is committed to it yet.

---

## Option — build from source

**Prerequisites:** Rust stable (MSRV 1.91, pinned by `rust-toolchain.toml`), a C linker
(`gcc` / `clang`), and SQLite 3.x development headers (`libsqlite3-dev` on Debian/Ubuntu).

```bash
git clone https://github.com/gradatum/gradatum.git
cd gradatum

cargo build --workspace --release --features gradatum-engine/serve

# Optional
cargo test --workspace
```

`gradatum-engine`'s binary carries `required-features = ["serve"]`
(`crates/gradatum-engine/Cargo.toml`), and `serve` is **not** a default feature —
**`--workspace` alone does not enable it.** Verified: `cargo build --workspace --release` without
the flag above finishes (exit 0) and produces `gradatum-admin`, `gradatum-cli`,
`gradatum-gateway`, `gradatum-mcp-stub`, `gradatum-server`, `gradatum-worker` — no
`target/release/gradatum-engine` (the crate's library target still builds silently as part of
the same command; only the binary is skipped). The `--features gradatum-engine/serve` flag
above builds the whole workspace, engine included, in one pass; building it standalone instead
is `cargo build -p gradatum-engine --features serve --release`.

Binaries land in `target/release/` — **not yet running.** Nothing is installed to a system
path, no data directory exists, nothing has been started. To bring them up, either:

- the **[Automated install (systemd)](#automated-install-systemd)** section immediately below
  (production path: creates the `gradatum` user, wires systemd units, initializes the vault), or
- the same manual bring-up sequence pre-built binaries follow —
  [docs/DEPLOYMENT.md § Running without systemd](../DEPLOYMENT.md#running-without-systemd)
  (`gradatum-admin init`, launch order, health check) — the binaries themselves are identical
  either way, only their origin (built vs. downloaded) differs.

arm64 is not covered by pre-built binaries — this is the only path on that platform. (macOS and
Windows: see [docs/DEPLOYMENT.md § Platform support](../DEPLOYMENT.md#platform-support) —
neither is a build-from-source target here.)

### Automated install (systemd)

Same scripts as [Guide B](B-install-binaries.md#systemd), with `--build` to compile as part of
the install:

```bash
sudo bash scripts/install-gradatum-services.sh --build
sudo bash scripts/install-gradatum-services.sh --build --with-engine --with-gateway
```

For subsequent deploys (binary swap without re-init):

```bash
bash scripts/deploy-gradatum-local.sh --build
bash scripts/deploy-gradatum-local.sh --build --engine
```

See `packaging/systemd/README.md` for the unit reference, and
[docs/DEPLOYMENT.md](../DEPLOYMENT.md) for engine multi-instance topology and upgrade ordering.

---

## Agent skills (optional, either path)

The paths above install Gradatum itself. The agent-facing skills are distributed separately, as
the **[gradatum-skills](https://github.com/gradatum/gradatum-skills)** plugin (Apache-2.0) — see
[Guide D — MCP & Studio](D-mcp-and-studio.md#agent-skills) for what they do and how to install
them.
