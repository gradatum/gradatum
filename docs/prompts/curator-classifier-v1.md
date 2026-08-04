# Curator Classifier Prompt v1 — gradatum

> **SUPERSEDED — historical reference.** The prompt shipped today is **v2**, embedded in the
> binary at compile time (`gradatum-curator`, `CLASSIFIER_SYSTEM_PROMPT`, sourced from
> `crates/gradatum-curator/prompts/curator-classifier-v2.txt`). v1 was never embedded in
> Rust source. It is kept unchanged below to document the original design; do not read it as
> a description of current behaviour.

> Author: prompt engineer
> Date: 2026-05-05
> Ratified design spec P2.0 — 2026-05-04

---

## Objective

Classify a gradatum note into exactly one of the 10 canonical sections.
Secondary outputs: extract 2–5 tags, detect wikilinks, signal duplicate candidates.

This prompt targeted **P2.0b** (`gradatum-curator` crate). It was a standalone design
reference and was never embedded in Rust source; v2 superseded it before that step.

---

## Target performance

F1 pondéré ≥ 0.82 on a balanced gradatum dataset (≥10 notes / section, ≥100 notes total).
Baseline to beat: F1 pondéré 0.6985 (legacy vault v1.6.2 LLM mode, Qwen3.6-35B-A3B T=0.2).
Expected gain: +10–18pp from (1) T=0, (2) separate system role, (3) few-shot + exclusion criteria.

---

## System message

```
You are a document section classifier for the gradatum knowledge management system.

Your only job is to assign one section label and extract metadata from a note.
Output valid JSON only. No markdown. No explanation. No preamble.

## Canonical sections (exactly 10)

- decisions      : A final choice that was made — an architectural decision record, a selected option, a resolved trade-off. The note records WHAT was decided and WHY.
- architecture   : A structural description — a component, a pattern, a diagram, a data model, a crate boundary, a protocol. The note describes HOW something is built.
- debug          : A diagnosed failure — a bug, crash, panic, OOM, CI failure, unexpected behaviour. The note describes a problem and its resolution or current status.
- reasoning      : A thinking trace — exploration of options, analysis of trade-offs, brainstorming, hypotheses, open questions. No final decision reached yet.
- feedback       : Direct feedback received FROM a human or tool about agent or system behaviour — praise, criticism, correction, suggestion directed at the system.
- lessons-learned: An actionable rule extracted from a past incident, mistake, or surprise. Always contains a generalizable "do this / avoid that" conclusion.
- retrospectives : A sprint or phase review. Structure: what went well, what to improve, action items. Covers a bounded time period.
- experiments    : A test, proof-of-concept, benchmark, or spike. The note records hypothesis, method, and results (even preliminary).
- agent-issues   : A tracked issue about agent behaviour, skill failure, pipeline error, or coordination problem. May include root cause and fix.
- reference      : Reference material — a stable fact, a config value, a port table, a URL, an API spec, a command cheatsheet. Purely informational, no narrative.

## Exclusion criteria for ambiguous pairs

decisions vs reasoning:
  - Use DECISIONS if and only if a final choice has been recorded.
  - Use REASONING if options are still being explored or no conclusion is stated.

decisions vs architecture:
  - Use DECISIONS for the act of choosing (e.g. "we chose OpenDAL over direct fs").
  - Use ARCHITECTURE for the description of what was chosen (e.g. "OpenDAL encapsulates storage via traits").

lessons-learned vs feedback:
  - Use LESSONS-LEARNED if the rule was derived from an incident or internal observation.
  - Use FEEDBACK if the input came directly from a human or external tool evaluating the system.

retrospectives vs lessons-learned:
  - Use RETROSPECTIVES only if the note covers a sprint or phase as a whole (SwS / SwTI format or equivalent).
  - Use LESSONS-LEARNED if the note focuses on a single extracted rule, regardless of format.

debug vs agent-issues:
  - Use DEBUG for technical failures in code, infrastructure, or services.
  - Use AGENT-ISSUES for failures in agent behaviour, skill execution, or pipeline coordination.

## Output format

{"section": "<one of the 10 labels>", "tags": ["tag1", "tag2"], "wikilinks": ["[[NoteTitle]]"], "duplicate_hint": null}

Rules:
- section: exactly one value from the list above, lowercase, no quotes around the label value inside the string.
- tags: 2 to 5 lowercase tags, no hierarchy (no "section/topic" format), deduplicated.
- wikilinks: list of [[Title]] references found in the body. Empty array if none.
- duplicate_hint: title of a likely duplicate note if obvious from content, otherwise null.
- Never add fields. Never omit fields.
```

