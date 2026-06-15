# Benchmarks

> **Status**: benchmark suite — only B1 measured; B2-B8 pending.
> **Date** : 2026-05-04
> **Hardware** : Linux container, 4c, 20GB RAM, NVMe ZFS mirror
> **Host CPU** : x86_64, AVX-512
> **Source** : `crates/gradatum-bench/benches/`
> **Annexe spec** : Phase 1 perf bench results spec (internal, not published) §3
> **Phase 1 status** : 10 benches actifs (B1, B2b, B3, B4, B5, B6, B7, B8a, B8b) + 1 feature-gated (B2a) + 2 scripts standalone (B9, B10)

## Méthodologie

Chaque bench utilise `criterion 0.5.1` avec :
- 3s warm-up (défaut criterion)
- 100 itérations mesurées (sauf benches I/O coûteux : `sample_size(20)` ou `sample_size(30)`)
- Médiane + outliers reportés par criterion

Run : `cargo bench -p gradatum-bench --bench bNN_*`

Active `--features fastembed-cpu` pour B2a uniquement.

## Résultats P0 (obligatoires avant tag v0.1.0-alpha)

| # | Bench | Target spec §3 | Médiane mesurée | Verdict |
|---|---|---|---|---|
| **B1** | ContentHash JCS — 10KB body | < 1ms | **5.23µs** | **PASS** ✓ (×190 sous la target) |
| B1 | ContentHash JCS — 100B body | — | 1.13µs | info |
| B1 | ContentHash JCS — 1KB body | — | 1.57µs | info |
| B1 | ContentHash JCS — 100KB body | — | 42.4µs | info |
| B2a | FastEmbedCpu single | < 100ms / single | SKIPPED | DEFERRED (feature `fastembed-cpu` off — bug ort-sys T08) |
| **B2b** | HttpEmbedder wiremock roundtrip | p50 < 15ms, p99 < 50ms | TODO — run T15 | TODO |
| **B3** | SQLite WAL INSERT 1000 upserts | > 5000/sec | TODO — run T15 | TODO |
| **B4** | Drift Phase A 100 fichiers stables | < 100ms / 10K files | TODO — run T15 | TODO |

> **B1 PASS confirmé** — mesuré lors du commit T14 (`cargo bench -p gradatum-bench --bench b01_jcs_hash`).
> B2b, B3, B4 : squelettes compilent, mesures déférées à T15 (run complet sur main avant tag).

## Résultats P1 (best-effort avant tag)

| # | Bench | Target spec §3 | Médiane | Verdict |
|---|---|---|---|---|
| B5 | Cache moka + checksum hit/miss | > 70% hit rate, p99 < 500µs | TODO — run T15 | TODO |
| B6 | EffectiveNoteCache cold insert / hot get | cold < 10ms, hot < 500µs | TODO — run T15 | TODO |
| B7 | JSONL audit BufWriter 64KB 1000 events | > 50K events/sec | TODO — run T15 | TODO |
| B8a | Curator heuristic fast path 100 notes | > 100 notes/sec | TODO — run T15 | TODO |
| B8b | Curator mixed 30% Noop LLM 100 notes | > 50 notes/sec | TODO — run T15 | TODO |

> P1 bench squelettes compilent. Mesures et verdict T15.

## Résultats P2 (déférés)

| # | Bench | Target | Status |
|---|---|---|---|
| B9 | posix_fallocate NFS | EOPNOTSUPP détecté sur NFS | DEFERRED — script `scripts/b09_posix_fallocate_nfs.sh`, gated `GRADATUM_TEST_NFS_PATH` |
| B10 | Binary size workspace release | feature gates effectives | DEFERRED — script `scripts/b10_binary_size.sh` + `cargo-bloat` |

## Reproduction

```bash
# Run tous les benches actifs
cargo bench -p gradatum-bench

# Run bench spécifique
cargo bench -p gradatum-bench --bench b01_jcs_hash

# B2a (requiert feature fastembed-cpu + ~150MB download modèle ONNX)
cargo bench -p gradatum-bench --features fastembed-cpu --bench b02a_fastembed_cpu

# B9 — NFS (requiert montage NFS)
GRADATUM_TEST_NFS_PATH=/mnt/nfs ./crates/gradatum-bench/scripts/b09_posix_fallocate_nfs.sh

# B10 — binary size (requiert cargo-bloat installé)
./crates/gradatum-bench/scripts/b10_binary_size.sh
cat docs/bench/b10_binary_size.txt
```

## Acceptance

- **P0 (5 benches)** : tous doivent atteindre leur target avant tag v0.1.0-alpha.
  B1 PASS confirmé T14. B2a DEFERRED (ort-sys bug T08 — non bloquant Phase 1).
  B2b, B3, B4 : mesures T15 — si miss, ouvrir issue + accepter delta documenté.
- **P1 (5 benches)** : best-effort. Misses tracés en backlog Phase 2.
- **P2 (2 benches)** : déférés Phase 2+, scripts disponibles pour run manuel.
