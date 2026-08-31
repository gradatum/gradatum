# gradatum-ingest

Code-ingest pipeline for [gradatum](https://github.com/gradatum/gradatum): parses source files with
tree-sitter and derives code symbols stored as notes in the gradatum vault.

Supports Rust, Python, Bash, TypeScript, and TSX. Symbols are derived deterministically from
source — zero LLM cost.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
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
gradatum-ingest = { version = "2.1.0", features = ["code-rust", "code-python"] }
```

## Usage

Used internally by `gradatum-admin`. Not intended as a standalone library;
the public API surface is intentionally minimal. External consumers are advised not to depend on
this crate directly — it is an implementation detail of the gradatum stack.

See the [gradatum repository](https://github.com/gradatum/gradatum) and
[ARCHITECTURE.md](https://github.com/gradatum/gradatum/blob/main/ARCHITECTURE.md) for the
full design.

## License

Apache-2.0 — see [LICENSE](https://github.com/gradatum/gradatum/blob/main/LICENSE).