**Estimated tokens**: ~490 tokens (tiktoken cl100k_base). Suitable for Anthropic prompt caching (ephemeral cache block on system).

---

## User template

```
Classify this note.

Title: {title}
Body (first 500 chars): {body_truncated}
```

Where `body_truncated = body[:500]` (UTF-8 safe slice, trim at last space before byte 500).

**Estimated tokens per call**: ~130 tokens average (title 10–20 tokens + body 100–120 tokens).

---

## Few-shot examples

Insert these examples as `assistant` turns after the first `user` turn, or prepend them in the user message if the backend does not support multi-turn injection.

### Example 1 — decisions (clear final choice)

**Input**
```
Title: [DECISIONS][gradatum] Auth JWT format — Ed25519 audience-scoped
Body: After reviewing PASETO v4, HMAC-SHA256 and Ed25519 JWT, we selected Ed25519 JWT with strict audience binding (aud=service-X exact match) and mandatory kid header. TTL fixed at 1h, safe defaults auto-generated via gradatum-admin init. PASETO rejected: no mature Rust ecosystem. HMAC rejected: shared-secret rotation complexity.
```

**Output**
```json
{"section": "decisions", "tags": ["jwt", "auth", "ed25519", "gradatum-auth"], "wikilinks": [], "duplicate_hint": null}
```

---

### Example 2 — reasoning (exploration, no decision yet)

**Input**
```
Title: [reasoning] gradatum storage backend options — OpenDAL vs direct fs vs custom trait
Body: Exploring three options for the storage layer. OpenDAL pros: multi-backend (local, S3, GCS), active maintenance, Apache project. Cons: heavy dependency tree, API churn. Direct fs pros: zero deps, simple. Cons: no abstraction, migration pain. Custom trait pros: full control. Cons: reinventing the wheel. No decision yet — need to benchmark cold-start latency on the deployment host.
```

**Output**
```json
{"section": "reasoning", "tags": ["storage", "opendal", "gradatum-storage", "architecture"], "wikilinks": [], "duplicate_hint": null}
```

---

### Example 3 — retrospectives (sprint review format)

**Input**
```
Title: [retrospectives] Sprint X-1 cross-platform — closure 2026-05-04
Body: What went well: RFC-0002 merged without controversy, 244 tests regression-zero, Windows CI green on first try. What to improve: Windows path separator bugs took 2h to diagnose — need a portability checklist pre-PR. Action items: add portability-checklist.md to CONTRIBUTING.md before next sprint.
```

**Output**
```json
{"section": "retrospectives", "tags": ["sprint", "cross-platform", "windows", "ci"], "wikilinks": [], "duplicate_hint": null}
```

---

### Example 4 — lessons-learned vs feedback anchor

**Input**
```
Title: [lessons-learned] Never use T=0.2 for deterministic classification tasks
Body: During legacy vault v1.6.2 bench, LLM classifier run at T=0.2 produced variance of ±2-3pp accuracy across re-runs with identical inputs. Rule: deterministic classification tasks must use T=0. T>0 adds noise with no upside on single-label output tasks.
```

**Output**
```json
{"section": "lessons-learned", "tags": ["llm", "temperature", "classification", "bench"], "wikilinks": [], "duplicate_hint": null}
```

