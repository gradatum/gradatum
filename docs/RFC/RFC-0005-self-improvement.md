# RFC-0005 — Self-improvement: agent reflexive capability building from classified knowledge base

| Field | Value |
|---|---|
| **RFC number** | 0005 |
| **Status** | `proposal` |
| **Started** | 2026-05-05 |
| **Resolved** | — |
| **Tracking issue** | — (backlog Phase 3+, post-rc.1, requires ≥200 notes runtime + 6 months observation period) |
| **Affected crates** | new crate `gradatum-self-improvement` (proposal); reads `gradatum-vault`, `gradatum-search`, `gradatum-embed`, `gradatum-curator`; produces drafts to separate space |
| **Authors** | Gradatum maintainers |
| **Phase target** | Phase 3+ (post `v0.1.0` stable + ≥6 months D5 dogfooding) |

---

## Status note

This RFC is a **future feature proposal** captured 2026-05-05 during P2.0c brainstorming. It is **NOT in scope** for `v0.1.0-alpha.4` (Phase 2.0c — runtime wiring), `v0.1.0-rc.1` (Phase 2.1 — `migrate-from-v0` + D5 cohabitation), or `v0.1.0` stable. It targets Phase 3+ once the gradatum knowledge base has matured to ≥200 classified notes and the agent has accumulated ≥6 months of real-world traces.

The proposal is included in the repository now to:
- Anchor the long-term direction for the gradatum project
- Inform Phase 2 architectural decisions where they may impact future feasibility (taxonomy stability, note metadata richness, drift handling)
- Serve as backlog reference for community contributors interested in agent self-improvement primitives

---

## 1. Definition

`self-improvement` is an autonomous agent function that, from an internal knowledge base of classified and categorized notes, continuously identifies:

- **Recurring routines** the agent repeats without having formalized the procedure
- **Repeated gaps** where the same missing information is re-searched multiple times
- **Automation opportunities** where a formal procedure would be more reliable and faster than free-form reasoning

On this basis, the function produces **drafts of capability artifacts** (formal procedures, documented patterns, consolidated references) that are submitted to an external validation mechanism before integration into the agent's capabilities.

The agent learns from its own traces, not from an external corpus. This is a reflexive loop: the agent becomes more performant on what it already does, without claiming to learn what it has never done.

## 2. Knowledge base prerequisites

`self-improvement` is not applicable to any base. Four properties are required.

### 2.1 Structured notes

Each note in the base has at minimum:

| Attribute | Role |
|---|---|
| Unique stable identifier | immutable reference |
| Creation and update timestamps | temporal reconstruction |
| Textual body | exploitable semantic content |
| Multi-axis classification | filter and grouping |
| Optional incoming and outgoing links | context propagation |
| Lifecycle status | distinction of active, superseded, obsolete notes |

### 2.2 Stable and unambiguous taxonomy

The classification applied to notes must be:

- **Stable** over time — classification codes do not change every month
- **Unambiguous** — two classifying instances applying the grid converge on the same note
- **Multi-axis** — at minimum three orthogonal axes: nature of activity, form of intellectual engagement, document type
- **Robust to proper nouns** — named tools, projects, or persons live in a separate attribute, never in classification codes themselves

Without these properties, the function produces noise.

### 2.3 Minimum volume

The algorithm relies on recurrence detection. A minimum volume is required for patterns to emerge without false positives:

- **Absolute floor**: ~50 notes
- **Comfortable volume**: ~200 notes
- **Optimal volume**: 500 notes or more

Below the floor, the function runs but emits few reliable candidates.

### 2.4 Temporal distribution

Notes must be timestamped and distributed over time. Three notes created on the same day on the same theme do not constitute recurrence — they constitute a single session.

Validation rule: a pattern is eligible only if its source notes are distributed over at least three distinct sessions separated by at least 24 hours.

## 3. Functional architecture

The function decomposes into three chained phases, independent in execution cadence.

```
+----------------------------------------------------------------+
|                       self-improvement                         |
+----------------------------------------------------------------+
                              |
        +---------------------+---------------------+
        v                     v                     v
   +---------+           +----------+           +----------+
   | Phase 1 |  ------>  | Phase 2  |  ------>  | Phase 3  |
   | Observe |           | Analyze  |           | Propose  |
   +---------+           +----------+           +----------+
        |                     |                     |
        v                     v                     v
   note traversal        pattern clusters     capability drafts
   classification        detected             for external
   enrichment            scoring criteria     validation
        |                     |                     |
        +---------------------+---------------------+
                              |
                              v
                    +-------------------+
                    | external          |
                    | validation        |
                    | (out of scope)    |
                    +-------------------+
                              |
                              v
                  integrated capabilities
```

