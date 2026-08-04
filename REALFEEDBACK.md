# Real Live Setup Feedback — AMD AI HX 395 MAX 128 Go / llama.cpp field notes

Field notes from running gradatum's gateway + engine layer against `llama-server` on an **AMD AI HX 395 MAX 128 Go** mini-PC — 128 GB unified LPDDR5X (32 GB system / 96 GB VRAM allocation), 8060S iGPU (RDNA 3.5, gfx1151), Ubuntu. Unified-memory APUs behave differently from a discrete-GPU host on several axes below; the exact numbers are specific to this platform, but the failure modes generalize to any llama.cpp deployment on AMD AI HX 395 MAX 128 Go or similar UMA (unified memory architecture) silicon.

This is a companion to the [README](README.md#example-multi-host-setup) — everything here is a field observation from one real deployment, not a promise about behavior on other hardware.

## Backend: Vulkan over ROCm

**Problem.** ROCm + rocWMMA + hipBLASlt looked like the natural accelerated path for prompt-processing (prefill).

**What we saw.** A same-model `llama-bench` comparison (Vulkan RADV vs ROCm 7.2 + rocWMMA, identical flags, no crash on either path) showed ROCm consistently *slower* in prefill — pp512 ×0.73, pp1024 ×0.59, pp4096 ×0.35 vs Vulkan, with the gap widening at longer sequences. Decode throughput was identical either way (memory-bandwidth bound). Separately, a native vLLM attempt never got past startup: the system ROCm (7.2) doesn't satisfy the HSA API version the vLLM wheel's bundled ROCm SDK requires (7.12), and no AMD-published gfx1151 wheel exists.

**Fix/Setting.** Stayed on Vulkan. It remains the backend that reliably runs on this silicon today.

## Prompt-cache slot thrashing under concurrent sessions

**Problem.** `llama-server`'s slot-reuse heuristic (`--slot-prompt-similarity`, default `0.10`) is tuned for a single active session. Two interleaved sessions sharing a similar prefix (same system prompt + tool schema, cross-session similarity measured at ~0.58-0.59) kept landing on the same slot and evicting each other.

**What we saw.** At the default threshold, 9 of 10 requests in an interleaved two-session replay triggered a full prefix re-prefill (214-222 s each) — one slot was never used at all.

**Fix/Setting.** Raising `--slot-prompt-similarity` to `0.8` (above the measured cross-session similarity, below the intra-session similarity of ~0.997-1.000) eliminated the thrash: after one cold turn per session, subsequent turns dropped to 7.7-11.9 s — roughly a **27×** improvement in the contended regime, with zero cross-session eviction and no regression to single-session latency. This only helps up to your `--parallel` slot count; one more concurrent session than you have slots still forces an eviction.

## Batch/ubatch tuning and flash-attention

**Problem.** Default batch/ubatch values were not tuned for prefill-heavy agentic traffic.

**What we saw.** Sweeping `--ubatch-size` (with `--batch-size 4096` fixed) peaked at **`--ubatch-size 1024`** and degraded *monotonically* above it (2048: −5.6% prefill / +1 GiB VRAM; 4096: −8.5% / +2.8 GiB) — bigger ubatch does not help on this iGPU, it hurts. The same `4096/1024` pairing reproduced a ~50% prefill improvement on a second, differently-sized model. Flash-attention is not optional: KV-cache quantization (`q8_0`) refuses to start without it, and even without KV quant it measured +20% prefill / +13% decode at 16K context vs off.

**Fix/Setting.** `-b 4096 -ub 1024 -fa 1` as the baseline profile; don't chase a larger ubatch expecting more throughput here.

## KV-cache quantization trade-off

**What we saw.** On a 30B-class MoE model, KV `q8_0` vs `f16` (iso config, 192K context) cost ~2.8% decode throughput but saved **8.4 GiB VRAM** — a good trade when VRAM is shared across several co-resident engines. `--n-predict` had zero measurable effect on prefill or VRAM at any value tested — it is an output-length cap only, not a pre-allocation.

## Sampling penalties can silently fall off the GPU path

**Problem.** Decode throughput on one model was ~40% below the number `llama-bench` reported for the same model, even with `--backend-sampling` enabled.

**What we saw.** Isolating axes one at a time (context size, batch/ubatch, KV quant) accounted for at most ~9% of the gap combined; `--presence-penalty` alone accounted for **~34%** — and the effect was binary (a penalty of `0.1` cost exactly as much as `1.0`), because any non-zero presence/frequency penalty routed generation through a CPU vocab-scan fallback instead of the GPU backend-sampling path.

**Fix/Setting.** We built and validated (bit-exact vs the unpatched path, verified by hash comparison) a small private llama.cpp patch that moves presence-penalty scoring onto the GPU path, recovering the throughput (56 → 85 t/s in one measurement). It is not upstream and not part of gradatum's own codebase. If you rely on presence/frequency penalties on Vulkan/UMA hardware and see decode throughput far below `llama-bench`, this is the first thing to check.

## Speculative decoding (MTP): architecture-dependent, not a free win

**What we saw.** Multi-token prediction was repeatedly confirmed net-*negative* (4B, 27B, and two 35B configurations) on models with a hybrid-state architecture: draft acceptance was high (up to 87%) but the gain was erased by per-cycle state save/restore overhead plus UMA bandwidth contention — one 35B case measured 53.0 t/s with MTP off vs 20.2 t/s with it on, a 2.6× slowdown despite good acceptance. A control test on a memory-shared draft/target architecture (no save/restore needed) came back net-*positive*: **+11.5%** at `--spec-draft-n-max 2`; `n-max 3` was only +4%, and `n-max 4` regressed −10.5% as acceptance collapsed at deeper draft positions.

**Fix/Setting.** Only enable speculative decoding where the draft head shares the target's KV state, and don't push `--spec-draft-n-max` past 2 on this class of hardware — high acceptance alone does not predict a win.

## Model sizing under a shared VRAM budget

**What we saw.** A dense (non-MoE) ~27B model was the worst possible fit for this bandwidth-bound APU — full parameter count active per token. A 35B-class MoE candidate topped out at 53 t/s decode in its best configuration, slower than a 30B-class MoE model already in service (80.9 t/s / 78.8 t/s across two of its modes), so it never replaced it. Separately, an 80B-class MoE model was retired from the live rotation in favor of a smaller ~26B-class MoE model that scored higher on an internal quality benchmark (223/240 vs 178/240) while freeing over 40 GiB of VRAM.

**Fix/Setting.** On a fixed shared-VRAM host running several concurrent engines, benchmark MoE candidates head-to-head on real traffic before assuming a larger parameter count wins — active-parameter count and achievable throughput matter more than total size on a bandwidth-bound APU.

## Models tested & benchmarked

The table below lists every model that was actually loaded and measured on this hardware, in the roles gradatum's engine layer serves (`deep`/reasoning, `vision`, `agent-main` — the unified role that later absorbed deep+vision+no-think, `curator`, `embed`/`router` candidates), plus three larger models benchmarked standalone (pre-dating the engine/gateway layer) for reference. `pp`/`tg` are prompt-processing/text-generation throughput in tokens/sec; where a value depends on the measured context length, it is given as `high→low (range)`. "—" means the value wasn't captured in the source note — we didn't guess it. That applies in
particular to the `gradatum version` column: it is filled only for the three rows whose source
note recorded a build reference (`gateway build 9d9f092`, the current live fleet). The other
fourteen rows are measurements of the `llama-server` layer taken between April and July 2026,
and no gradatum build was recorded alongside them at the time. Back-attributing a version to
them now would be a reconstruction, not a measurement, so the cells are left empty on purpose.
`— (pre-engine)` is stronger than an empty cell: it means the measurement predates the
engine/gateway layer entirely, so no gradatum version could apply. The `llama.cpp (internal fork)` column gives a short build reference — see [Internal llama.cpp fork — build lineage](#internal-llama-cpp-fork--build-lineage) below for what each build handled or added.

| Model | Arch / params | Quant | Role / workload | llama.cpp (internal fork) | gradatum version | Status | pp (t/s) | tg (t/s) | ctx | VRAM | KV | MTP | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Qwen3-72B-Instruct | dense, 72B | Q4_K_M | standalone bench | c30e012 | — (pre-engine) | Evaluated, never deployed | — | 4.4 | 131K | 84.0 GiB | — | n/a | FAIL — incoherent output; dense 72B incompatible with unified memory |
| Qwen3.5-122B-A10B | MoE, 122B / ~10B active | Q3_K_M and UD-Q4_K_XL (bench, loaded OK); UD-Q3_K_XL (later attempt, did not load) | standalone bench, then deep candidate | c30e012 (bench) / b9725a31 (later attempt) | — | Evaluated, never deployed | — | 33 (Q3_K_M) / 22 (UD-Q4_K_XL) | 131K (April bench); untested (June, load failure) | 58.6 GiB (Q3_K_M) / 75.3 GiB (UD-Q4_K_XL) | — | n/a | Best quality/speed in the original bench (9.25-9.5/10 internal score); a newer llama-server build later failed to load it (unsupported hybrid MoE architecture) |
| Nemotron 3 Super 120B | MoE, 120B | Q4_K_XL and Q3_K_M | standalone bench | c30e012 | — (pre-engine) | Evaluated, never deployed | — | 13 (Q4_K_XL) / 14 (Q3_K_M) | 131K | 79.4 GiB (Q4_K_XL) / 58.9 GiB (Q3_K_M) | — | n/a | Strong code output; Q3 quant broke a math test |
| Qwen3.6-27B | dense, 27B | Q4_K_XL (historical default) | `deep` (original) | — | — | EX-LIVE — decommissioned | — | 9.89 | — | ~19 GiB | — | n/a | Worst possible fit for a bandwidth-bound APU (full param count active per token) |
| Gemma 4 12B | dense, 12B | UD-Q4_K_XL | `deep` candidate | — | — | Evaluated, rejected | — | 4.23 | — | 12 GiB | — | n/a | -57% vs the dense-27B baseline; vision path blocked (multimodal projector unsupported by that build) |
| Qwen3.6-35B-A3B | MoE, 35B / ~3B active | Q4_K_XL (historical default) | `deep`+`vision` consolidated, then `vision`-only | b9549 | — | EX-LIVE — decommissioned | — | 12.0 (contended bench) → 18.97 (clean re-measure) | up to 1M (YARN, pre-consolidation vision instance) | ~30-37 GiB (per a contemporaneous config note) | — | n/a | +22% throughput vs the dense-27B baseline; later superseded by a vision-purpose-built MoE |
| Qwen3-Next-80B-A3B-Thinking | hybrid state-space + MoE, 80B / ~3B active | UD-Q4_K_XL | `deep` (dedicated) | b9549 | — | EX-LIVE — decommissioned | 694 (production baseline) | 25.1 (initial reasoning bench) → 60.5 (production baseline) | @256K (topology) | ~43-47 GiB (KV alone 3.19 GiB) | — | tested — no MTP head in this GGUF | +32% vs the 35B-A3B on the initial reasoning bench; scored 178/240 on an internal quality benchmark at retirement |
| Qwen3-VL-30B-A3B-Instruct | MoE, vision-purpose-built, 30B / ~3B active | Q4_K_XL (historical default) | `vision` (+agent+coder) | b9549 | — | STANDBY — disabled during a fleet consolidation, kept for a burn-in window before a separate decommission decision | 1021 → 578 (0.5-16K, test instance) | 81.5 → 58.6 (0.5-16K, test instance) | 256K (production) / 32768 (bound, head-to-head test instance) | ~38.5 GiB (@256K, production) | — | n/a | Vision accuracy 3/3 on an image/tool-call/OCR check |
| Gemma 4 26B-A4B QAT | MoE, 26B / ~4B active | UD-Q4_K_XL | `deep` | b9780 | — | EX-LIVE — decommissioned | 1143 (short ctx) | 74-94 (≤16K) → 17.5-21 (≥24K) → 16.62 (55K, LIVE settled) | 65536 (LIVE settled) | — | f16 | on (n-max=2), net-positive +11.5% in an isolated control test | Scored 223/240 on an internal quality benchmark (the best measured) |
| Qwen3.6-35B-A3B (+ MTP head) | MoE, 35B / ~3B active | UD-Q4_K_XL | `deep` re-test | b9780 | — | Evaluated, rejected | — | 53.0 (MTP-off) / 20.2 (MTP-on) | — | — | — | tested-rejected (2.6× slower despite 87% draft acceptance) | Base throughput also non-competitive vs the incumbent |
| Qwen3-30B-A3B-Instruct-2507 | MoE, 30B / ~3B active | Q4_K_XL | `deep` | b9780 + private patches (B4/B1) | — | STANDBY — disabled during the same fleet consolidation, burn-in pending | ~950 (stock build) | ~56-58 (stock) → 86.4 (private GPU-path patch) | 196608 (192K) | 68.3 GiB | q8_0 | n/a (no draft head configured for this variant) | Settled deep config before the E1 fleet consolidation |
| Qwen3.5-35B-A3B (+ MTP head, disabled) | MoE, 35B / ~3B active | Q4_K_XL + vision projector, F16 | `agent-main` (unified no-think + think + vision) | b9780 + private patches (B4/B1) | gateway build `9d9f092` | **LIVE** (current) | — | 87 → 79 (0.5-16K) | 196608 (96K/slot × 2 parallel) | 23.3 GiB | q8_0 | head present, disabled (structural — hybrid-state architecture) | Scored parity with the previous reasoning incumbent on a 200-item benchmark |
| Qwen3-4B-Instruct-2507 | dense, 4B | Q8 | `curator` (incumbent) | b9780 | gateway build `9d9f092` | **LIVE** (current) | — | 39.3 | — | 5.45 GiB | — | n/a | Won 3 separate challenger benchmarks; ~0.1 s response time as a routing candidate |
| bge-m3 | embedding model | Q8 | `embed` | b9780 | gateway build `9d9f092` | **LIVE** (current) | n/a | n/a | — | 0.74 GiB | n/a | n/a | Small dedicated footprint; unchanged since the vision/deep topology was finalized |
| Qwen3.5-4B (2 configs) | dense, 4B | Q8 | `curator` candidate | b9780 | — | Evaluated, rejected | — | 31 (backend-sampling) / 19 (MTP) | — | — | — | tested-rejected (86% draft acceptance, quality-neutral, slower) | Parity-or-worse output quality vs the incumbent |
| Qwen3-4B-Thinking-2507 | dense, 4B, reasoning-tuned | UD-Q8_K_XL | `curator` candidate | b9780 | — | Evaluated, rejected | ~1000 | 37.0-37.2 | 16384 | — | — | n/a | ~55 s per item — over-reasons on trivial classification instead of stopping |
| LFM2.5-8B-A1B | MoE, 8B / ~1B active | Q6_K | router/`curator` candidate | b9780, CPU-only | — | Evaluated, rejected | — | — | 4096 | n/a (CPU-only) | — | n/a | No "stop reasoning" toggle by design — either fast-and-wrong or far too slow for a latency-critical role |

### Internal llama.cpp fork — build lineage

The engine layer has always run locally-compiled `llama-server` binaries — never a stock package. This table traces the build lineage referenced in the `llama.cpp (internal fork)` column above.

| Build | Date | Base upstream | Period / role in the fleet | What this build handled / added |
|---|---|---|---|---|
| `c30e012` | 2026-04-02 | — (standalone HEAD build, no fork applied) | Standalone benchmarks, pre-dating the engine/gateway layer entirely | Used for the initial Qwen3-72B-Instruct / Qwen3.5-122B-A10B / Nemotron 3 Super 120B comparison (Vulkan backend, flash-attention on, 131K context). |
| `b9549` | 2026-06-07 | upstream | Engine fleet era: deep+vision consolidation, dedicated Qwen3-Next-80B-A3B-Thinking, Qwen3-VL-30B-A3B-Instruct, the B' v2.1 topology cutover | Unblocked Gemma's multimodal projector (`gemma4uv`) and Qwen3.5's MoE architecture (`qwen35moe`) upstream — both of which the June fleet depended on. Note: one source from the same day cites a different build string, `b9725a31`, for a 122B/Gemma-vision load-failure test run in the same session — reproduced here as-is, not reconciled with `b9549`. |
| `b9780` | 2026-06-24 (earliest documented incident evidence — see date caveat) | upstream | Base build from the vision-engine upgrade onward, through the July 2026 engine-layer benchmark round (curator Q8 A/B/C, KV `q8_0`/ubatch tuning, MTP/backend-sampling experiments, the vision head-to-head) — and the base upstream for the private fork below | Flash-attention, KV-cache quantization (`q8_0`), and `--backend-sampling` were all exercised on this build (not necessarily first introduced here — not sourced either way). Also the build on which a tool-call grammar (GBNF) regression was found, on integer-range schema rules with large `maximum` values (llama.cpp issues #20867/#21228) — workaround: force `tool_choice=auto` server-side when a request carries tools but no explicit choice; the proper fix requires rebuilding past the point where the regression was patched upstream (≥b9800, PR#21003), not yet applied here. |
| `b9780 + private patches (B4/B1)` | 2026-07-09 | `b9780` | Deployed across the engine fleet at the time (deep, vision, curator, embed) — now the base for the agent-main/curator/embed fleet post-E1 cutover | **Patch B4**: moves presence-penalty scoring onto the GPU `--backend-sampling` path — previously, any non-zero presence/frequency penalty silently fell back to a CPU vocab-scan. Recovered decode throughput on the deep model, 56→85 t/s (+52%), bit-exact vs the unpatched path (verified by hash comparison). **Patch B1**: extends `--backend-sampling` to multi-row/multi-sequence speculative verification — previously gated off for any sequence producing more than one output. +314% decode throughput on a spec-stateless model (20.96→86.8 t/s), bit-exact for stateless sampling; a server-side gate routes stateful-penalty + speculative combinations back to CPU, since that combination is not bit-exact under the patch. Both patches compile-clean and were validated token-identical against the unpatched build before the fleet-wide deploy. |

**Live today:** the current fleet is a 3-model set — one unified reasoning/vision model, one small classification model, and one embedding model — running on a private llama.cpp fork built on top of a specific upstream build.

**Standby, not decommissioned:** two other models (a dedicated reasoning model and a dedicated vision model) were disabled during a fleet consolidation and held for a short burn-in window pending a separate decision — configuration and weights were kept, and reverting was measured at about two minutes. This is a live-fleet snapshot: by the time you read this, those entries may have been either decommissioned or reinstated.

**Ex-LIVE, decommissioned:** four other models held the reasoning and/or vision role at various points and were each superseded once a later benchmark produced a clear winner — this is normal churn on a platform where the *active-parameter* count matters more than the headline parameter count.

**Evaluated, never adopted into the fleet:** several classification/routing candidates, and three much larger dense/MoE models tested in early standalone benchmarks before the engine/gateway layer existed — none of the three ever made it into the actual gradatum-served fleet, and one of them later turned out to be unloadable on a newer llama-server build due to an unsupported architecture.

## Host-level OS tuning: mostly a dead end

**What we saw.** Disabling `amd_iommu` and forcing the CPU governor to `performance` moved throughput by +0.7-0.8% — within noise. The bottleneck here is GPU memory bandwidth, not host OS configuration; don't expect kernel or IOMMU tuning to move the needle on this class of hardware.

## Context window sizing vs. actual prompt size

**Problem.** Claude Code sent a large prompt (system prompt + a rich MCP tool surface, tokenized at over 80K tokens) against an engine configured with a 64K context window.

**What we saw.** The engine rejected the oversized request and the gateway silently fell back to a small CPU-only model with an 8K context — the symptom looked like "the model is slow and gives generic answers" (an ~8-minute round trip), not "context exceeded."

**Fix/Setting.** Size `context_len` to your real worst-case prompt (tokenize a representative payload — don't guess), and set upstream request timeouts above your measured cold-prefill time; a timeout shorter than cold prefill keeps triggering the same silent fallback even after the context size is fixed. Also worth knowing: if you use a draft/speculative head, its own trained context length can cap the *usable* context of the pair below the target model's trained maximum.

## Tool-call grammar regression on a specific llama.cpp build

**Problem.** Claude Code sends many tool schemas without an explicit `tool_choice`, which triggers constrained-grammar generation (GBNF) for tool-calls. On one llama.cpp build, the grammar compiler regressed on integer-range schema rules (large `maximum` values in tool schemas), producing a hard parse failure — masked behind a misleading "context exceeded" error once the gateway's automatic fallback kicked in.

**Fix/Setting.** Forcing `tool_choice=auto` server-side whenever a request carries tools but no explicit `tool_choice` avoids triggering grammar generation entirely and worked as an effective bypass. The proper fix is rebuilding past the point where the upstream grammar regression was patched (tracked, not yet applied here). If you hit `"failed to parse grammar"` on tool-heavy agentic traffic, check your llama.cpp build date before assuming it's a client or gateway bug.

## Engine supervisor flag allowlisting

**What we saw.** gradatum's engine supervisor validates `llama-server` extra flags against an explicit allowlist before spawning the child process. An unlisted flag doesn't silently no-op — the supervisor refuses to start the child at all, which can present as a restart loop rather than a clear "unknown flag" error if you're only watching the health endpoint. Check supervisor/service logs for a rejected-flag message before assuming a hardware or model problem right after a config change.
