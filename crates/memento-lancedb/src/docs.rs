//! `docs` table access (T-060): document records + the tenant-scoped
//! idempotency probe (REQ-MC-005).
//!
//! Every ingest writes ONE docs row keyed by `doc_id`. The row carries the
//! tenant-scoped content hash of the ingested input (raw text for
//! `ingest_text`, raw blob for `ingest_document`). The dedup probe scans
//! `content_hash` within the bound tenant: a re-ingest of identical content
//! finds the stored doc and the application returns its existing chunk ids —
//! no new chunks are ever created for duplicate content.
//!
//! The hash input embeds the tenant id, so two tenants can never collide even
//! when their content is identical (REQ-MC-005 "same content, different
//! tenant → independent copy" holds by construction).
//!
//! Concurrency note: the probe-then-write sequence is not atomic across
//! concurrent ingests of the same content. The MVP serves one tenant per
//! process with synchronous ingests (REQ-MC-007), so a race would require
//! two simultaneous in-process ingests; the duplicate pair then lands as two
//! docs rows with the same hash and the probe deterministically returns the
//! first one (rows are ordered by insertion).

use crate::schema::{
    CHUNKS, COL_AGENT_ID, COL_CONTENT_HASH, COL_CREATED_AT, COL_DOC_ID, COL_SOURCE, COL_TENANT_ID,
    COL_TITLE, COL_WORKSPACE_ID, docs_schema, tenant_scope, ts_to_nanos,
};
use crate::store::{LanceStore, id_at, map_error, missing_column, string_at};
use futures::TryStreamExt;
use lancedb::arrow::arrow_array::{
    RecordBatch, StringArray, TimestampNanosecondArray, cast::AsArray,
};
use lancedb::expr::{col, lit};
use lancedb::query::{ExecutableQuery, QueryBase};
use memento_domain::{
    AgentId, ChunkId, DocId, DomainError, MemoryChunk, SourceKind, TenantContext, WorkspaceId,
};
use std::sync::Arc;

/// One `docs` table row: ingest metadata + the idempotency key.
#[derive(Debug, Clone, PartialEq)]
pub struct DocRecord {
    pub doc_id: DocId,
    pub tenant_id: memento_domain::TenantId,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub title: Option<String>,
    pub source: SourceKind,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub content_hash: String,
}

/// Insert or replace the docs row for `doc.doc_id` (delete-then-add, same
/// pattern as the symbols mirror): re-ingesting with an explicit doc id
/// refreshes the metadata without leaving stale rows.
pub async fn upsert_doc(
    store: &LanceStore,
    ctx: &TenantContext,
    doc: &DocRecord,
) -> Result<(), DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(crate::schema::DOCS).await?;
    table
        .delete(&tenant_scope(ctx.tenant_id()).and(col(COL_DOC_ID).eq(lit(doc.doc_id.to_string()))))
        .await
        .map_err(|err| map_error("upsert_doc delete", err))?;

    let schema = docs_schema();
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(vec![doc.doc_id.to_string()])),
            Arc::new(StringArray::from(vec![doc.tenant_id.to_string()])),
            Arc::new(StringArray::from(vec![doc.workspace_id.to_string()])),
            Arc::new(StringArray::from(vec![doc.agent_id.as_str()])),
            Arc::new(StringArray::from(vec![
                doc.title.as_deref().unwrap_or_default(),
            ])),
            Arc::new(StringArray::from(vec![
                serde_json::to_string(&doc.source).expect("SourceKind serializes"),
            ])),
            Arc::new(TimestampNanosecondArray::from(vec![Some(ts_to_nanos(
                doc.created_at,
            ))])),
            Arc::new(StringArray::from(vec![doc.content_hash.clone()])),
        ],
    )
    .map_err(|err| DomainError::Internal {
        message: format!("build docs batch: {err}"),
    })?;

    table
        .add(batch)
        .execute()
        .await
        .map_err(|err| map_error("upsert_doc add", err))?;
    Ok(())
}

/// The idempotency probe (REQ-MC-005): first docs row of this tenant whose
/// content hash equals `content_hash`, or `None`.
pub async fn find_doc_by_hash(
    store: &LanceStore,
    ctx: &TenantContext,
    content_hash: &str,
) -> Result<Option<DocRecord>, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(crate::schema::DOCS).await?;
    let filter = tenant_scope(ctx.tenant_id()).and(col(COL_CONTENT_HASH).eq(lit(content_hash)));
    let stream = table
        .query()
        .only_if_expr(filter)
        .limit(1)
        .execute()
        .await
        .map_err(|err| map_error("find_doc_by_hash", err))?;
    collect_first_doc(stream).await
}

