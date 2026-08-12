//! IR golden-set gate (integration test).
//!
//! Ingests the synthetic corpus (`tests/fixtures/ir-corpus/`) into a fresh
//! temp tenant, runs every golden query through the RRF hybrid path with
//! `top_k = 5` and asserts MRR@5 / Recall@5 against the mode thresholds.
//!
//! * Default: stub embeddings (deterministic, no model download) — enforces
//!   [`memento_ir_gate::STUB_MRR5`] / [`memento_ir_gate::STUB_RECALL5`].
//! * `MEMENTO_IR_GATE=1`: the real int8 MultilingualE5Base — enforces
//!   [`memento_ir_gate::REAL_MRR5`] / [`memento_ir_gate::REAL_RECALL5`]. The
//!   model must exist at `../../models/int8/multilingual-e5-base-int8/`
//!   relative to this crate (see `scripts/provision_int8.py`).

use async_trait::async_trait;
use memento_application::{AppService, SystemClock};
use memento_domain::{DomainError, SourceKind};
use memento_embed_fastembed::{FastEmbedEmbedder, FastReranker, ModelLoader, Reranker};
use memento_ir_gate::{CORPUS_DIR, GoldenQuery, is_relevant, load_golden_set, mrr_at_5};
use memento_ports::{
    DEFAULT_RRF_K, EmbedPort, IngestTextRequest, Metadata, ParsePort, ParsedDocument, RerankPort,
    SearchQuery,
};
use memento_testkit::{StubEmbedPort, TempStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Parse boundary that is never invoked: the gate ingests raw text
/// (`ingest_text`), never documents, so the parse port is never called.
struct NeverParse;

#[async_trait]
impl ParsePort for NeverParse {
    async fn parse(&self, _blob: &[u8], _hint: SourceKind) -> Result<ParsedDocument, DomainError> {
        Err(DomainError::Parse {
            message: "ir-gate never parses documents".into(),
        })
    }
}

/// Path of the int8 model relative to this crate (`crates/memento-ir-gate` →
/// repo root `models/int8/multilingual-e5-base-int8/model.onnx`).
fn int8_model_path() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("..")
        .join("..")
        .join("models")
        .join("int8")
        .join("multilingual-e5-base-int8")
        .join("model.onnx")
}

/// Real embedder for `MEMENTO_IR_GATE=1`: points `MEMENTO_QUANTIZED_MODEL`
/// at the int8 model so `FastEmbedBackend::try_new` loads it from disk (no
/// HF download). Fails loudly when the file is missing.
fn real_embedder() -> Arc<dyn EmbedPort> {
    let model = int8_model_path();
    assert!(
        model.is_file(),
        "MEMENTO_IR_GATE=1 requires the int8 model at {} — run \
         `python crates/memento-ir-gate/scripts/provision_int8.py` first",
        model.display()
    );
    // SAFETY: test-only; single process, set before any embedder construction.
    unsafe {
        std::env::set_var("MEMENTO_QUANTIZED_MODEL", &model);
    }
    // The int8 user-defined path bypasses the fastembed cache dir entirely;
    // point it at the repo models/ dir for consistency with the CLI.
    let cache_dir = model.parent().unwrap().parent().unwrap().parent().unwrap();
    let loader = ModelLoader::new(cache_dir.to_path_buf(), true);
    Arc::new(FastEmbedEmbedder::new(Arc::new(loader)))
}

/// Path of the int8 cross-encoder reranker relative to this crate
/// (`crates/memento-ir-gate` → repo root
/// `models/int8/bge-reranker-v2-m3-int8/model.onnx`).
fn rerank_model_path() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("..")
        .join("..")
        .join("models")
        .join("int8")
        .join("bge-reranker-v2-m3-int8")
        .join("model.onnx")
}

