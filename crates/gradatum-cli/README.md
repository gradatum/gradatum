# gradatum-cli

> End-user CLI for gradatum-server HTTP API — placeholder, not yet implemented.

**Status**: Placeholder — not yet implemented. Binary exits with an error message. `publish = false`: this crate was published once at `0.7.6` and that version remains installable from crates.io, but it has not been republished since. A real implementation is expected to ship alongside the agent runtime; no version is committed to it yet.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-cli` is reserved for a future interactive command-line interface that will
communicate with a running `gradatum-server` instance over HTTP.

No commands are implemented yet. The binary currently exits immediately with:

```
gradatum-cli: not yet implemented
```

Use direct HTTP calls to `gradatum-server` (`/api/v1/...`), or an MCP client pointed at
its native `/mcp` endpoint, in the meantime.

## License

Apache-2.0
