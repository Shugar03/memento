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
use memento_embed_fastembed::{FastEmbedEmbedder, ModelLoader};
use memento_ir_gate::{CORPUS_DIR, GoldenQuery, is_relevant, load_golden_set, mrr_at_5};
use memento_ports::{
    EmbedPort, IngestTextRequest, Metadata, ParsePort, ParsedDocument, SearchQuery,
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
    let mut total_mrr = 0.0f32;
    let mut recall_hits = 0u32;

    for q in &golden {
        let mut query = SearchQuery::new(q.query.clone(), 5, *ts.workspace_id());
        query.rrf_enabled = true;
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
        println!(
            "{:<10} {:<58} MRR={:.3} top5={}",
            q.id, q.query, mrr, in_top5
        );
    }

    let n = golden.len() as f32;
    let mrr5 = total_mrr / n;
    let recall5 = recall_hits as f32 / n;
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