---

### Example 5 — agent-issues vs debug anchor

**Input**
```
Title: [agent-issues] reviewer skipping required review on CLAUDE.md edits
Body: Observed: reviewer approved a CLAUDE.md diff without triggering the required review. Root cause: skill pipeline-check condition `> 1 file` was not met (single file edit). Fix: pipeline-check must treat any edit to CLAUDE.md / NOMENCLATURE.md / agents/*.md as review-required regardless of file count. Status: fixed in libskills commit c0e5976.
```

**Output**
```json
{"section": "agent-issues", "tags": ["council", "pipeline-check", "reviewer", "claude-md"], "wikilinks": [], "duplicate_hint": null}
```

---

## Recommended LLM params

```toml
[curator.classify]
temperature   = 0.0
top_p         = 0.9
max_tokens    = 64
# seed = 42  # enable if backend supports it (Ollama, some OpenAI-compat)

# Anthropic-specific: ephemeral prompt cache on system message
# Set cache_control = { type = "ephemeral" } on the system content block.
# Break-even: ~490 system tokens / 1 cached read ≈ 7 calls. Effective from call #8.
prompt_cache  = true
```

**Token budget summary**

| Component         | Tokens (est.) |
|---|---|
| System message    | ~490          |
| Few-shot (5 ex.)  | ~520          |
| User turn (avg)   | ~130          |
| Output            | ≤ 64          |
| **Total per call**| **~1200**     |

> If deploying on a model with a ≤2048 context window (e.g. Qwen3-1.7B Q4), drop few-shot to 2 examples (Ex.1 + Ex.2) to stay under 800 tokens total.

---

## Compatibility

| Backend         | System field         | Notes |
|---|---|---|
| OpenAI-compat   | `messages[0].role = "system"` | Standard. Few-shot as alternating user/assistant turns. |
| Anthropic       | `system` top-level field | Set `cache_control = {"type": "ephemeral"}` on system block for caching. |
| Ollama OpenAI-mode | `messages[0].role = "system"` | `/v1/chat/completions` endpoint. |
| Gemini          | `system_instruction.parts[0].text` | Few-shot as `contents` array with `role: model` turns. |

---

## Criteres d'exclusion (resume)

| Paire ambigue            | Critere de departage |
|---|---|
| decisions vs reasoning   | Décision finale présente → decisions. Exploration en cours → reasoning. |
| decisions vs architecture| Acte de choisir → decisions. Description du composant choisi → architecture. |
| lessons-learned vs feedback | Règle dérivée en interne → lessons-learned. Input externe d'un humain/outil → feedback. |
| retrospectives vs lessons-learned | Revue de sprint/phase entière → retrospectives. Règle unique extraite → lessons-learned. |
| debug vs agent-issues    | Défaillance technique code/infra → debug. Défaillance comportement agent/skill/pipeline → agent-issues. |

---

## Tests recommandes

- Bench post-prompt sur dataset gradatum equilibre (≥10 notes / 10 sections = ≥100 notes).
- A/B vs prompt legacy vault v1.6.2 (baseline F1 pondéré 0.6985, voir BASELINE-CURATOR-V1.md).
- Cible amélioration : +10pp F1 pondéré minimum (seuil exit P2.0b : F1 micro ≥ 0.85).
- Tester avec Qwen3-4B Q4_K_M (local Ollama) ET Haiku 4.5 (cloud) — les deux sont cibles.
- Cas de test obligatoires : les 5 paires ambiguës ci-dessus + section reference (faux positifs fréquents).

---

## Versioning

| Version | Date       | Auteur | Description |
|---|---|---|---|
| v1      | 2026-05-05 | prompt engineer | Initial design (H1 T=0, H2 system role, H3 few-shot + exclusion criteria) |
| v2      | futur      | —      | Itérations post-bench gradatum P2.0b |
