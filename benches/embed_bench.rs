//! T-103 — embedding latency bench (informational; no spec budget).
//!
//! Measures the REAL fastembed backend (`FastEmbedBackend`, MultilingualE5
//! Small, 384 dims) at the production batch size (64 texts, T-024
//! boundary). The model downloads on first run (~500 MB, documented risk
//! R1); the bench SKIPS honestly when the model is not cached and
//! `MEMENTO_BENCH_EMBED` is not set — a skipped embed bench never pretends
//! to be a measurement.
//!
//! Model cache: `MEMENTO_MODELS_DIR` (default `<MEMENTO_ROOT>/models`, the
//! production D8 layout). Bench.sh checks for a cached `.onnx` file before
//! deciding to run or skip.

use std::path::PathBuf;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use memento_embed_fastembed::{EmbeddingBackend, FastEmbedBackend};
use serde_json::json;

#[path = "common/mod.rs"]
mod common;

use common::{percentile, report};

const TEXTS_PER_BATCH: usize = 64;
const MEASURED_BATCHES: usize = 16;

/// A deterministic Spanish-ish pseudo-corpus of ~270 tokens per text — the
/// same size as a stored chunk (T-032 bounds), so the measured cost is the
/// real per-chunk embedding cost.
fn batch_texts(salt: usize) -> Vec<String> {
    let sentence = "la memoria es un río subterráneo que fluye entre documentos antiguos y nuevos de la historia de la región ";
    (0..TEXTS_PER_BATCH)
        .map(|i| {
            let mut s = String::with_capacity(1400);
            for j in 0..28 {
                s.push_str(sentence);
                s.push_str(&(i + salt + j).to_string());
                s.push(' ');
            }
            s
        })
        .collect()
}

fn models_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MEMENTO_MODELS_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(root) = std::env::var("MEMENTO_ROOT") {
        return PathBuf::from(root).join("models");
    }
    // Bench-local cache fallback (never a real ~/.memento).
    std::env::temp_dir().join("memento-bench-models")
}

/// Is the ONNX model already cached under `dir`? fastembed caches the hub
/// snapshot somewhere below the cache dir; a recursive `.onnx` probe is the
/// honest presence check.
fn model_cached(dir: &PathBuf) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut stack: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            if let Ok(children) = std::fs::read_dir(&path) {
                stack.extend(children.filter_map(|e| e.ok().map(|e| e.path())));
            }
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("onnx"))
        {
            return true;
        }
    }
    false
}

fn bench_embed(c: &mut Criterion) {
    let dir = models_dir();
    let force = std::env::var("MEMENTO_BENCH_EMBED").is_ok();
    if !model_cached(&dir) && !force {
        report(
            "gate_embed",
            json!({
                "status": "skipped",
                "reason": "model not cached under MEMENTO_MODELS_DIR; run with MEMENTO_BENCH_EMBED=1 to download (first run ~500MB)",
            }),
        );
        return;
    }

    let backend = FastEmbedBackend::try_new(dir).expect("fastembed initializes");
    let mut batch_latencies: Vec<f64> = Vec::with_capacity(MEASURED_BATCHES);

    // Warmup: 2 batches before any measurement.
    for salt in 0..2 {
        let owned = batch_texts(salt);
        let texts: Vec<&str> = owned.iter().map(String::as_str).collect();
        let _ = backend.embed_batch(&texts).expect("warmup batch embeds");
    }

    for salt in 2..(2 + MEASURED_BATCHES) {
        let owned = batch_texts(salt);
        let texts: Vec<&str> = owned.iter().map(String::as_str).collect();
        let start = std::time::Instant::now();
        let vectors = backend.embed_batch(&texts).expect("measured batch embeds");
        std::hint::black_box(vectors);
        batch_latencies.push(start.elapsed().as_secs_f64() * 1e3);
    }
    batch_latencies.sort_by(|a, b| a.total_cmp(b));
    let p50 = percentile(&batch_latencies, 0.50);
    let p99 = percentile(&batch_latencies, 0.99);
    report(
        "gate_embed",
        json!({
            "status": "ok",
            "texts_per_batch": TEXTS_PER_BATCH,
            "batches": MEASURED_BATCHES,
            "p50_ms_per_batch": p50,
            "p99_ms_per_batch": p99,
            "p50_ms_per_chunk": p50 / TEXTS_PER_BATCH as f64,
        }),
    );

    let mut group = c.benchmark_group("embed");
    group.throughput(Throughput::Elements(TEXTS_PER_BATCH as u64));
    group.sample_size(20);
    group.bench_function("batch_64_texts", |b| {
        let mut salt = 1000;
        b.iter_batched(
            || {
                salt += 1;
                batch_texts(salt)
            },
            |texts| {
                let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
                let vectors = backend.embed_batch(&refs);
                std::hint::black_box(vectors.expect("batch embeds"));
            },
            criterion::BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_embed);
criterion_main!(benches);
