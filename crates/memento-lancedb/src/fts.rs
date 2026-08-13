//! FTS (BM25) index and search (T-021).
//!
//! Lance's inverted index over the `text` column powers BM25 queries via
//! `FullTextSearchQuery`. The index is built with `ascii_folding` so Spanish
//! accents fold to their base letters ("memória" matches "memoria",
//! "información" matches "informacion").
//!
//! The index is created lazily on the first search over non-empty data and
//! stays stale-tolerant: lance merges unindexed rows at query time, so rows
//! added after index creation are still searchable (correctness first; a
//! re-index is a maintenance concern, not a search blocker).
//!
//! `full_text_search` returns full [`SearchHit`]s (BM25 `_score` + row
//! materialization) so the `SearchPort` impl needs no second lookup.

use crate::schema::{CHUNKS, COL_DOC_ID, COL_SOURCE, COL_TEXT, FTS_INDEX_NAME, chunks_scope};
use crate::store::{LanceStore, map_error, row_to_search_hit};
use futures::TryStreamExt;
use lancedb::arrow::arrow_array::{RecordBatch, cast::AsArray, types::Float32Type};
use lancedb::expr::{col, lit};
use lancedb::index::Index;
use lancedb::index::scalar::{FtsIndexBuilder, FullTextSearchQuery};
use lancedb::query::{ExecutableQuery, QueryBase};
use memento_domain::{DomainError, TenantContext, WorkspaceId};
use memento_observability::EventRecord;
use memento_ports::{SearchFilters, SearchHit};
use tracing::Instrument;

/// Max results a single search may return (REQ-MR-007 budget context).
pub const MAX_TOP_K: usize = 100;

/// Build the FTS index over `text` if missing and there is data to train on.
/// Idempotent; no-op on an empty table (retried on the next search).
///
/// The build runs inside a `fts_ensure` span (REQ-OBS-003) and appends a
/// tenant-scoped `fts_build` event (REQ-OBS-008) through the store's shared
/// [`EventSink`] when one is attached — ids/counts only (index name + row
/// count), outcome ok|error with the stable code.
pub async fn ensure_fts_index(store: &LanceStore) -> Result<(), DomainError> {
    let span = tracing::info_span!(
        "fts_ensure",
        tenant_id = %store.tenant_id(),
        chore_id = tracing::field::Empty,
    );
    async {
        let table = store.table(CHUNKS).await?;
        let indices = table
            .list_indices()
            .await
            .map_err(|err| map_error("list_indices", err))?;
        if indices.iter().any(|i| i.name == FTS_INDEX_NAME) {
            return Ok(());
        }
        let rows = table
            .count_rows(None)
            .await
            .map_err(|err| map_error("count_rows", err))?;
        if rows == 0 {
            return Ok(());
        }

        let builder = FtsIndexBuilder::default().ascii_folding(true);
        let result = table
            .create_index(&[COL_TEXT], Index::FTS(builder))
            .name(FTS_INDEX_NAME.to_string())
            .execute()
            .await
            .map_err(|err| map_error("create_fts_index", err));
        if let Some(sink) = store.events() {
            let event = match &result {
                Ok(()) => EventRecord {
                    ts: chrono::Utc::now(),
                    tenant_id: *store.tenant_id(),
                    agent_id: None, // adapter actor — no agent (never faked)
                    action: "fts_build".to_string(),
                    target: serde_json::json!({"index": FTS_INDEX_NAME, "chunks": rows}),
                    outcome: "ok",
                    error_code: None,
                    chore_id: None,
                },
                Err(err) => EventRecord {
                    ts: chrono::Utc::now(),
                    tenant_id: *store.tenant_id(),
                    agent_id: None,
                    action: "fts_build".to_string(),
                    target: serde_json::json!({"index": FTS_INDEX_NAME, "chunks": rows}),
                    outcome: "error",
                    error_code: Some(err.code()),
                    chore_id: None,
                },
            };
            sink.record(&event);
        }
        result
    }
    .instrument(span)
    .await
}

/// BM25 full-text search within the tenant+workspace scope, ranked by score
/// (higher is better). An empty (or whitespace-only) query yields an empty
/// result set, never an error.
pub async fn full_text_search(
    store: &LanceStore,
    ctx: &TenantContext,
    query: &str,
    workspace_id: &WorkspaceId,
    top_k: usize,
    filters: Option<&SearchFilters>,
) -> Result<Vec<SearchHit>, DomainError> {
    store.ensure_tenant(ctx)?;
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    if top_k > MAX_TOP_K {
        return Err(DomainError::TopKExceeded {
            requested: top_k,
            max: MAX_TOP_K,
        });
    }

    // No rows → nothing to search; without this guard the FTS engine errors
    // ("no INVERTED index") because an empty table never gets an index.
    let table = store.table(CHUNKS).await?;
    let rows = table
        .count_rows(None)
        .await
        .map_err(|err| map_error("count_rows", err))?;
    if rows == 0 {
        return Ok(Vec::new());
    }

    ensure_fts_index(store).await?;

    // Scope: tenant AND workspace (mandatory), plus optional doc/source
    // filters. Applied as a post-filter on the FTS results.
    let mut scope = chunks_scope(ctx.tenant_id(), workspace_id);
    if let Some(f) = filters {
        if let Some(doc_id) = &f.doc_id {
            scope = scope.and(col(COL_DOC_ID).eq(lit(doc_id.to_string())));
        }
        if let Some(source) = &f.source {
            let source_json =
                serde_json::to_string(source).map_err(|err| DomainError::Internal {
                    message: format!("serialize source filter: {err}"),
                })?;
            scope = scope.and(col(COL_SOURCE).eq(lit(source_json)));
        }
    }

    let table = store.table(CHUNKS).await?;
    let stream = table
        .query()
        .full_text_search(FullTextSearchQuery::new(query.to_string()))
        .only_if_expr(scope)
        .limit(top_k)
        .execute()
        .await
        .map_err(|err| map_error("full_text_search", err))?;

    let batches: Vec<RecordBatch> = stream
        .try_collect()
        .await
        .map_err(|err| map_error("full_text_search", err))?;

    let mut hits: Vec<SearchHit> = Vec::new();
    for batch in &batches {
        let scores = batch
            .column_by_name("_score")
            .ok_or_else(|| DomainError::Internal {
                message: "FTS result set missing _score column".into(),
            })?
            .as_primitive::<Float32Type>();
        for row in 0..batch.num_rows() {
            hits.push(row_to_search_hit(batch, row, scores.value(row))?);
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_top_k_bounds() {
        assert_eq!(MAX_TOP_K, 100);
    }
}
