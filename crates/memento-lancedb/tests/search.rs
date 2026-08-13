//! T-021 acceptance: tempdir insert→search round-trip; foreign chunk →
//! not-found (REQ-MR-005). Real LanceDB on a TempStore.

use chrono::Utc;
use memento_domain::{ChunkId, DocId, MemoryChunk, Provenance, SourceKind, WorkspaceId};
use memento_lancedb::{
    LanceStore, add_chunks_batch, ensure_fts_index, full_text_search, vector_search,
};
use memento_observability::EventSink;
use memento_ports::{DEFAULT_RRF_K, SearchPort, SearchQuery};
use memento_testkit::{TempStore, deterministic_embed};
use std::sync::Arc;

/// Build a chunk with deterministic provenance; `vector` uses the testkit
/// hash-bucketed embedding so tests need no ONNX runtime.
fn chunk(ts: &TempStore, text: &str, workspace_id: WorkspaceId, doc_id: DocId) -> MemoryChunk {
    let tenant_id = *ts.tenant_id();
    let agent_id = ts.agent_id().clone();
    let chunk_id = ChunkId::new();
    let created_at = Utc::now();
    let provenance = Provenance {
        source: SourceKind::Text,
        doc_id,
        chunk_id,
        created_at,
        embedding_model_version: "multilingual-e5-base-v0.0.3".to_string(),
        tenant_id,
        workspace_id,
        agent_id: agent_id.clone(),
    };
    MemoryChunk {
        id: chunk_id,
        tenant_id,
        workspace_id,
        agent_id,
        doc_id,
        text: text.to_string(),
        vector: Some(deterministic_embed(text, 768)),
        created_at,
        provenance,
    }
}

async fn open_store(ts: &TempStore) -> LanceStore {
    let store = LanceStore::open(&ts.ctx(), ts.root())
        .await
        .expect("open store");
    store.ensure_schema().await.expect("ensure schema");
    store
}

#[tokio::test(flavor = "multi_thread")]
async fn insert_then_search_round_trip() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let doc = DocId::new();

    let chunks = vec![
        chunk(&ts, "El río de la memoria nunca deja de fluir.", ws, doc),
        chunk(
            &ts,
            "La documentación es la base del conocimiento organizacional.",
            ws,
            doc,
        ),
        chunk(
            &ts,
            "El derecho al olvido es tan importante como el recuerdo.",
            ws,
            doc,
        ),
    ];
    add_chunks_batch(&store, &ts.ctx(), &chunks)
        .await
        .expect("batch add");

    // BM25: the exact-match document ranks first.
    let hits = full_text_search(&store, &ts.ctx(), "documentación", &ws, 10, None)
        .await
        .expect("fts search");
    assert!(!hits.is_empty(), "expected hits");
    assert_eq!(hits[0].chunk_id, chunks[1].id, "exact match first");

    // Every hit round-trips full provenance (REQ-MC-006).
    for hit in &hits {
        assert_eq!(hit.provenance.tenant_id, *ts.tenant_id());
        assert_eq!(hit.provenance.chunk_id, hit.chunk_id);
        assert!(!hit.provenance.embedding_model_version.is_empty());
    }

    // count reflects the batch.
    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_add_is_atomic_and_round_trips() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let doc = DocId::new();

    let chunks = vec![
        chunk(&ts, "Primer fragmento del documento.", ws, doc),
        chunk(&ts, "Segundo fragmento del documento.", ws, doc),
    ];
    add_chunks_batch(&store, &ts.ctx(), &chunks)
        .await
        .expect("atomic batch add");

    // Both chunks visible after the single add (REQ-MC-007).
    let hits = full_text_search(&store, &ts.ctx(), "documento", &ws, 10, None)
        .await
        .expect("search");
    assert_eq!(hits.len(), 2, "whole batch visible: {hits:?}");

    // get_chunk round-trips the full entity, vector included.
    let got = SearchPort::get_chunk(&store, &ts.ctx(), &chunks[0].id)
        .await
        .expect("get_chunk")
        .expect("chunk exists");
    assert_eq!(got, chunks[0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn foreign_chunk_returns_none() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let doc = DocId::new();

    let c = chunk(&ts, "Chunk del tenant A.", ws, doc);
    add_chunks_batch(&store, &ts.ctx(), std::slice::from_ref(&c))
        .await
        .expect("add");

    // A foreign tenant's store (different dir) cannot see tenant A's chunk.
    let foreign = TempStore::new();
    let foreign_store = open_store(&foreign).await;
    let got = SearchPort::get_chunk(&foreign_store, &foreign.ctx(), &c.id)
        .await
        .expect("get_chunk");
    assert!(got.is_none(), "foreign chunk must not resolve (REQ-MR-005)");
}

#[tokio::test(flavor = "multi_thread")]
async fn vector_search_returns_ranked() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let doc = DocId::new();

    let chunks = vec![
        chunk(&ts, "cerveza artesanal de lúpulo", ws, doc),
        chunk(&ts, "motor de combustión interna", ws, doc),
        chunk(&ts, "fermentación de malta y cebada", ws, doc),
    ];
    add_chunks_batch(&store, &ts.ctx(), &chunks)
        .await
        .expect("add");

    let query_vec = deterministic_embed("cerveza artesanal lúpulo", 768);
    let ranked = vector_search(&store, &ts.ctx(), &query_vec, &ws, 3)
        .await
        .expect("vector search");

    assert_eq!(ranked.len(), 3);
    assert_eq!(ranked[0].0, chunks[0].id, "nearest first");
    // Scores are 1/(1+d) ∈ (0,1], monotonically decreasing.
    assert!(ranked[0].1 > ranked[1].1 && ranked[1].1 > ranked[2].1);
}

