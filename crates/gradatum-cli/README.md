# gradatum-cli

> End-user CLI for gradatum-server HTTP API — placeholder, not yet implemented.

**Status**: Placeholder — not yet implemented. Binary exits with an error message. `publish = false`: this crate was published once at `0.7.6` and that version remains installable from crates.io, but it is **not republished at `1.0.0`**. A real implementation is expected with the agent runtime at `2.0.0`.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-cli` is reserved for a future interactive command-line interface that will
communicate with a running `gradatum-server` instance over HTTP.

No commands are implemented yet. The binary currently exits immediately with:

```
gradatum-cli: not yet implemented
```

Use `gradatum-mcp-stub` (MCP stdio adapter) or direct HTTP calls to `gradatum-server`
in the meantime.

## License

Apache-2.0