/// Real reranker for the rerank gate (A1): sets the `MEMENTO_RERANK`
/// capability + model path so the lazy `Reranker` loads the int8
/// bge-reranker-v2-m3 from disk on the first opt-in query. Fails loudly when
/// the file is missing.
fn real_reranker() -> Arc<dyn RerankPort> {
    let model = rerank_model_path();
    assert!(
        model.is_file(),
        "rerank gate requires the int8 reranker at {} — run the quantize \
         script (quantize_bge_reranker.py) first",
        model.display()
    );
    // SAFETY: test-only; single process, set before any Reranker construction.
    unsafe {
        std::env::set_var("MEMENTO_RERANK", "1");
        std::env::set_var("MEMENTO_RERANK_MODEL", &model);
    }
    Arc::new(FastReranker::new(Arc::new(Reranker::new(
        std::env::temp_dir(),
    ))))
}

/// Ingest every `.md` file in the synthetic corpus into the temp tenant.
async fn ingest_corpus(app: &AppService, ts: &TempStore) {
    let corpus_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS_DIR);
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&corpus_dir)
        .expect("corpus dir must exist")
        .map(|e| e.expect("corpus entry").path())
        .filter(|p| p.extension().map(|e| e == "md").unwrap_or(false))
        .collect();
    entries.sort();
    assert!(
        entries.len() >= 12,
        "corpus must hold at least 12 synthetic docs, got {}",
        entries.len()
    );
    for path in entries {
        let text = std::fs::read_to_string(&path).expect("read fixture doc");
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut meta = serde_json::Map::new();
        meta.insert(
            "title".to_string(),
            serde_json::Value::String(title.clone()),
        );
        app.ingest_text(
            &ts.ctx(),
            IngestTextRequest {
                text,
                doc_id: None,
                metadata: Some(Metadata(meta)),
            },
        )
        .await
        .expect("ingest fixture doc");
    }
}

/// Run every golden query through the RRF hybrid path at `rrf_k` and return
/// the aggregate (MRR@5, Recall@5). The query-embed cache makes repeated k
/// sweeps cheap: each distinct query text embeds exactly once. With
/// `rerank = true` every query also opts into the cross-encoder rerank (A1)
/// — requires the reranker capability on the app.
async fn score_golden(
    app: &AppService,
    ts: &TempStore,
    rrf_k: f32,
    rerank: bool,
    print_rows: bool,
) -> (f32, f32) {
    let golden: Vec<GoldenQuery> = load_golden_set();
    let mut total_mrr = 0.0f32;
    let mut recall_hits = 0u32;

    for q in &golden {
        let mut query = SearchQuery::new(q.query.clone(), 5, *ts.workspace_id());
        query.rrf_enabled = true;
        query.rrf_k = rrf_k;
        query.rerank = rerank;
        let hits = app
            .search(&ts.ctx(), query)
            .await
            .expect("golden query search must succeed");
        let texts: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        let mrr = mrr_at_5(&texts, &q.expected_fragments);
        let in_top5 = texts
            .iter()
            .take(5)
            .any(|t| is_relevant(t, &q.expected_fragments));
        total_mrr += mrr;
        recall_hits += u32::from(in_top5);
        if print_rows {
            println!(
                "{:<10} {:<58} MRR={:.3} top5={}",
                q.id, q.query, mrr, in_top5
            );
        }
    }

    let n = golden.len() as f32;
    (total_mrr / n, recall_hits as f32 / n)
}

