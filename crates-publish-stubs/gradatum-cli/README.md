# gradatum-cli

> End-user CLI for reading, writing, and searching notes via the gradatum-server HTTP API.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Usage (Phase 2.0+)

```
gradatum [--server <url>] [--token <bearer>] <command>
```

## Subcommands

```
gradatum write <file.md>                     # Write a note from file
gradatum write --title "My note" --section decisions  # Write from stdin
gradatum read <note-path>                    # Read a note
gradatum search <query>                      # Search (BM25 + semantic fusion)
gradatum list [--section <name>] [--limit N] # List notes
gradatum status                              # Vault status
gradatum tags                                # List tags with frequency
gradatum authors                             # List authors
```

## Configuration

Environment variables:
```
GRADATUM_SERVER_URL=http://127.0.0.1:19090
GRADATUM_BEARER_TOKEN=<jwt>
```

Or config file at `~/.config/gradatum/config.toml`:
```toml
server_url = "http://127.0.0.1:19090"
bearer_token_file = "~/.config/gradatum/token"
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0 (Phase 2.0 implementation)
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0