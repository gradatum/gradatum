# RFCs (Request for Comments)

Index of design decisions and structural changes to Gradatum.

RFCs are required for any change to `gradatum-core` public traits, workspace structure, versioning policy, governance, license, or code of conduct. See [`GOVERNANCE.md`](../../GOVERNANCE.md) §"RFC process" for the workflow.

## Current RFCs

| Number | Title | Status | Date | Affected crates |
|---|---|---|---|---|
| [RFC-0001](RFC-0001-versioning-gradatum-core.md) | Trait stability tiers and versioning for `gradatum-core` | `accepted` | 2026-05-03 | `gradatum-core`, all consumers |
| [RFC-0002](RFC-0002-cross-platform-support.md) | Cross-platform support: Linux primary + Windows secondary | `accepted` | 2026-05-04 | `gradatum-storage`, `gradatum-chat`, `gradatum-embed`; workspace-wide portability rules |
| [RFC-0003](RFC-0003-http-api-surface-and-mcp-integration.md) | HTTP API surface and MCP integration topology | `accepted` | 2026-05-04 | `gradatum-server`, `gradatum-mcp-stub` |
| [RFC-0005](RFC-0005-self-improvement.md) | Self-improvement: agent reflexive capability building from classified knowledge base | `proposal` | 2026-05-05 | new crate `gradatum-self-improvement` (Phase 3+ target) |

## Process

1. **Draft:** Author creates a PR adding a new RFC markdown file, numbered sequentially.
2. **Discussion:** Minimum 7 days for maintainer comment. Non-maintainers may comment; only maintainers block/approve.
3. **Resolution:** Marked `accepted`, `postponed`, or `rejected`. Rejected RFCs remain in the repo for historical reference.
4. **Implementation:** Tracked issue created; implementation PRs cross-reference the RFC number.

**RFC numbering is monotonic and never reused.**

## Templates and guidance

- Use [`RFC-TEMPLATE.md`](../../RFC-TEMPLATE.md) as the skeleton.
- Focus on **why** (motivation) and **what** (design). Implementation follows.
- Include alternatives, drawbacks, and unresolved questions.
- Aim for human and AI clarity: explicit constraints, numbered rules, decision matrices.

## Historical RFCs (future)

As the project grows, accepted and rejected RFCs will accumulate here.
