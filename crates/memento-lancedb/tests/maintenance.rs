//! T-022 acceptance: purge chain — deleted rows unrecoverable incl.
//! old-version inspection (REQ-ML-004, REQ-CG-001, discovery 2573).

use chrono::{Duration, Utc};
use memento_domain::{ChunkId, DocId, MemoryChunk, Provenance, SourceKind, WorkspaceId};
use memento_lancedb::{
    LanceStore, add_chunks_batch, compact, delete_chunks, delete_doc, delete_tenant,
    delete_workspace, full_text_search, list_versions, prune, sweep_expired, version_snapshot,
};
use memento_ports::{DeleteScope, LifecyclePort, SearchPort, SearchQuery};
use memento_testkit::{TempStore, deterministic_embed};

fn chunk_at(
    ts: &TempStore,
    text: &str,
    workspace_id: WorkspaceId,
    doc_id: DocId,
    created_at: chrono::DateTime<Utc>,
) -> MemoryChunk {
    let tenant_id = *ts.tenant_id();
    let agent_id = ts.agent_id().clone();
    let chunk_id = ChunkId::new();
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

fn chunk(ts: &TempStore, text: &str, ws: WorkspaceId, doc: DocId) -> MemoryChunk {
    chunk_at(ts, text, ws, doc, Utc::now())
}

async fn open_store(ts: &TempStore) -> LanceStore {
    let store = LanceStore::open(&ts.ctx(), ts.root()).await.expect("open");
    store.ensure_schema().await.expect("schema");
    store
}

async fn insert(store: &LanceStore, ts: &TempStore, chunks: &[MemoryChunk]) {
    add_chunks_batch(store, &ts.ctx(), chunks)
        .await
        .expect("insert");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_by_chunk_id_removes_all_kinds() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let doc = DocId::new();

    let chunks = vec![
        chunk(&ts, "uno", ws, doc),
        chunk(&ts, "dos", ws, doc),
        chunk(&ts, "tres", ws, doc),
    ];
    insert(&store, &ts, &chunks).await;

    let report = delete_chunks(&store, &ts.ctx(), &[chunks[1].id])
        .await
        .expect("delete");
    assert_eq!(report.deleted_count, 1);

    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 2);
    let gone = SearchPort::get_chunk(&store, &ts.ctx(), &chunks[1].id)
        .await
        .expect("get");
    assert!(gone.is_none(), "deleted chunk must be gone");

    // Siblings survive.
    assert!(
        SearchPort::get_chunk(&store, &ts.ctx(), &chunks[0].id)
            .await
            .expect("get")
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_by_doc_id_removes_only_that_doc() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let doc_a = DocId::new();
    let doc_b = DocId::new();

    let chunks = vec![
        chunk(&ts, "doc a: primero", ws, doc_a),
        chunk(&ts, "doc a: segundo", ws, doc_a),
        chunk(&ts, "doc b: único", ws, doc_b),
    ];
    insert(&store, &ts, &chunks).await;

    let report = delete_doc(&store, &ts.ctx(), &doc_a)
        .await
        .expect("delete doc");
    assert_eq!(report.deleted_count, 2);

    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 1);
    let hits = full_text_search(&store, &ts.ctx(), "doc", &ws, 10, None)
        .await
        .expect("search");
    assert_eq!(hits.len(), 1, "only doc b's chunk remains");
    assert_eq!(hits[0].chunk_id, chunks[2].id);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_by_workspace_isolates_workspaces() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws_a = *ts.workspace_id();
    let ws_b = WorkspaceId::new();

    let chunks = vec![
        chunk(&ts, "presupuesto equipo", ws_a, DocId::new()),
        chunk(&ts, "presupuesto marketing", ws_b, DocId::new()),
    ];
    insert(&store, &ts, &chunks).await;

    let report = delete_workspace(&store, &ts.ctx(), &ws_b)
        .await
        .expect("delete ws");
    assert_eq!(report.deleted_count, 1);

    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 1);
    let hits = full_text_search(&store, &ts.ctx(), "presupuesto", &ws_a, 10, None)
        .await
        .expect("search");
    assert_eq!(hits.len(), 1, "workspace A untouched");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_by_tenant_is_complete() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();

    let chunks = vec![
        chunk(&ts, "uno", ws, DocId::new()),
        chunk(&ts, "dos", ws, DocId::new()),
    ];
    insert(&store, &ts, &chunks).await;

    let report = delete_tenant(&store, &ts.ctx())
        .await
        .expect("delete tenant");
    assert_eq!(report.deleted_count, 2);
    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 0);

    // Erase path via the LifecyclePort with the tenant scope.
    let ts2 = TempStore::new();
    let store2 = open_store(&ts2).await;
    insert(
        &store2,
        &ts2,
        &[chunk(&ts2, "otro", *ts2.workspace_id(), DocId::new())],
    )
    .await;
    let report = LifecyclePort::delete(
        &store2,
        &ts2.ctx(),
        DeleteScope::Tenant {
            id: *ts2.tenant_id(),
        },
    )
    .await
    .expect("port delete tenant");
    assert_eq!(report.deleted_count, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_chain_makes_data_unrecoverable() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let doc = DocId::new();

    let chunks = vec![
        chunk(&ts, "secreto uno", ws, doc),
        chunk(&ts, "secreto dos", ws, doc),
    ];
    insert(&store, &ts, &chunks).await;

    // Snapshot the version BEFORE the purge chain: the deleted rows must be
    // visible there (time travel) — this is why delete alone is NOT erasure.
    let versions_before = list_versions(&store, &ts.ctx()).await.expect("versions");
    assert!(
        versions_before.len() >= 2,
        "insert + add = at least 2 versions"
    );

    delete_chunks(&store, &ts.ctx(), &[chunks[0].id, chunks[1].id])
        .await
        .expect("delete");
    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 0);

    // Pre-prune: the pre-delete version still contains the rows (GDPR hazard).
    // versions_before = [v1 (empty table), v2 (insert)] → the data version is
    // the LAST one.
    let old = versions_before.last().expect("data version");
    let snapshot = version_snapshot(&store, &ts.ctx(), old.version)
        .await
        .expect("checkout pre-delete version");
    assert!(
        snapshot.iter().any(|c| c.id == chunks[0].id),
        "deleted rows still recoverable in old versions before prune"
    );

    // The purge chain: delete → compact → prune (discovery 2573).
    compact(&store, &ts.ctx()).await.expect("compact");
    prune(&store, &ts.ctx()).await.expect("prune");

    // Only the current version remains.
    let versions_after = list_versions(&store, &ts.ctx()).await.expect("versions");
    assert_eq!(
        versions_after.len(),
        1,
        "all old versions pruned: {versions_after:?}"
    );

    // The old version can no longer be checked out — unrecoverable.
    let err = version_snapshot(&store, &ts.ctx(), old.version)
        .await
        .expect_err("pruned version must be unreachable");
    assert!(
        !err.code().is_empty(),
        "structured error expected (got {err:?})"
    );

    // And the current data is empty.
    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn compact_does_not_lose_active_data() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let doc = DocId::new();

    let chunks = vec![
        chunk(&ts, "dato activo uno", ws, doc),
        chunk(&ts, "dato activo dos", ws, doc),
    ];
    insert(&store, &ts, &chunks).await;

    compact(&store, &ts.ctx()).await.expect("compact");

    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 2);
    let hits = full_text_search(&store, &ts.ctx(), "activo", &ws, 10, None)
        .await
        .expect("search");
    assert_eq!(hits.len(), 2, "search still finds everything after compact");
}