/// MRR@5 / Recall@5 sweep over RRF fusion constants. Manual run (not CI):
///
/// ```sh
/// cargo test -p memento-ir-gate --test ir_gate rrf_k_sweep -- --ignored --nocapture
/// MEMENTO_IR_GATE=1 cargo test -p memento-ir-gate --test ir_gate rrf_k_sweep -- --ignored --nocapture
/// ```
///
/// Sweeps `[30, 60, 100, 150]` (or `RRF_K_SWEEP=45,75` comma-separated overrides)
/// and prints the evidence table. The real int8 model (`MEMENTO_IR_GATE=1`) is
/// the source of truth for Spanish tuning.
#[tokio::test]
#[ignore = "manual sweep: rrf_k_sweep -- --ignored --nocapture"]
async fn rrf_k_sweep() {
    let real = std::env::var_os("MEMENTO_IR_GATE").is_some();
    let ts = TempStore::new();

    let embedder: Arc<dyn EmbedPort> = if real {
        real_embedder()
    } else {
        Arc::new(StubEmbedPort::default())
    };

    let app = AppService::open(
        &ts.ctx(),
        ts.root(),
        Arc::new(NeverParse),
        Some(embedder),
        Arc::new(SystemClock),
    )
    .await
    .expect("open app service");

    ingest_corpus(&app, &ts).await;

    let mut ks = vec![30.0f32, 60.0, 100.0, 150.0];
    if let Ok(raw) = std::env::var("RRF_K_SWEEP") {
        ks = raw
            .split(',')
            .map(|s| {
                s.trim()
                    .parse()
                    .expect("RRF_K_SWEEP must be comma-separated floats")
            })
            .collect();
    }
    // RRF_SWEEP_ROWS=1 prints every (query, MRR, top5) row per k.
    let rows = std::env::var_os("RRF_SWEEP_ROWS").is_some();

    let n = load_golden_set().len();
    println!(
        "\n[ir-gate-sweep] mode={} queries={} rrf-k sweep",
        if real { "int8" } else { "stub" },
        n
    );
    println!("{:<8} {:<10} {:<10}", "rrf_k", "MRR@5", "Recall@5");
    for k in &ks {
        let (mrr5, recall5) = score_golden(&app, &ts, *k, false, rows).await;
        println!("{:<8.0} {:<10.4} {:<10.4}", k, mrr5, recall5);
    }
}

#[tokio::test]
async fn ir_gate() {
    let real = std::env::var_os("MEMENTO_IR_GATE").is_some();
    let ts = TempStore::new();

    let embedder: Arc<dyn EmbedPort> = if real {
        real_embedder()
    } else {
        Arc::new(StubEmbedPort::default())
    };

    let app = AppService::open(
        &ts.ctx(),
        ts.root(),
        Arc::new(NeverParse),
        Some(embedder),
        Arc::new(SystemClock),
    )
    .await
    .expect("open app service");

    ingest_corpus(&app, &ts).await;

    let golden: Vec<GoldenQuery> = load_golden_set();
    let (mrr5, recall5) = score_golden(&app, &ts, DEFAULT_RRF_K, false, true).await;
    let (mrr_thr, recall_thr) = if real {
        (memento_ir_gate::REAL_MRR5, memento_ir_gate::REAL_RECALL5)
    } else {
        (memento_ir_gate::STUB_MRR5, memento_ir_gate::STUB_RECALL5)
    };

    println!(
        "\n[ir-gate] mode={} queries={} MRR@5={:.4} Recall@5={:.4} (thresholds {:.2}/{:.2})",
        if real { "int8" } else { "stub" },
        golden.len(),
        mrr5,
        recall5,
        mrr_thr,
        recall_thr
    );

    assert!(
        mrr5 >= mrr_thr,
        "MRR@5 {mrr5:.4} below threshold {mrr_thr:.2} — retrieval degraded"
    );
    assert!(
        recall5 >= recall_thr,
        "Recall@5 {recall5:.4} below threshold {recall_thr:.2} — relevant chunks lost"
    );
}

/// A1 rerank gate: the full 50-query golden set through the REAL int8
/// embedder + REAL int8 cross-encoder reranker (`rerank: true` on every
/// query). Manual run (NOT CI — the ~543 MB reranker model + per-query
/// inference make it too slow for the fast test job):
///
/// ```sh
/// cargo test -p memento-ir-gate --test ir_gate rerank_ir_gate -- --ignored --nocapture
/// ```
///
/// Env required: `MEMENTO_IR_GATE=1` (real embedder), `MEMENTO_RERANK=1`
/// (rerank capability; also set by the test from `MEMENTO_RERANK_MODEL`).
#[tokio::test]
#[ignore = "manual rerank gate: rerank_ir_gate -- --ignored --nocapture (requires the real reranker model)"]
async fn rerank_ir_gate() {
    let real = std::env::var_os("MEMENTO_IR_GATE").is_some();
    assert!(
        real,
        "rerank gate is a real-model gate: run with MEMENTO_IR_GATE=1 (see test docs)"
    );
    let ts = TempStore::new();

    let app = AppService::open(
        &ts.ctx(),
        ts.root(),
        Arc::new(NeverParse),
        Some(real_embedder()),
        Arc::new(SystemClock),
    )
    .await
    .expect("open app service")
    .with_reranker(real_reranker());

    ingest_corpus(&app, &ts).await;

    let golden: Vec<GoldenQuery> = load_golden_set();
    let (mrr5, recall5) = score_golden(&app, &ts, DEFAULT_RRF_K, true, true).await;

    println!(
        "\n[ir-gate-rerank] queries={} MRR@5={:.4} Recall@5={:.4} (thresholds {:.2}/{:.2})",
        golden.len(),
        mrr5,
        recall5,
        memento_ir_gate::REAL_MRR5,
        memento_ir_gate::REAL_RECALL5
    );

    assert!(
        mrr5 >= memento_ir_gate::REAL_MRR5,
        "rerank MRR@5 {mrr5:.4} below threshold {:.2}",
        memento_ir_gate::REAL_MRR5
    );
    assert!(
        recall5 >= memento_ir_gate::REAL_RECALL5,
        "rerank Recall@5 {recall5:.4} below threshold {:.2}",
        memento_ir_gate::REAL_RECALL5
    );
}

