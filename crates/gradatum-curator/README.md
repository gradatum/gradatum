# gradatum-curator

> LLM-powered note curation: heuristic-first section routing with an optional LLM review for low-confidence notes.

**Status**: v2.0.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-curator` implements the note intake pipeline. When a note arrives via the HTTP API,
the curator decides whether to admit it, which section it belongs to, and what metadata to
attach — without requiring a network call for the common case.

**`Curator<C>` (heuristic + optional LLM review)**

```text
Curator<C: Chat>::decide(note, ctx) → CuratorDecision
  step 1: Heuristic::classify_curator(note, ctx)
  step 2: confidence > threshold → fast path (heuristic verdict)
  step 3: llm_review_enabled → C::classify_curator(note, ctx)
  step 4: LLM error → FallbackStrategy applied
```

**`CuratorPipeline` (heuristic routing + optional LLM review)**

```text
CuratorPipeline::process(note) → CurateOutcome
  step 0: valid section_hint → admitted directly with that section (no enrichment)
  step 1: routing::heuristic_route(title, body) → (section, confidence)
          confidence ≥ heuristic_admit_threshold (default 0.8) → Admitted
  step 2: llm_review_enabled = false OR confidence > confidence_threshold (default 0.7) → Pending
  step 3: llm_review_enabled = true AND confidence <= confidence_threshold
          → LLM classify → Admitted { section, tags } | Pending | Rejected
```

As of `2.0.0`, `process` runs section routing plus the optional LLM review only. The
`CuratorDecisions` fields `novelty`, `wikilinks`, and `dedup` always hold fixed defaults
(`Admitted` / `[]` / `Unique`); `tags` are populated only by the LLM review. Novelty
detection (SHA-256 + MinHash), TF-IDF tagging, wikilink scoring (Jaro-Winkler), and
semantic deduplication (cosine over embeddings) are **provided as standalone detectors
(`novelty`, `dedup` modules) but not wired into `process`** — no release currently
integrates them into the pipeline.

Routing runs offline: no network call is made unless the optional LLM review is enabled.

## Usage

```toml
[dependencies]
gradatum-curator = "2.0.0"
```

```rust
use gradatum_curator::{CuratorPipeline, CurateOutcome, CuratorPipelineConfig};

// Build from TOML configuration (reads api_key_env if set).
let pipeline = CuratorPipeline::from_config(&config);

// Run curation — infallible (LLM errors are absorbed into the outcome).
let outcome: CurateOutcome = pipeline.process(note).await;
```

## License

Apache-2.0
