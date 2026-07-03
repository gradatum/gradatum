# gradatum-ingest

Code-ingest pipeline for [gradatum](https://github.com/gradatum/gradatum): parses source files with
tree-sitter and derives code symbols stored as notes in the gradatum vault.

Supports Rust, Python, Bash, TypeScript, and TSX. Symbols are derived deterministically from
source — zero LLM cost.

**Status**: v0.7.6 — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum)

## Feature flags

Language parsers are opt-in. `code-rust` is enabled by default.

| Feature           | Language         | Default |
|-------------------|------------------|---------|
| `code-rust`       | Rust             | yes     |
| `code-python`     | Python           | no      |
| `code-bash`       | Bash             | no      |
| `code-typescript` | TypeScript / TSX | no      |

```toml
[dependencies]
gradatum-ingest = { version = "0.7.6", features = ["code-rust", "code-python"] }
```

## Usage

Used internally by `gradatum-worker` and `gradatum-admin`. Not intended as a standalone library;
the public API surface is intentionally minimal. External consumers should not depend on this
crate directly — signatures may change across minor versions until v1.0.

See the [gradatum repository](https://github.com/gradatum/gradatum) and
[ARCHITECTURE.md](https://github.com/gradatum/gradatum/blob/main/ARCHITECTURE.md) for the
full design.

## License

Apache-2.0 — see [LICENSE](../../LICENSE).