/// A1 adoption-bar comparison: the 50-query golden set on the REAL int8
/// model, WITH and WITHOUT the cross-encoder reranker, reporting both
/// MRR@5/Recall@5. Manual run (real models, slow):
///
/// ```sh
/// MEMENTO_IR_GATE=1 cargo test -p memento-ir-gate --test ir_gate rerank_comparison -- --ignored --nocapture
/// ```
///
/// Adoption bar: rerank must NOT drop MRR@5 below the no-rerank baseline
/// (within a small epsilon for run-to-run variance).
#[tokio::test]
#[ignore = "manual baseline comparison: rerank_comparison -- --ignored --nocapture"]
async fn rerank_comparison() {
    let real = std::env::var_os("MEMENTO_IR_GATE").is_some();
    assert!(
        real,
        "rerank comparison is a real-model gate: run with MEMENTO_IR_GATE=1 (see test docs)"
    );
    let ts = TempStore::new();

    let app = AppService::open(
        &ts.ctx(),
        ts.root(),
        Arc::new(NeverParse),
        Some(real_embedder()),
        Arc::new(SystemClock),
    )
    .await
    .expect("open app service")
    .with_reranker(real_reranker());

    ingest_corpus(&app, &ts).await;

    let t0 = std::time::Instant::now();
    let (base_mrr, base_recall) = score_golden(&app, &ts, DEFAULT_RRF_K, false, false).await;
    let base_elapsed = t0.elapsed();
    let t1 = std::time::Instant::now();
    let (rr_mrr, rr_recall) = score_golden(&app, &ts, DEFAULT_RRF_K, true, false).await;
    let rr_elapsed = t1.elapsed();
    // Both modes share the query-embed cache (warmed by the first run), so the
    // delta per query ≈ the rerank cost of the second run.
    let rr_extra_ms = ((rr_elapsed.as_millis() as f32 - base_elapsed.as_millis() as f32)
        / load_golden_set().len() as f32)
        .max(0.0);

    println!(
        "\n[ir-gate-rerank-comparison] queries={}",
        load_golden_set().len()
    );
    println!(
        "{:<18} {:<10} {:<10} {:<12}",
        "mode", "MRR@5", "Recall@5", "total_s"
    );
    println!(
        "{:<18} {:<10.4} {:<10.4} {:<12.1}",
        "rrf (no rerank)",
        base_mrr,
        base_recall,
        base_elapsed.as_secs_f32()
    );
    println!(
        "{:<18} {:<10.4} {:<10.4} {:<12.1}",
        "rrf + rerank",
        rr_mrr,
        rr_recall,
        rr_elapsed.as_secs_f32()
    );
    println!(
        "{:<18} {:<+10.4} {:<+10.4}",
        "delta",
        rr_mrr - base_mrr,
        rr_recall - base_recall
    );
    println!(
        "[ir-gate-rerank-comparison] ~{rr_extra_ms:.0} ms extra per query (rerank over fused)"
    );

    // Adoption bar: rerank must not drop MRR below the baseline.
    assert!(
        rr_mrr >= base_mrr - 0.01,
        "rerank MRR@5 {rr_mrr:.4} dropped below the no-rerank baseline {base_mrr:.4} — adoption bar not met"
    );
}
