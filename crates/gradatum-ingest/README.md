# gradatum-ingest

Code-ingest pipeline for [gradatum](https://github.com/gradatum/gradatum): parses source files with tree-sitter and derives code symbols stored as notes in the gradatum vault.

Supports Rust, Python, Bash, TypeScript, and TSX. Symbols are derived deterministically from source — zero LLM cost.

## Usage

Used internally by `gradatum-admin code ingest` and `gradatum-admin code update`. Not intended as a standalone library; the public API surface is intentionally minimal.

See the [gradatum repository](https://github.com/gradatum/gradatum) and [ARCHITECTURE.md](https://github.com/gradatum/gradatum/blob/main/ARCHITECTURE.md) for the full design.

## License

Apache-2.0 — see [LICENSE](../../LICENSE).