#[tokio::test(flavor = "multi_thread")]
async fn search_filters_by_workspace() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws_a = *ts.workspace_id();
    let ws_b = WorkspaceId::new();

    let doc_a = DocId::new();
    let doc_b = DocId::new();
    let chunks = vec![
        chunk(&ts, "presupuesto anual del equipo", ws_a, doc_a),
        chunk(&ts, "presupuesto anual de marketing", ws_b, doc_b),
    ];
    add_chunks_batch(&store, &ts.ctx(), &chunks)
        .await
        .expect("add");

    let hits = full_text_search(&store, &ts.ctx(), "presupuesto", &ws_a, 10, None)
        .await
        .expect("search");
    assert_eq!(hits.len(), 1, "only workspace A's chunk matches: {hits:?}");
    assert_eq!(hits[0].chunk_id, chunks[0].id);

    let hits_b = full_text_search(&store, &ts.ctx(), "presupuesto", &ws_b, 10, None)
        .await
        .expect("search");
    assert_eq!(hits_b.len(), 1);
    assert_eq!(hits_b[0].chunk_id, chunks[1].id);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_query_returns_empty() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();

    let hits = full_text_search(&store, &ts.ctx(), "   ", &ws, 10, None)
        .await
        .expect("empty query is not an error");
    assert!(hits.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn port_search_bounds_top_k() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();

    // SearchPort path with a query built through the port DTO.
    let q = SearchQuery::new("presupuesto", 10, ws);
    let hits = SearchPort::search(&store, &ts.ctx(), q)
        .await
        .expect("port search");
    assert!(hits.is_empty(), "empty store");

    // top_k beyond the cap → structured TOP_K_EXCEEDED.
    let q = SearchQuery::new("x", 101, ws);
    let err = SearchPort::search(&store, &ts.ctx(), q)
        .await
        .expect_err("top_k over cap must fail");
    assert_eq!(err.code(), memento_domain::error::CODE_TOP_K_EXCEEDED);
}

#[tokio::test(flavor = "multi_thread")]
async fn accent_insensitive_search_matches() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let doc = DocId::new();

    // Accented text stored; unaccented query must match (ascii_folding).
    let c = chunk(&ts, "La información debe ser accesible.", ws, doc);
    add_chunks_batch(&store, &ts.ctx(), std::slice::from_ref(&c))
        .await
        .expect("add");

    let hits = full_text_search(&store, &ts.ctx(), "informacion", &ws, 10, None)
        .await
        .expect("search");
    assert_eq!(hits.len(), 1, "accent-folded match expected");
    assert_eq!(hits[0].chunk_id, c.id);
}

#[tokio::test(flavor = "multi_thread")]
async fn hybrid_flag_errors_until_application_layer() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();

    // REQ-MR-003: hybrid needs the application embedding layer; the port
    // surfaces a structured error until T-061 wires the real composition.
    let q = SearchQuery {
        query: "algo".into(),
        top_k: 5,
        workspace_id: ws,
        rrf_enabled: true,
        rrf_k: DEFAULT_RRF_K,
        rerank: false,
        filters: None,
    };
    let err = SearchPort::search(&store, &ts.ctx(), q)
        .await
        .expect_err("hybrid not servable by the port alone");
    assert_eq!(err.code(), memento_domain::error::CODE_INVALID_INPUT);
}

#[tokio::test(flavor = "multi_thread")]
async fn fts_build_appends_event_when_sink_attached() {
    // REQ-OBS-008 (design D5): with an EventSink attached via with_events,
    // the first FTS index build appends one tenant-scoped `fts_build` line
    // (ids+counts: index name + trained row count). A repeated ensure is a
    // no-op — the index exists, so NO second event (idempotent by design).
    let ts = TempStore::new();
    let sink = EventSink::tenant(ts.root(), ts.tenant_id()).expect("sink opens");
    let store = LanceStore::open(&ts.ctx(), ts.root())
        .await
        .expect("open store")
        .with_events(Some(Arc::new(sink)));
    store.ensure_schema().await.expect("ensure schema");
    let ws = *ts.workspace_id();
    let doc = DocId::new();
    let c = chunk(&ts, "El río de la memoria nunca deja de fluir.", ws, doc);
    add_chunks_batch(&store, &ts.ctx(), std::slice::from_ref(&c))
        .await
        .expect("add");

    ensure_fts_index(&store).await.expect("first build");
    ensure_fts_index(&store).await.expect("idempotent (no-op)");

    let path = ts
        .root()
        .join("logs")
        .join(format!("{}.events.jsonl", ts.tenant_id()));
    let raw = std::fs::read_to_string(&path).expect("events file");
    let lines: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(lines.len(), 1, "one fts_build event, idempotent index");
    assert_eq!(lines[0]["action"], "fts_build");
    assert_eq!(lines[0]["outcome"], "ok");
    assert_eq!(lines[0]["tenant_id"], ts.tenant_id().to_string());
    assert_eq!(lines[0]["target"]["index"], "chunks_text_fts");
    assert_eq!(lines[0]["target"]["chunks"], 1, "trained row count");
}