/// Look up one doc row by id within the bound tenant (existence check for
/// delete scopes; `None` for foreign/unknown ids — no existence leak).
pub async fn find_doc(
    store: &LanceStore,
    ctx: &TenantContext,
    doc_id: &DocId,
) -> Result<Option<DocRecord>, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(crate::schema::DOCS).await?;
    let filter = tenant_scope(ctx.tenant_id()).and(col(COL_DOC_ID).eq(lit(doc_id.to_string())));
    let stream = table
        .query()
        .only_if_expr(filter)
        .limit(1)
        .execute()
        .await
        .map_err(|err| map_error("find_doc", err))?;
    collect_first_doc(stream).await
}

/// All doc rows of the tenant (export, REQ-CG-005).
pub async fn all_docs(
    store: &LanceStore,
    ctx: &TenantContext,
) -> Result<Vec<DocRecord>, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(crate::schema::DOCS).await?;
    let stream = table
        .query()
        .only_if_expr(tenant_scope(ctx.tenant_id()))
        .execute()
        .await
        .map_err(|err| map_error("all_docs", err))?;
    let batches: Vec<RecordBatch> = stream
        .try_collect()
        .await
        .map_err(|err| map_error("all_docs", err))?;
    let mut docs = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            docs.push(doc_at(batch, row)?);
        }
    }
    Ok(docs)
}

/// The chunk ids belonging to one doc within the bound tenant (dedup
/// response payload + doc-existence checks). Empty for an unknown doc.
pub async fn chunk_ids_by_doc(
    store: &LanceStore,
    ctx: &TenantContext,
    doc_id: &DocId,
) -> Result<Vec<ChunkId>, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(CHUNKS).await?;
    let filter = tenant_scope(ctx.tenant_id()).and(col(COL_DOC_ID).eq(lit(doc_id.to_string())));
    let stream = table
        .query()
        .only_if_expr(filter)
        .execute()
        .await
        .map_err(|err| map_error("chunk_ids_by_doc", err))?;
    let batches: Vec<RecordBatch> = stream
        .try_collect()
        .await
        .map_err(|err| map_error("chunk_ids_by_doc", err))?;
    let mut ids = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            let chunk: MemoryChunk = crate::store::row_to_chunk(batch, row)?;
            ids.push(chunk.id);
        }
    }
    Ok(ids)
}

async fn collect_first_doc(
    stream: impl futures::Stream<Item = Result<RecordBatch, lancedb::Error>> + std::marker::Unpin,
) -> Result<Option<DocRecord>, DomainError> {
    let mut stream = Box::pin(stream);
    while let Some(batch) = stream
        .try_next()
        .await
        .map_err(|err| map_error("collect doc", err))?
    {
        if batch.num_rows() > 0 {
            return Ok(Some(doc_at(&batch, 0)?));
        }
    }
    Ok(None)
}

