//! memento-lancedb — Memento RS LanceDB storage adapter (design D8).
//!
//! One process-bound tenant per database directory; every table carries
//! `tenant_id`/`workspace_id` columns so all queries are scoped by
//! construction. See [`schema`] for the layout, [`store`] for the connection
//! bootstrap, [`vector`] for batch insert + ANN search, [`fts`] for BM25
//! search, and [`maintenance`] for the delete→compact→prune purge chain.

pub mod fts;
pub mod maintenance;
pub mod schema;
pub mod store;
pub mod vector;

pub use fts::{MAX_TOP_K, ensure_fts_index, full_text_search};
pub use maintenance::{
    VersionSummary, compact, delete_chunks, delete_doc, delete_tenant, delete_workspace, erase,
    list_versions, prune, sweep_expired, version_snapshot,
};
pub use schema::{
    CHUNKS, DOCS, FEEDBACK, SYMBOLS, chunks_scope, tenant_scope, workspace_scope,
};
pub use store::{LanceStore, map_error};
pub use vector::{add_chunks_batch, ensure_vector_index, vector_search};

use async_trait::async_trait;
use futures::TryStreamExt;
use lancedb::arrow::arrow_array::{RecordBatch};
use lancedb::expr::{col, lit};
use lancedb::query::{ExecutableQuery, QueryBase};
use memento_domain::{ChunkId, DomainError, MemoryChunk, TenantContext};
use memento_ports::{SearchHit, SearchPort};

/// SearchPort for the LanceDB adapter.
///
/// The store owns both the FTS index and the vector index, so the port
/// implementation can serve both retrieval modes (REQ-MR-001/002/003):
///
/// * `rrf_enabled = false` (default) — BM25 full-text search.
/// * `rrf_enabled = true` — hybrid requires a query vector, which the port
///   contract does not carry (the query text must be embedded first). The
///   application layer (T-061) composes hybrid via [`vector_search`] +
///   [`full_text_search`] and the RRF fuse; reaching the port with the flag
///   on surfaces the REQ-MR-003 structured error until then.
///
/// Every hit is tenant-scoped by the bound context and workspace-scoped by
/// the mandatory `SearchQuery.workspace_id` (REQ-MR-006).
#[async_trait]
impl SearchPort for LanceStore {
    async fn search(
        &self,
        ctx: &TenantContext,
        query: memento_ports::SearchQuery,
    ) -> Result<Vec<SearchHit>, DomainError> {
        self.ensure_tenant(ctx)?;
        if query.top_k > MAX_TOP_K {
            return Err(DomainError::TopKExceeded {
                requested: query.top_k,
                max: MAX_TOP_K,
            });
        }
        if query.query.trim().is_empty() {
            return Ok(Vec::new());
        }
        if query.rrf_enabled {
            // REQ-MR-003: hybrid needs embeddings. The port cannot embed the
            // query text itself; memento-application composes the real hybrid
            // path through the adapter's public vector_search/fts methods.
            return Err(DomainError::InvalidInput {
                message: "hybrid search requires the application embedding layer (rrf_enabled)".into(),
            });
        }
        crate::fts::full_text_search(
            self,
            ctx,
            &query.query,
            &query.workspace_id,
            query.top_k,
            query.filters.as_ref(),
        )
        .await
    }

    async fn get_chunk(
        &self,
        ctx: &TenantContext,
        id: &ChunkId,
    ) -> Result<Option<MemoryChunk>, DomainError> {
        self.ensure_tenant(ctx)?;
        let table = self.table(schema::CHUNKS).await?;
        // Tenant-scoped (REQ-MR-005): a chunk id from another tenant simply
        // does not resolve inside this store.
        let filter = schema::tenant_scope(ctx.tenant_id()).and(col(schema::COL_CHUNK_ID).eq(lit(id.to_string())));
        let stream = table
            .query()
            .only_if_expr(filter)
            .limit(1)
            .execute()
            .await
            .map_err(|err| store::map_error("get_chunk", err))?;

        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|err| store::map_error("get_chunk", err))?;

        for batch in &batches {
            for row in 0..batch.num_rows() {
                return Ok(Some(store::row_to_chunk(batch, row)?));
            }
        }
        Ok(None)
    }
}

/// Materialize full hits for a set of ids (used by the hybrid RRF fuse).
pub async fn fetch_search_hits(
    store: &LanceStore,
    ctx: &TenantContext,
    ids: &[ChunkId],
) -> Result<Vec<SearchHit>, DomainError> {
    store.ensure_tenant(ctx)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let table = store.table(schema::CHUNKS).await?;
    let filter = schema::tenant_scope(ctx.tenant_id())
        .and(lancedb::expr::is_in(
            col(schema::COL_CHUNK_ID),
            ids.iter()
                .map(|id| lit(id.to_string()))
                .collect::<Vec<_>>(),
        ));
        let stream = table
            .query()
            .only_if_expr(filter)
            .execute()
            .await
            .map_err(|err| store::map_error("fetch_search_hits", err))?;
    let batches: Vec<RecordBatch> = stream
        .try_collect()
        .await
        .map_err(|err| store::map_error("fetch_search_hits", err))?;

    let mut hits = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            hits.push(store::row_to_search_hit(batch, row, 0.0)?);
        }
    }
    Ok(hits)
}

/// Reciprocal-rank fusion of two ranked id lists (hybrid retrieval, RRF
/// k=60). Scores are rank-based: `sum(1 / (k + rank))`, higher = better.
pub fn rrf_fuse(
    first: &[(ChunkId, f32)],
    second: &[(ChunkId, f32)],
    k: f32,
) -> Vec<(ChunkId, f32)> {
    use std::collections::HashMap;
    let mut scores: HashMap<ChunkId, f32> = HashMap::new();
    for (rank, (id, _)) in first.iter().enumerate() {
        *scores.entry(*id).or_insert(0.0) += 1.0 / (k + rank as f32 + 1.0);
    }
    for (rank, (id, _)) in second.iter().enumerate() {
        *scores.entry(*id).or_insert(0.0) += 1.0 / (k + rank as f32 + 1.0);
    }
    let mut out: Vec<(ChunkId, f32)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}
