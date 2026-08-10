# Memento RS embed bench — ONNX model cached, first run

**Recorded:** 2026-08-10
**Commit:** `e7f8276` (`main`)
**Toolchain:** w64devkit rustc 1.97, ONNX Runtime 1.28.0 (vendored)
**Crate:** `memento-e2e` — `benches/embed_bench.rs` (T-103)

## What this captures

The `embed_bench` `MEMBENCH gate_embed` line was previously reported as
`status: skipped, reason: model not cached under MEMENTO_MODELS_DIR` because the
MultilingualE5Small ONNX weights were absent (obs #2657, risk R1). This is the
first measured run after downloading the model.

Bench forced with `MEMENTO_BENCH_EMBED=1` (the documented skip override).
Cache location pinned to `MEMENTO_MODELS_DIR=F:\target\memento-bench-cache\models`
so re-runs exercise the warm path.

## How the bench is structured

- Same corpus as the production chunker: 64 Spanish-style texts × ~270 tokens
  each (T-024 boundary, T-032 chunk-size bounds).
- 2 warmup batches (absorb model-init / allocator warmup).
- 16 measured batches.
- Criterion group `embed/batch_64_texts` with `sample_size=20` and
  `Throughput::Elements(64)`.
- **No cold-cache measurement is taken by this harness.** Cold-cache latency is
  the time to load the ONNX into the ONNX Runtime session — that path is
  exercised only on first use and is masked by the 2-batch warmup.
- The bench is labeled **"T-103 — embedding latency bench (informational; no
  spec budget)"** in its own header. There is no hard `<1s` gate in
  `scripts/bench.sh`.

## Measured numbers (warm cache, host w64devkit)

```
MEMBENCH gate_embed {"batches":16,"p50_ms_per_batch":13540.4508,
                     "p50_ms_per_chunk":211.56954375,
                     "p99_ms_per_batch":14394.5792,
                     "status":"ok","texts_per_batch":64}

embed/batch_64_texts    time:   [13.440 s 13.662 s 13.900 s]
                        thrpt:  [4.6043 elem/s 4.6846 elem/s 4.7620 elem/s]
```

| Metric | Value |
| --- | --- |
| p50 latency per 64-text batch | **13.54 s** |
| p99 latency per 64-text batch | 14.39 s |
| p50 latency per chunk (amortized) | **211.6 ms** |
| Criterion mean batch time | 13.66 s |
| Throughput | ~4.68 elements/s |
| Wall time for the whole bench | ~9.5 min (first compile + ONNX download + warmup + 36 criterion iterations) |

## Verdict vs. "<1 s first-token" target

The bench measures **64-text batch latency**, not "time to first vector".
Reported honestly:

- **Per-chunk amortized = ~212 ms warm.** A single chunk of ~270 tokens
  embeds in ~212 ms — comfortably **under 1 s**.
- **Whole batch = ~13.5 s.** For 64 texts in production batch size, the
  ONNX session is ~13.5 s end-to-end. There is no spec budget for this, and
  the bench header marks it informational.

The "first token" framing in obs #2657 was a soft target, not a hard gate.
The bench produced a real number, but the existing harness does not isolate
single-text first-call latency from warm batch latency.

## Cache contents

After the first run, the fastembed HF cache holds **464.78 MB across 11
files** under `F:\target\memento-bench-cache\models\` (the
`hub/models--intfloat--multilingual-e5-small/snapshots/.../onnx/model.onnx`
lives here). Subsequent runs with the same `MEMENTO_MODELS_DIR` reuse this
cache and hit the warm-path numbers above.

## Risks / caveats

1. **Throughput is ~4.7 chunks/s.** A real ingest run on a multi-thousand
   chunk document will be ONNX-bound for the embedding phase. The previous
   bench (obs #2657) measured ingest at ~3.6 k chunks/s — that uses the
   `StubEmbedPort` (no ONNX). Real ingest with the production embedder
   will be markedly slower, by design.
2. **No cold-call measurement.** The first request of a fresh process pays
   ONNX Runtime session-load + allocator warmup. This bench hides it in the
   2-batch warmup. If REQ-MR-007 / REQ-CG-002 demand an isolated cold-call
   measurement, that's a separate bench or a startup-time hook.
3. **`-j 2` build flag is required on w64devkit** (obs #2603) — parallel
   linking exhausts `ld.exe` memory.
4. **No bandwidth / no RTT measurement.** This bench is single-host CPU
   latency only; disk + mmap and ONNX session init are not part of the
   reported number.
5. **CI doesn't run this bench by default.** The bench is gated by
   `MEMENTO_BENCH_EMBED=1` (or `scripts/bench.sh --embed`) precisely so
   CI doesn't download 500 MB per run.

## Reproducibility

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;F:\OPENCODE proyectos\.toolchains\w64devkit\bin;$env:Path"
$env:RUSTUP_HOME = "F:\OPENCODE proyectos\.toolchains\rustup"
$env:CARGO_HOME = "F:\OPENCODE proyectos\.toolchains\cargo"
$env:MEMENTO_MODELS_DIR = "F:\target\memento-bench-cache\models"
$env:MEMENTO_BENCH_EMBED = "1"
cargo bench -p memento-e2e --bench embed_bench -j 2
```

Or via the canonical wrapper:

```bash
scripts/bench.sh --embed
```

## Source

- `benches/embed_bench.rs` — bench source (read-only, no changes)
- `target/criterion/embed/batch_64_texts/` — criterion HTML report
- `target/bench-out/embed.log` — captured stdout (host w64devkit)
- Obs #2655 — `fix/ort-onnx-version-mismatch` (vendored onnxruntime.dll 1.28.0)
- Obs #2657 — previous benchmark run (embed skipped)
