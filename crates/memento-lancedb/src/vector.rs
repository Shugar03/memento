//! Vector index + ANN search (T-021).
//!
//! Inserts go through [`add_chunks_batch`] — a single `table.add()` call so
//! the whole batch becomes visible atomically (REQ-MC-007). Searches are
//! tenant+workspace scoped by construction (REQ-MR-006) and return
//! `(chunk_id, score)` pairs where `score = 1 / (1 + distance)` (higher is
//! better, consistent with BM25 in [`crate::fts`]).
//!
//! The IVF-PQ index (design: `n_list ≈ sqrt(rows)`, `n_pq = dim/4`) is built
//! lazily once a table is large enough to make it worthwhile; below that we
//! search brute force (exact, small-scale correct).

use crate::schema::{
    CHUNKS, COL_CHUNK_ID, COL_VECTOR, EMBEDDING_DIM, VECTOR_INDEX_NAME, chunks_schema,
    chunks_scope, ts_to_nanos,
};
use crate::store::{LanceStore, map_error};
use futures::TryStreamExt;
use lancedb::arrow::arrow_array::{
    FixedSizeListArray, RecordBatch, StringArray, TimestampNanosecondArray, cast::AsArray,
    types::Float32Type,
};
use lancedb::index::Index;
use lancedb::index::vector::IvfPqIndexBuilder;
use lancedb::query::{ExecutableQuery, QueryBase};
use memento_domain::{ChunkId, DomainError, MemoryChunk, TenantContext, WorkspaceId};
use std::sync::Arc;

/// Row count at which building the IVF-PQ index pays off. Below this the
/// brute-force scan is exact and fast enough for tests and small tenants.
const VECTOR_INDEX_MIN_ROWS: u64 = 256;

/// Insert a batch of chunks with ONE `table.add()` call: atomic visibility
/// (REQ-MC-007) — the batch is either fully visible or not at all.
pub async fn add_chunks_batch(
    store: &LanceStore,
    ctx: &TenantContext,
    chunks: &[MemoryChunk],
) -> Result<(), DomainError> {
    if chunks.is_empty() {
        return Ok(());
    }
    store.ensure_tenant(ctx)?;
    let table = store.table(CHUNKS).await?;
    let batch = chunks_to_batch(chunks)?;
    table
        .add(batch)
        .execute()
        .await
        .map_err(|err| map_error("add_chunks_batch", err))?;
    Ok(())
}

/// ANN search within the tenant+workspace scope. Returns `(chunk_id, score)`
/// pairs ranked by decreasing score (score = `1 / (1 + distance)`).
pub async fn vector_search(
    store: &LanceStore,
    ctx: &TenantContext,
    query_vec: &[f32],
    workspace_id: &WorkspaceId,
    top_k: usize,
) -> Result<Vec<(ChunkId, f32)>, DomainError> {
    store.ensure_tenant(ctx)?;
    if query_vec.len() != EMBEDDING_DIM {
        return Err(DomainError::InvalidInput {
            message: format!(
                "query vector has {} dims, expected {EMBEDDING_DIM}",
                query_vec.len()
            ),
        });
    }

    let table = store.table(CHUNKS).await?;
    let stream = table
        .vector_search(query_vec.to_vec())
        .map_err(|err| map_error("vector_search", err))?
        .column(COL_VECTOR)
        .only_if_expr(chunks_scope(ctx.tenant_id(), workspace_id))
        .limit(top_k)
        .execute()
        .await
        .map_err(|err| map_error("vector_search", err))?;

    let batches: Vec<RecordBatch> = stream
        .try_collect()
        .await
        .map_err(|err| map_error("vector_search", err))?;

    let mut out: Vec<(ChunkId, f32)> = Vec::new();
    for batch in &batches {
        let ids = batch
            .column_by_name(COL_CHUNK_ID)
            .ok_or_else(|| missing_column(COL_CHUNK_ID))?
            .as_string::<i32>();
        let distances = batch
            .column_by_name("_distance")
            .ok_or_else(|| missing_column("_distance"))?
            .as_primitive::<Float32Type>();
        for row in 0..batch.num_rows() {
            let id: ChunkId = ids
                .value(row)
                .parse()
                .map_err(|_| DomainError::Internal {
                    message: "corrupt chunk_id in store".into(),
                })?;
            let distance = distances.value(row);
            out.push((id, 1.0 / (1.0 + distance)));
        }
    }
    Ok(out)
}

/// Build (idempotently) the IVF-PQ index on the `vector` column once the
/// table holds enough rows (design: `n_list = sqrt(rows)`, `n_pq = dim/4`).
/// No-op when the table is below the threshold, when the index already
/// exists, or when the table is empty (nothing to train on yet).
pub async fn ensure_vector_index(store: &LanceStore) -> Result<(), DomainError> {
    let table = store.table(CHUNKS).await?;
    let indices = table
        .list_indices()
        .await
        .map_err(|err| map_error("list_indices", err))?;
    if indices.iter().any(|i| i.name == VECTOR_INDEX_NAME) {
        return Ok(());
    }
    let rows = table
        .count_rows(None)
        .await
        .map_err(|err| map_error("count_rows", err))? as u64;
    if rows < VECTOR_INDEX_MIN_ROWS {
        return Ok(());
    }

    // n_pq = dim / 4 → 96 codes for the 384-d E5-small vectors.
    let num_sub_vectors = (EMBEDDING_DIM / 4) as u32;
    let builder = IvfPqIndexBuilder::default().num_sub_vectors(num_sub_vectors);
    table
        .create_index(&[COL_VECTOR], Index::IvfPq(builder))
        .name(VECTOR_INDEX_NAME.to_string())
        .execute()
        .await
        .map_err(|err| map_error("create_vector_index", err))
}

/// Build a `RecordBatch` with the `chunks` schema from domain chunks.
fn chunks_to_batch(chunks: &[MemoryChunk]) -> Result<RecordBatch, DomainError> {
    let schema = chunks_schema();
    let vectors: Vec<Option<Vec<Option<f32>>>> = chunks
        .iter()
        .map(|c| {
            c.vector
                .clone()
                .map(|v| v.into_iter().map(Some).collect::<Vec<Option<f32>>>())
        })
        .collect();

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(
                chunks.iter().map(|c| c.id.to_string()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                chunks
                    .iter()
                    .map(|c| c.tenant_id.to_string())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                chunks
                    .iter()
                    .map(|c| c.workspace_id.to_string())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                chunks
                    .iter()
                    .map(|c| c.agent_id.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                chunks.iter().map(|c| c.doc_id.to_string()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                chunks
                    .iter()
                    .map(|c| {
                        serde_json::to_string(&c.provenance.source).expect("SourceKind serializes")
                    })
                    .collect::<Vec<_>>(),
            )),
            Arc::new(FixedSizeListArray::from_iter_primitive::<
                Float32Type,
                _,
                _,
            >(vectors, EMBEDDING_DIM as i32)),
            Arc::new(StringArray::from(
                chunks
                    .iter()
                    .map(|c| c.provenance.embedding_model_version.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(TimestampNanosecondArray::from(
                chunks
                    .iter()
                    .map(|c| Some(ts_to_nanos(c.created_at)))
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(|err| DomainError::Internal {
        message: format!("build chunk batch: {err}"),
    })
}

fn missing_column(name: &str) -> DomainError {
    DomainError::Internal {
        message: format!("result set missing column {name}"),
    }
}
