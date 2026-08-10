//! T-103 — ingest throughput bench (REQ-MR-007 evidence companion).
//!
//! Measures the ingest pipeline (chunk → embed → single batch add,
//! REQ-MC-007) with the deterministic stub embedder:
//!
//! * **Gate** (`gate_ingest`): 10 documents of ~40 chunks each ingested
//!   back-to-back on the same real store — chunks/sec over the whole batch.
//!   No spec budget exists for ingest; the numbers feed the ops docs
//!   (REQ-OP-005 honest numbers) and the batch-12 manifesto update.
//! * **Criterion**: the per-document ingest latency distribution, with
//!   `Throughput::Elements` so criterion also reports chunks/sec.
//!
//! Every measured doc is fresh (salted content) — the tenant-scoped
//! content-hash dedup (REQ-MC-005) must never absorb a measured write.

use std::sync::Arc;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use memento_ports::IngestTextRequest;
use memento_testkit::TempStore;
use serde_json::json;

#[path = "common/mod.rs"]
mod common;

use common::{app, doc_text, percentile, report, timed};

const DOCS_PER_GATE: usize = 10;
const CHUNKS_PER_DOC: usize = 40;

fn bench_ingest(c: &mut Criterion) {
    let ts = TempStore::new();
    let root = ts.root().to_path_buf();
    let ctx = ts.ctx();
    let rt = tokio::runtime::Runtime::new().expect("bench tokio runtime");
    let app = Arc::new(app(&root, &ctx));

    // Gate: 10 fresh docs (~40 chunks each) through the real pipeline.
    let mut per_doc: Vec<f64> = Vec::with_capacity(DOCS_PER_GATE);
    let mut total_chunks = 0usize;
    let (batch, _) = timed(|| {
        for i in 0..DOCS_PER_GATE {
            let text = doc_text(CHUNKS_PER_DOC, 1000 + i);
            let (elapsed, res) = timed(|| {
                rt.block_on(app.ingest_text(
                    &ctx,
                    IngestTextRequest {
                        text,
                        doc_id: None,
                        metadata: None,
                    },
                ))
            });
            let res = res.expect("gate ingest succeeds");
            total_chunks += res.chunk_ids.len();
            per_doc.push(elapsed.as_secs_f64() * 1e3);
        }
    });
    per_doc.sort_by(|a, b| a.total_cmp(b));
    let chunks_per_sec = total_chunks as f64 / batch.as_secs_f64();
    report(
        "gate_ingest",
        json!({
            "phase": "cold_first_10_docs_on_fresh_store",
            "docs": DOCS_PER_GATE,
            "chunks": total_chunks,
            "batch_ms": batch.as_secs_f64() * 1e3,
            "chunks_per_sec": chunks_per_sec,
            "p50_ms_per_doc": percentile(&per_doc, 0.50),
            "p99_ms_per_doc": percentile(&per_doc, 0.99),
            "note": "cold-table ramp; steady-state per-doc latency is the criterion report below",
        }),
    );

    // Criterion: per-document latency over the same pipeline (the doc
    // generator keeps every iteration a real write, not a dedup hit).
    let mut group = c.benchmark_group("ingest");
    group.throughput(Throughput::Elements(CHUNKS_PER_DOC as u64));
    group.sample_size(20);
    group.bench_function("doc_40_chunks", |b| {
        let mut salt = 10_000;
        b.iter_batched(
            || {
                salt += 1;
                doc_text(CHUNKS_PER_DOC, salt)
            },
            |text| {
                let res = rt.block_on(app.ingest_text(
                    &ctx,
                    IngestTextRequest {
                        text,
                        doc_id: None,
                        metadata: None,
                    },
                ));
                std::hint::black_box(res.expect("ingest succeeds"));
            },
            criterion::BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_ingest);
criterion_main!(benches);
