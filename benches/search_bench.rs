//! T-103 — warm search latency bench (REQ-MR-007).
//!
//! Builds the reference corpus (default 100k chunks over a real LanceDB
//! tempdir store, deterministic stub embedder) and measures warm
//! `memory.search` latency:
//!
//! * **Gate** (checked by `scripts/bench.sh`): p50 < 20 ms and p99 < 100 ms
//!   at 100k chunks — the REQ-MR-007 budgets. 200 warm searches over a
//!   32-query pool, percentiles computed explicitly so the gate never
//!   depends on criterion's reporting format.
//! * **Criterion**: the per-search latency distribution over the same
//!   corpus (median + spread for the report).
//! * **Cold start** (REQ-MR-007 "cold start SHOULD be < 3 s"): reported
//!   separately — a fresh process-level open of the populated store plus
//!   the first search. Reported, not gated (SHOULD).
//!
//! The corpus size follows `MEMENTO_BENCH_CHUNKS` (default 100_000); the
//! actual chunk count is reported with the gate so a scaled-down run is
//! never mistaken for the reference measurement.

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use memento_application::AppService;
use memento_domain::TenantContext;
use memento_ports::SearchQuery;
use memento_testkit::TempStore;
use serde_json::json;

#[path = "common/mod.rs"]
mod common;

use common::{app, bench_chunks, corpus_docs, percentile, queries, report, timed};

/// Warm search gate: 200 searches over the 32-query pool, explicit
/// p50/p99 against the REQ-MR-007 budgets. Runs on the bench thread inside
/// the provided runtime.
fn measure_search_gate(
    rt: &tokio::runtime::Runtime,
    app: &AppService,
    ctx: &TenantContext,
    qs: &[String],
    chunks: usize,
) {
    // 5 warmup rounds so the store page cache settles before measurement.
    for _ in 0..5 {
        for q in qs {
            let _ = rt.block_on(app.search(ctx, search_query(ctx, q, 20)));
        }
    }
    let mut latencies: Vec<f64> = Vec::with_capacity(qs.len() * 6);
    for _ in 0..6 {
        for q in qs {
            let (elapsed, hits) = timed(|| rt.block_on(app.search(ctx, search_query(ctx, q, 20))));
            let hits = hits.expect("warm search succeeds");
            std::hint::black_box(hits);
            latencies.push(elapsed.as_secs_f64() * 1e3);
        }
    }
    latencies.sort_by(|a, b| a.total_cmp(b));
    let p50 = percentile(&latencies, 0.50);
    let p99 = percentile(&latencies, 0.99);
    report(
        "gate_search",
        json!({
            "chunks": chunks,
            "searches": latencies.len(),
            "p50_ms": p50,
            "p99_ms": p99,
            "budget_p50_ms": 20.0,
            "budget_p99_ms": 100.0,
            "corpus_is_reference": chunks >= 100_000,
        }),
    );
}

/// Cold-start report (REQ-MR-007 "cold start SHOULD be < 3 s"): a FRESH
/// seeded store — the process-equivalent open (store + schema ensure +
/// audit log) plus the first search. Runs on a separate tempdir so the
/// measured store is never double-opened (Windows file locks). Reported,
/// not gated.
fn measure_cold_start(ctx: &TenantContext) {
    let ts = TempStore::new();
    let root = ts.root().to_path_buf();
    let rt = tokio::runtime::Runtime::new().expect("cold-start runtime");
    {
        let seed = app(&root, ctx);
        let _ = rt.block_on(seed.ingest_text(
            ctx,
            memento_ports::IngestTextRequest {
                text: common::doc_text(5, 1),
                doc_id: None,
                metadata: None,
            },
        ));
    } // seed app dropped: store closed before the measured open

    let (open, opened) = timed(|| app(&root, ctx));
    let app = opened;
    let (first, hits) =
        timed(|| rt.block_on(app.search(ctx, search_query(ctx, "memoria río", 10))));
    let hits = hits.expect("first search succeeds");
    std::hint::black_box(hits);
    drop(app);
    report(
        "gate_cold_start",
        json!({
            "store_open_ms": open.as_secs_f64() * 1e3,
            "first_search_ms": first.as_secs_f64() * 1e3,
            "budget_seconds": 3.0,
        }),
    );
}

fn search_query(ctx: &TenantContext, query: &str, top_k: usize) -> SearchQuery {
    SearchQuery::new(query.to_string(), top_k, *ctx.workspace_id())
}

fn bench_search(c: &mut Criterion) {
    let chunks = bench_chunks();
    let ts = TempStore::new();
    let root = ts.root().to_path_buf();
    let ctx = ts.ctx();
    let rt = tokio::runtime::Runtime::new().expect("bench tokio runtime");
    let app = Arc::new(app(&root, &ctx));

    // Corpus build: docs of ~40 chunks each, ingested through the REAL
    // pipeline (chunk → embed → single batch add). The build time is
    // reported with the gate so the corpus is reproducible.
    let docs = corpus_docs(chunks, chunks / 40);
    let (build, total) = timed(|| {
        let mut total = 0usize;
        for doc in &docs {
            let res = rt
                .block_on(app.ingest_text(
                    &ctx,
                    memento_ports::IngestTextRequest {
                        text: doc.clone(),
                        doc_id: None,
                        metadata: None,
                    },
                ))
                .expect("corpus ingest succeeds");
            total += res.chunk_ids.len();
        }
        total
    });
    let qs = queries();

    // Criterion: per-search latency distribution (median + spread).
    let mut group = c.benchmark_group("search");
    group.throughput(Throughput::Elements(1));
    group.sample_size(50);
    group.bench_with_input(
        BenchmarkId::new("warm", format!("{total} chunks")),
        &qs,
        |b, qs| {
            b.iter(|| {
                let q = &qs[0]; // representative query; gate covers the pool
                let hits = rt.block_on(app.search(&ctx, search_query(&ctx, q, 20)));
                std::hint::black_box(hits.expect("search succeeds"));
            })
        },
    );
    group.finish();

    measure_search_gate(&rt, &app, &ctx, &qs, total);
    report(
        "gate_corpus_build",
        json!({
            "docs": docs.len(),
            "chunks": total,
            "build_s": build.as_secs_f64(),
        }),
    );
    measure_cold_start(&ctx);
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
