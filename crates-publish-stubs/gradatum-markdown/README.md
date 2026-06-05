# gradatum-markdown

> Parse and serialize Markdown notes with YAML frontmatter and wikilink extraction.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Functions

```rust
/// Parse a Markdown file (YAML frontmatter + body) into a ParsedNote.
pub fn parse(src: &str) -> Result<ParsedNote, MarkdownError>

/// Serialize a ParsedNote back to on-disk Markdown format.
pub fn write_parsed(note: &ParsedNote) -> String

/// Serialize a full gradatum_core::note::Note to Markdown.
pub fn write(note: &Note) -> String

/// Extract all [[wikilinks]] from a Markdown body.
pub fn wikilinks(body: &str) -> Vec<Wikilink>
```

### Structs

```rust
pub struct ParsedNote {
    pub frontmatter: Frontmatter,
    pub body: String,
}

pub struct Wikilink {
    pub target: String,
    pub alias: Option<String>,
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum MarkdownError {
    MissingFrontmatter,
    FrontmatterParse(serde_yaml::Error),
    InvalidSchemaVersion { found: u32, expected: u32 },
}
```

## On-disk format

```
---
schema_version: 1
vault_id: main
section: decisions
status: live
created: "2026-05-04T11:00:00Z"
---

# Note title

Body with [[wikilinks]] and [[target|alias]] support.
```

## Round-trip guarantee

`parse(write_parsed(parse(x)?))` is semantically equivalent to `parse(x)` (1-cycle idempotence on values, not exact text representation).

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0