Recommended cadences:

- **Phase 1 (Observe)**: continuous, reactive to new notes, complemented by short periodic sweep
- **Phase 2 (Analyze)**: daily
- **Phase 3 (Propose)**: weekly, more compute-intensive

## 4. Phase 1 — Observe

### 4.1 Objective

Guarantee that each note in the base is correctly classified according to the taxonomy in force. Without this clean foundation, subsequent phases are not exploitable.

### 4.2 Activities

1. **Detection of new or modified notes** by content hash comparison
2. **Deterministic classification first**: named entity extraction by rules, keywords, detectable formats (file extension, particular syntax, presence of structured elements)
3. **Model-based classification next**: call to a lightweight language model to produce missing taxonomic codes, accompanied by confidence score
4. **Production of a classification patch** stored in a space separate from the knowledge base, without in-place modification of the source note
5. **Queueing for review** if confidence score is below a parameterizable threshold

### 4.3 Output

- A base where each note has a complete frontmatter conforming to taxonomy after patch application
- Doubtful notes flagged for manual review
- No note modified without explicit validation

### 4.4 Guard-rail

Phase 1 never deletes existing classification. It can propose an upgrade (missing codes added), never a downgrade or replacement, except via explicit pass through the validation mechanism.

## 5. Phase 2 — Analyze

### 5.1 Objective

Identify in the base the meaningful groupings that may indicate:

- A recurring pattern: same type of problem solved multiple times
- A repeated gap: same question asked without stored answer
- A method divergence: same problem, different solutions across sessions

### 5.2 Similarity graph construction

Vector representation of each note (semantic embedding computed on title and first paragraph). Search for K nearest neighbors by cosine similarity, K parameterizable, recommended starting value K=10.

### 5.3 Cluster detection

Algorithm:

1. For each note, find neighbors above a similarity threshold (recommended starting value 0.75)
2. Filter neighbors that share at least one classification axis in common with the source note
3. Verify the set (note + neighbors) totals a minimum number of occurrences distributed over time
4. Create a "cluster pattern" entry with a stable identifier

### 5.4 Eligibility scoring

For each cluster reaching the recurrence threshold, application of the six-criteria grid:

| Criterion | Measure | Automatic method |
|---|---|---|
| Recurrence | number of occurrences distributed over time | counting |
| Discriminating trigger | does an `IF/THEN` condition produce false positives? | generation + precision/recall test on sample |
| Universal applicability | does the method hold on N synthetic sub-cases? | generation + model validation |
| Reproducibility | do two pipeline executions diverge? | A/B on same input, output comparison |
| Added value | does the agent forget a critical step without the procedure? | contrastive simulation with and without procedure |
| Stability | does the method survive replacement of its underlying tools? | dependency analysis on volatile elements in member notes |

Composite score computed by weighted average. Threshold parameterizable, recommended starting value 0.75 on each individual criterion and 0.85 on composite mean.

### 5.5 Output

- List of detected clusters with members, score, status (`under_threshold`, `eligible`, `rejected`, `drafted`)
- Periodic consultable report
- No artifact generation at this stage — reserved for phase 3

### 5.6 Guard-rail

Every cluster is traceable: its member notes, its crossed threshold, its detection date, its score on each criterion. A rejection must be motivated by grid values.

## 6. Phase 3 — Propose

### 6.1 Objective

From clusters identified in phase 2 as eligible, produce drafts of artifacts ready to enter the external validation cycle.

### 6.2 Artifact types produced

#### 6.2.1 Formal procedure

When the cluster passes the six criteria. Generation of a structured procedure with:

- **Short descriptive name**, without proper noun of tool
- **Unambiguous trigger**: object condition (on what) + intent condition (to do what) + non-overlap clause (what is explicitly excluded)
- **Positive examples** (at least three) and **negative examples** (at least two)
- **Method** in numbered steps
- **Test cases** (at least ten synthetic cases the agent must handle correctly)
- **Metadata**: confidence, source cluster, version, status