#[tokio::test(flavor = "multi_thread")]
async fn prune_retains_minimum_versions() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let doc = DocId::new();

    insert(&store, &ts, &[chunk(&ts, "primera versión", ws, doc)]).await;
    insert(&store, &ts, &[chunk(&ts, "segunda versión", ws, doc)]).await;

    let before = list_versions(&store, &ts.ctx()).await.expect("versions");
    assert!(before.len() > 1, "two inserts must create two versions");

    prune(&store, &ts.ctx()).await.expect("prune");

    let after = list_versions(&store, &ts.ctx()).await.expect("versions");
    assert_eq!(after.len(), 1, "only the latest version survives");

    // Latest data still queryable.
    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_expired_removes_old_keeps_new() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();
    let now = Utc::now();

    let old = chunk_at(
        &ts,
        "reliquia del pasado",
        ws,
        DocId::new(),
        now - Duration::days(40),
    );
    let fresh = chunk_at(&ts, "noticia reciente", ws, DocId::new(), now);
    insert(&store, &ts, &[old, fresh]).await;

    let cutoff = now - Duration::days(30);
    let report = sweep_expired(&store, &ts.ctx(), cutoff)
        .await
        .expect("sweep");
    assert_eq!(report.expired_count, 1);

    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 1);
    let hits = full_text_search(&store, &ts.ctx(), "noticia", &ws, 10, None)
        .await
        .expect("search");
    assert_eq!(hits.len(), 1, "fresh chunk survives the sweep");
}

#[tokio::test(flavor = "multi_thread")]
async fn cross_tenant_delete_scope_is_forbidden() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;

    let other = TempStore::new();
    let err = LifecyclePort::delete(
        &store,
        &ts.ctx(),
        DeleteScope::Tenant {
            id: *other.tenant_id(),
        },
    )
    .await
    .expect_err("foreign tenant scope must fail");
    assert_eq!(err.code(), memento_domain::error::CODE_TENANT_FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn erase_chain_leaves_zero_searchable() {
    let ts = TempStore::new();
    let store = open_store(&ts).await;
    let ws = *ts.workspace_id();

    insert(
        &store,
        &ts,
        &[chunk(&ts, "dato a erradicar", ws, DocId::new())],
    )
    .await;

    let report = LifecyclePort::erase(&store, &ts.ctx())
        .await
        .expect("erase");
    assert_eq!(report.deleted_count, 1);

    // Searches are zero after the full chain.
    let hits = SearchPort::search(&store, &ts.ctx(), SearchQuery::new("erradicar", 10, ws))
        .await
        .expect("search");
    assert!(hits.is_empty());
    assert_eq!(store.count_chunks(&ts.ctx()).await.expect("count"), 0);

    // Old versions are gone: nothing recoverable.
    let versions = list_versions(&store, &ts.ctx()).await.expect("versions");
    assert_eq!(versions.len(), 1);
}
