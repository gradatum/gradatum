# gradatum-markdown

> Parse and serialize Markdown notes with YAML frontmatter and wikilink extraction.

**Status**: v1.0.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-markdown` handles the on-disk Markdown format used by gradatum notes. Each note
is a `.md` file with a YAML frontmatter block followed by a Markdown body.

Functions:

- `parse(src)` — parses frontmatter + body into a `ParsedNote`.
- `write_parsed(note)` — serializes a `ParsedNote` back to the on-disk format.
- `write(note)` — serializes a full `gradatum_core::note::Note`.
- `wikilinks(body)` — extracts `[[target]]` and `[[target|alias]]` links from a body string.

Round-trip invariant: `parse(write_parsed(parse(x)?)) == parse(x)`.

## Usage

```toml
[dependencies]
gradatum-markdown = "1.0.0"
```

```rust
use gradatum_markdown::{parse, wikilinks};

let parsed = parse(src)?;
println!("section: {}", parsed.frontmatter.section);

let links = wikilinks(&parsed.body.markdown);
for link in links {
    println!("links to: {}", link.target);
}
```

## On-disk format

```markdown
---
schema_version: 1
vault_id: main
section: decisions
status: live
created: "2026-05-04T11:00:00Z"
---

# Note title

Body with [[wikilinks]].
```

## License

Apache-2.0