#### 6.2.2 Documented pattern

When the cluster passes recurrence but fails on a qualitative criterion, typically added value or universal applicability. Lighter format, without trigger or test cases, but with documentation of the recurring solution and its application context.

#### 6.2.3 Consolidated reference

When a topic appears repeatedly in purely informational mode (factual lookups), generation of a consolidated reference note that supersedes multiple scattered queries.

#### 6.2.4 Signaled gap

When a cluster detects a recurring question without stored answer — the agent has re-searched the same information multiple times without memorizing it — production of a signal with proposal of reference note to create.

### 6.3 Generation

Content is produced by a language model capable of structured reasoning, with:

- **Strict prompt template** per artifact type
- **Auto-syntactic validation** post-generation: parsable frontmatter, mandatory sections present, format constraints respected
- **Bounded retry** on failure: maximum three attempts, then dead-letter

### 6.4 Output

- Drafts in storage zone separate from main knowledge base
- Notification to validation mechanism
- Status `pending_validation` until decision

### 6.5 Guard-rail

No draft is integrated into agent capabilities without explicit pass through validation mechanism. Phase 3 proposes, it never publishes.

## 7. Validation mechanism (external interface)

`self-improvement` relies on an **external** validation mechanism that the function does not include but that it feeds. The specification of this mechanism is intentionally open: it can be human (an operator), automatic (deterministic rules on scores), or hybrid.

### 7.1 Interface contract

The validation mechanism receives a draft with its context and returns a status among:

| Status | Consequence |
|---|---|
| `accepted` | The artifact is integrated into agent capabilities |
| `rejected` | The artifact is archived with motive. The source cluster is marked to not regenerate the same draft. |
| `revise` | Return to phase 3 with annotations to amend |

### 7.2 Traceability

The status is traced in the base and feeds phase 1 of the next cycle. A cluster with `rejected` draft no longer emits a new draft until its composition has significantly evolved (new members, modified score).

### 7.3 Out of function scope

The mechanism itself (its rules, its user interface, its notifications) is out of `self-improvement` scope. The function exposes an integration point and consumes returned statuses.

## 8. Feedback loop (function self-improvement)

`self-improvement` must be able to improve itself over time. Three quality metrics to track.

### 8.1 Classification precision (phase 1)

- Rate of `accepted` versus `rejected` patches during review
- If less than 80% acceptance: taxonomy or classification prompt drifts and must be amended

### 8.2 Detection relevance (phase 2)

- Rate of clusters that pass the grid then are effectively validated
- If less than 50%: criteria are too lax, or thresholds are miscalibrated

### 8.3 Draft quality (phase 3)

- Rate of drafts validated without major revision
- If less than 60%: generator (prompt, model, template) must be refined

These metrics feed a periodic report (monthly recommended) which itself can become a note in the knowledge base, classified and exploitable by the next iteration of the function.

## 9. Operational invariants

Five non-negotiable rules guarantee function integrity.

### 9.1 Read-only on knowledge base

`self-improvement` never modifies an existing note in the base. All its outputs live in a separate space (patches, drafts, reports) and are applied by an explicit channel after validation.

### 9.2 Append-only on artifacts

When a draft is validated, it is published. If it becomes obsolete later, it transitions to `superseded` status but is never deleted. Traceability prevails over directory cleanliness.

### 9.3 Idempotence

Any phase can be interrupted and restarted without state corruption. Operations are identified by hash and act only if state has changed since last pass.

### 9.4 Backpressure

The function respects available compute capacities (language models, embeddings, storage). On saturation, it slows down, it does not crash. Token bucket and configurable semaphores.

### 9.5 Confidentiality respected

Notes marked as sensitive (restricted visibility) are never sent to a remote model nor used as sources for public drafts. Filtering by visibility attribute before any outgoing call.

## 10. Technical architecture

### 10.1 Components

| Component | Role |
|---|---|
| Knowledge base watcher | detection of new or modified notes |
| Processing queue | persistence of notes to process, retry management |
| Async workers | parallel execution of phases within resource limits |
| State store | tracking of processed notes, clusters, drafts |
| Vector index | embedding storage for similarity search |
| Language model connector | classification (lightweight model) and generation (powerful model) |
| Embedding connector | vector computation on notes |
| Notification emitter | alerts validation mechanism |
| Metrics endpoint | Prometheus or equivalent exposition |