fn doc_at(batch: &RecordBatch, row: usize) -> Result<DocRecord, DomainError> {
    Ok(DocRecord {
        doc_id: id_at(batch, COL_DOC_ID, row)?,
        tenant_id: id_at(batch, COL_TENANT_ID, row)?,
        workspace_id: id_at(batch, COL_WORKSPACE_ID, row)?,
        agent_id: AgentId::new(string_at(batch, COL_AGENT_ID, row)?),
        title: Some(string_at(batch, COL_TITLE, row)?)
            .filter(|t| !t.is_empty())
            .or(None),
        source: serde_json::from_str(&string_at(batch, COL_SOURCE, row)?).map_err(|err| {
            DomainError::Internal {
                message: format!("corrupt source_json in docs store: {err}"),
            }
        })?,
        created_at: crate::schema::nanos_to_ts(
            batch
                .column_by_name(COL_CREATED_AT)
                .ok_or_else(|| missing_column(COL_CREATED_AT))?
                .as_primitive::<lancedb::arrow::arrow_array::types::TimestampNanosecondType>()
                .value(row),
        ),
        content_hash: string_at(batch, COL_CONTENT_HASH, row)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::chunks_to_batch;
    use memento_testkit::TempStore;

    fn record(ts: &TempStore, doc_id: DocId, hash: &str) -> DocRecord {
        DocRecord {
            doc_id,
            tenant_id: *ts.tenant_id(),
            workspace_id: *ts.workspace_id(),
            agent_id: AgentId::new("test-agent"),
            title: Some("Título".into()),
            source: SourceKind::Text,
            created_at: chrono::Utc::now(),
            content_hash: hash.to_string(),
        }
    }

    async fn open(ts: &TempStore) -> LanceStore {
        let store = LanceStore::open(&ts.ctx(), ts.root()).await.unwrap();
        store.ensure_schema().await.unwrap();
        store
    }

    #[tokio::test]
    async fn upsert_find_and_probe_round_trip() {
        let ts = TempStore::new();
        let store = open(&ts).await;
        let ctx = ts.ctx();
        let doc_id = DocId::new();

        assert!(find_doc(&store, &ctx, &doc_id).await.unwrap().is_none());
        assert!(
            find_doc_by_hash(&store, &ctx, "h1")
                .await
                .unwrap()
                .is_none()
        );

        upsert_doc(&store, &ctx, &record(&ts, doc_id, "h1"))
            .await
            .unwrap();
        let found = find_doc(&store, &ctx, &doc_id)
            .await
            .unwrap()
            .expect("doc present");
        assert_eq!(found.doc_id, doc_id);
        assert_eq!(found.content_hash, "h1");
        assert_eq!(found.title.as_deref(), Some("Título"));
        assert_eq!(found.source, SourceKind::Text);

        // Probe hits; a different hash does not.
        let hit = find_doc_by_hash(&store, &ctx, "h1")
            .await
            .unwrap()
            .expect("probe hit");
        assert_eq!(hit.doc_id, doc_id);
        assert!(
            find_doc_by_hash(&store, &ctx, "h2")
                .await
                .unwrap()
                .is_none()
        );

        // Upsert replaces the row (same doc_id, new hash).
        upsert_doc(&store, &ctx, &record(&ts, doc_id, "h2"))
            .await
            .unwrap();
        let found = find_doc(&store, &ctx, &doc_id)
            .await
            .unwrap()
            .expect("still present");
        assert_eq!(found.content_hash, "h2");
        assert_eq!(all_docs(&store, &ctx).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn probe_is_tenant_scoped() {
        let ts = TempStore::new();
        let store = open(&ts).await;
        let ctx = ts.ctx();
        let doc_id = DocId::new();
        upsert_doc(&store, &ctx, &record(&ts, doc_id, "shared"))
            .await
            .unwrap();

        // A foreign context cannot see the row (defense in depth), and a
        // second tenant's store with identical content hashes resolves to
        // nothing (REQ-MC-005: no cross-tenant dedup).
        let ts2 = TempStore::new();
        let ctx2 = ts2.ctx();
        assert_eq!(
            find_doc_by_hash(&store, &ctx2, "shared")
                .await
                .unwrap_err()
                .code(),
            "TENANT_FORBIDDEN"
        );
        let store2 = open(&ts2).await;
        assert!(
            find_doc_by_hash(&store2, &ctx2, "shared")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn chunk_ids_by_doc_returns_only_that_doc() {
        let ts = TempStore::new();
        let store = open(&ts).await;
        let ctx = ts.ctx();
        let a = DocId::new();
        let b = DocId::new();
        upsert_doc(&store, &ctx, &record(&ts, a, "ha"))
            .await
            .unwrap();
        upsert_doc(&store, &ctx, &record(&ts, b, "hb"))
            .await
            .unwrap();

        let chunk_a = ChunkId::new();
        let chunk_b = ChunkId::new();
        let t = chrono::Utc::now();
        let mk = |id: ChunkId, doc: DocId| MemoryChunk {
            id,
            tenant_id: *ts.tenant_id(),
            workspace_id: *ts.workspace_id(),
            agent_id: AgentId::new("test-agent"),
            doc_id: doc,
            text: "hola".into(),
            vector: None,
            created_at: t,
            provenance: memento_domain::Provenance {
                source: SourceKind::Text,
                doc_id: doc,
                chunk_id: id,
                created_at: t,
                embedding_model_version: "test".into(),
                tenant_id: *ts.tenant_id(),
                workspace_id: *ts.workspace_id(),
                agent_id: AgentId::new("test-agent"),
            },
        };
        let batch = chunks_to_batch(&[mk(chunk_a, a), mk(chunk_b, b)]).unwrap();
        store
            .table(CHUNKS)
            .await
            .unwrap()
            .add(batch)
            .execute()
            .await
            .unwrap();

        let ids = chunk_ids_by_doc(&store, &ctx, &a).await.unwrap();
        assert_eq!(ids, vec![chunk_a]);
        assert!(
            chunk_ids_by_doc(&store, &ctx, &DocId::new())
                .await
                .unwrap()
                .is_empty()
        );
    }
}
