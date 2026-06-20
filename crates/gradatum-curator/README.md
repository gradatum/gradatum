# gradatum-curator

> LLM-powered note curation: heuristic-first gating, LLM section routing, and a five-step pipeline for novelty, tagging, wikilinks, and deduplication.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-curator` implements the note intake pipeline. When a note arrives via the HTTP API,
the curator decides whether to admit it, which section it belongs to, and what metadata to
attach — without requiring a network call for the common case.

**Phase 1 — `Curator<C>` (heuristic + optional LLM review)**

```text
Curator<C: Chat>::decide(note, ctx) → CuratorDecision
  step 1: Heuristic::classify_curator(note, ctx)
  step 2: confidence > threshold → fast path (heuristic verdict)
  step 3: llm_review_enabled → C::classify_curator(note, ctx)
  step 4: LLM error → FallbackStrategy applied
```

**Phase 2 — `CuratorPipeline` (five-step offline-capable pipeline)**

```text
CuratorPipeline::process(note) → CurateOutcome
  step 1: novelty   — SHA-256 exact match + MinHash 128-perm Jaccard ≥ 0.92
  step 2: routing   — regex heuristic across 11 gradatum sections
  step 3: tags      — TF-IDF top-5 + kebab-case normalization
  step 4: wikilinks — regex extraction + Jaro-Winkler 0.88 fuzzy matching
  step 5: dedup     — cosine similarity on embeddings
```

The pipeline is offline-first: all steps except optional LLM review run without network access.

## Usage

```toml
[dependencies]
gradatum-curator = "0.4.0"
```

```rust
use gradatum_curator::{CuratorPipeline, CurateOutcome};
use gradatum_core::config::CuratorPipelineConfig;

// Construction depuis la config TOML (lit api_key_env si configurée).
let pipeline = CuratorPipeline::from_config(&config);

// Exécution — infaillible (erreurs LLM → FallbackStrategy interne).
let outcome: CurateOutcome = pipeline.process(note).await;
```

## License

Apache-2.0