### 10.2 Minimum required storage

Four logical tables or collections:

```
notes_index
  note_identifier, content_hash, last_completed_phase,
  processing_status, error_counter

patterns
  cluster_identifier, common_axes, common_entities,
  occurrences, first_seen, last_seen, composite_score,
  skill_status

pattern_members
  cluster_identifier, note_identifier, similarity

drafts
  name, version, source_cluster, validation_status,
  artifact_path, created_at, decided_at

metrics
  timestamp, phase, note_identifier,
  duration_ms, tokens_in, tokens_out, error
```

### 10.3 Configuration

The function exposes at minimum the following parameters:

```
[knowledge_base]
read_path
output_patches_path
output_drafts_path

[phases]
cron_observe
cron_analyze
cron_propose

[classification_model]
endpoint
model_name
timeout_seconds

[generation_model]
endpoint
model_name
timeout_seconds

[embedding]
endpoint
model_name
dimension

[clustering]
similarity_threshold
minimum_cluster_size
recurrence_window_days

[eligibility]
individual_criterion_threshold
composite_score_threshold

[resources]
max_concurrent_classifications
max_concurrent_generations
token_budget_per_minute

[validation]
notification_endpoint
normal_priority
high_priority

[security]
dead_letter_path
max_retries
backoff_initial_ms
```

### 10.4 Failure modes and mitigations

| Risk | Mitigation |
|---|---|
| Self-destructive write to base | write limited to output space, base read-only by system configuration |
| Infinite loop on malformed note | failure counter per note, dead-letter after N attempts |
| Language model saturation | semaphores, token bucket, preference for lightweight models in phase 1 |
| Procedure hallucination | auto-syntactic validation + mandatory pass through external validation mechanism |
| Taxonomy drift | explicit taxonomy versioning in store, version migration triggering reprocess |
| Sensitive information leak | filtering by visibility attribute before model send, audit log of each outgoing call |

## 11. Articulation with rest of agent

`self-improvement` is a meta-level function. It acts on the agent itself, not on its current tasks. It fits in an architecture where:

- The agent does its normal work and traces its interactions as classified notes
- `self-improvement` runs in background, observes, analyzes, proposes
- The validation mechanism accepts or rejects proposals
- Validated artifacts enrich the agent's permanent toolbox
- On next cycle, the agent is more performant on recurring tasks

This is a closed virtuous cycle: more the agent works, more the base enriches, more the function detects opportunities, more the agent becomes capable.

## 12. Success criteria

The feature is considered a success when, over a six-month observation period:

- At least 80% of proposed classifications are accepted without modification
- At least 50% of produced drafts are published after validation
- At least three new capabilities per month are effectively integrated
- The agent demonstrates measurable reduction of resolution time on recurring tasks (sampling measurement)
- No regression caused by a validated draft (measurement: post-publication incident rate)

## 13. Out of scope

`self-improvement` does not:

- **Learn from scratch** — it improves the existing, does not invent capabilities absent from notes
- **Replace validation mechanism** — it proposes, it does not decide
- **Modify underlying model** — it acts in prompt/procedure layer, not in fine-tuning
- **Synthesize external knowledge** — it exploits only the agent's own notes
- **Auto-deploy executable code** — a draft contains instructions, not autonomous code

## 14. Implementation maturity levels

Four progressive tiers, incrementally deployable:

| Level | Capability | Operational risk |
|---|---|---|
| **L1 — Read-only audit** | Phase 1 and 2 work, generate reports, never write anything to base | Nil |
| **L2 — Assisted patches** | Phase 1 produces classification patches applied manually after review | Low |
| **L3 — Assisted drafts** | Phase 3 produces drafts submitted to validation, manual application after approval | Low |
| **L4 — High-confidence auto-application** | Patches and drafts above high confidence threshold are applied automatically, journaled, reversible | Moderate |

Recommended start: **L1 for four weeks** to validate that classification and pattern detection produce sensible results on existing base, before scaling up.

## 15. Synthesis

`self-improvement` is a reflexive function that transforms an agent's memory into a continuous improvement engine. It only requires the knowledge base to be clean, classified, and voluminous — not exhaustive.

Its value comes not from the sophistication of its algorithms, but from the rigor of its operational frame: stable taxonomy, append-only, explicit validation, idempotence, observability.

It is agnostic to the underlying knowledge base implementation. Any system satisfying the four prerequisites (structured notes, stable taxonomy, minimum volume, temporal distribution) can host this feature.

The benefit is asymmetric: setup effort is one-shot and bounded, the gain is cumulative and grows with base volume. More the agent works, more it becomes competent on what it does. This is the inverse of the classic model where performance plateaus at initial training.

## 16. Glossary

- **Capability artifact**: structured object (procedure, pattern, reference) the agent can consult or execute to handle a task.
- **Cluster pattern**: set of notes grouped by semantic similarity and common classification, candidate for procedure formalization.
- **Multi-axis classification**: codification system where a note is filed according to multiple independent dimensions (e.g., nature of activity, engagement form, document type).
- **Draft**: artifact automatically generated by the function, awaiting validation.
- **Embedding**: vector representation of text enabling semantic similarity computation.
- **Idempotence**: property of an operation producing the same result whether executed once or multiple times.
- **Validation mechanism**: external interface (human, automatic, hybrid) that accepts or rejects drafts produced by the function. Out of `self-improvement` scope.
- **Pattern**: formalized recurring solution applicable to multiple similar cases.
- **Formal procedure**: structured artifact containing a trigger, a method, and test cases, applicable deterministically when trigger is satisfied.
- **Trigger**: formal condition (combination of object and intent) determining whether a formal procedure should be activated.

---

## 17. Gradatum-specific integration notes

These notes adapt the generic specification to the gradatum project context. They are advisory, not normative for the RFC itself.

### 17.1 Crate placement

Proposed crate: `gradatum-self-improvement` (Layer L3 — Application). Reads:
- `gradatum-vault::Vault` (notes, frontmatter, lifecycle status)
- `gradatum-search::SearchEngine` (FTS5 + vector index)
- `gradatum-embed::Embedder` (semantic representation)
- `gradatum-curator` (classification taxonomy + heuristic/LLM backends)

Writes only to:
- New table `self_improvement_clusters` (in gradatum SQLite)
- New table `self_improvement_drafts` (separate from `notes`)
- New directory `/var/lib/gradatum/self-improvement/drafts/` (markdown drafts)

### 17.2 Taxonomy mapping

Gradatum taxonomy (10 canonical sections) satisfies §2.2 multi-axis requirement:
- Axis 1 (nature): `decisions` / `debug` / `experiments` / `agent-issues`
- Axis 2 (engagement form): `reasoning` / `feedback` / `lessons-learned` / `retrospectives`
- Axis 3 (document type): `architecture` / `reference`

Tags + wikilinks provide additional discriminating dimensions without polluting section codes.

### 17.3 Phase prerequisites for activation

| Prerequisite | Gradatum measurement | Estimated readiness |
|---|---|---|
| ≥200 notes in vault | `vault_status.total_notes` | Phase 2.1 dogfooding D5 (the maintainer legacy vault migrated) |
| Taxonomy stable ≥6 months | git history `routing.rs` PREFIX_PATTERNS unchanged | post-Phase 2.2 if no breaking changes |
| Volume optimal ≥500 notes | idem | Phase 3+ depending adoption |
| 3 sessions distinctes per pattern | timestamp distribution analysis | runtime measurable post-Phase 2.1 |

### 17.4 Articulation with curator (Phase 2.0c)

The Phase 2.0c curator cascade (5 functions: novelty + routing + tags + wikilinks + dedup) provides Phase 1 (Observe) primitives:
- Novelty SHA-256 + MinHash → "new note" detection
- Routing → classification patch generation
- Audit JSONL → trace history reconstruction

`self-improvement` Phase 1 reuses these primitives. Phase 2 (Analyze) and Phase 3 (Propose) require new logic.

### 17.5 LLM tier sizing

Phase 1 classification: lightweight model (Qwen3-4B-Instruct-2507 Q4_K_M from Phase 2.0c TOML config sufficient).

Phase 3 generation: requires structured reasoning capability — Qwen3-8B Q4_K_M minimum recommended (see council Phase 2.0c caveat B-03, deferred from alpha.4 scope but relevant here).

### 17.6 Audit trail integration

All `self-improvement` actions emit `AuditEvent` to existing `JsonlFileSink` (`/var/log/gradatum/audit.YYYY-MM-DD.jsonl`):
- `event: "self_improvement.observe"` — phase 1 patch generated
- `event: "self_improvement.cluster_detected"` — phase 2 cluster eligible
- `event: "self_improvement.draft_proposed"` — phase 3 draft produced
- `event: "self_improvement.validated"` — external validation accepted
- `event: "self_improvement.rejected"` — external validation rejected

### 17.7 Validation mechanism candidates

The external validation mechanism (§7) is out-of-scope for the RFC, but candidates aligned with gradatum's design philosophy include:
- **Human operator review** (default): `gradatum-cli self-improvement review` interactive prompt
- **Score-threshold automatic** (advanced): composite score ≥0.95 + criteria all ≥0.85 → auto-accept; logged but applied
- **Hybrid**: low-score → human review; high-score → auto-accept with audit trail
- **Multi-agent council** (gradatum-specific): multiple independent reviewers for high-impact drafts

## 18. Alternatives considered

### 18.1 Fine-tuning per session
Train a LoRA adapter on agent traces. Rejected because:
- Requires GPU compute infrastructure beyond gradatum scope
- Loses interpretability (procedure as data > weights)
- Cannot be reviewed/edited by human operator before integration
- Conflicts with `self-improvement` core principle: "explicit validation, not weight changes"

### 18.2 External agent training framework (e.g., DSPy, AutoGen)
Use existing framework. Rejected because:
- Adds dependency outside Rust ecosystem (Python primarily)
- Tied to LLM provider abstractions less generic than gradatum's 5-backend protocol
- gradatum philosophy: minimal dependencies, OSS-first-class, reproducibility

### 18.3 Manual procedure curation only
Skip the function entirely; rely on operator to write procedures from observations. Rejected because:
- Doesn't scale beyond ~50 procedures
- Operator cognitive load increases with knowledge base growth
- Loses opportunity for systematic pattern detection
- Conflicts with gradatum ambition: "memory as growth engine"

## 19. Drawbacks

- **Compute cost**: Phase 3 generation requires LLM with structured reasoning (≥8B model), non-negligible inference budget
- **Validation bottleneck**: human review of drafts can become a queue if generation rate exceeds review rate
- **Taxonomy lock-in**: stable taxonomy is prerequisite, but evolving project may require taxonomy migration → reprocess cost
- **Cold start**: requires ≥200 notes minimum + 6 months distribution; not useful in early adoption
- **False positive risk**: low-quality clusters generate drafts that waste validation cycles even with rejection markers

## 20. Unresolved questions

1. **Cluster identity stability**: when a cluster's member set evolves (new note added, old note superseded), should the cluster identifier change or stay stable? Argument for stable: traceability of decisions. Argument for change: prevents staleness of `rejected` markers.
2. **Multi-tenant isolation**: in gradatum, clusters span tenants? Or strictly tenant-scoped? Privacy implications.
3. **LLM provider for generation**: Phase 3 generation requires reasoning-capable model. Cloud providers (Anthropic, OpenAI, Google) raise privacy concerns for sensitive notes (§9.5). Local-only is preferred but constrains model choice to ≥8B local.
4. **Versioning of generated procedures**: if `self-improvement` regenerates a procedure (newer cluster member set), does it supersede the previous version automatically, or requires explicit re-validation?
5. **Interaction with `gradatum migrate-from-v0`** (Phase 2.1): pre-migration legacy vault notes have different metadata granularity. Should `self-improvement` analyze them, or wait for full re-classification post-migration?

These questions remain open and will be refined when this RFC moves from `proposal` to `accepted` (target: Phase 3+).

---

## References

- gradatum architecture: [`ARCHITECTURE.md`](../../ARCHITECTURE.md)
- gradatum taxonomy: 10 canonical sections (`decisions`, `architecture`, `debug`, `reasoning`, `feedback`, `lessons-learned`, `retrospectives`, `experiments`, `agent-issues`, `reference`)
- Phase 2.0c design spec: internal design document (not published in this repository)
- Curator cascade alpha.3 implementation: `crates/gradatum-curator/src/`
- Audit JSONL format: Phase 2.0b T10 (commit `b93fa4a` + `57450e2`)
- Legacy vault v1.6.2 baseline (gradatum predecessor): see migration guide

---

*RFC-0005 — proposal opened 2026-05-05. Discussion period: open until pre-Phase 3 readiness review.*
