//! `feedback` table access (T-062, REQ-ML-001): usefulness signals persisted
//! with full attribution, readable via chunk inspection, and consumed by the
//! context_fit value function (design D6 bonus ≤ +0.5).
//!
//! Schema (see [`crate::schema::feedback_schema`]): chunk_id, tenant_id,
//! workspace_id, agent_id, score (0.0 = not useful, 1.0 = useful), optional
//! comment, created_at. Rows are tenant-scoped by construction: every query
//! AND every insert is filtered by the bound context (defense in depth).
//!
//! MVP ranking effects are out of scope (REQ-ML-001); the signals only feed
//! the context_fit bonus and the export artifact (REQ-CG-005).

use crate::schema::{
    COL_AGENT_ID, COL_CHUNK_ID, COL_COMMENT, COL_CREATED_AT, COL_SCORE, COL_TENANT_ID,
    COL_WORKSPACE_ID, feedback_schema, tenant_scope, ts_to_nanos,
};
use crate::store::{LanceStore, id_at, map_error, missing_column, string_at};
use futures::TryStreamExt;
use lancedb::arrow::arrow_array::{
    Float32Array, RecordBatch, StringArray, TimestampNanosecondArray, cast::AsArray,
};
use lancedb::expr::{col, lit};
use lancedb::query::{ExecutableQuery, QueryBase};
use memento_domain::{AgentId, ChunkId, DomainError, TenantContext, WorkspaceId};
use std::sync::Arc;

/// One feedback row (REQ-ML-001 attribution contract).
#[derive(Debug, Clone, PartialEq)]
pub struct FeedbackRecord {
    pub chunk_id: ChunkId,
    pub tenant_id: memento_domain::TenantId,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    /// Usefulness signal: 0.0 = not useful, 1.0 = useful.
    pub score: f32,
    pub comment: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Persist one feedback signal for a chunk of the bound tenant
/// (REQ-ML-001). The chunk-existence check lives in the application layer
/// (it owns the `CHUNK_NOT_FOUND` semantics); this writes the row only.
pub async fn add_feedback(
    store: &LanceStore,
    ctx: &TenantContext,
    record: &FeedbackRecord,
) -> Result<(), DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(crate::schema::FEEDBACK).await?;

    let schema = feedback_schema();
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(vec![record.chunk_id.to_string()])),
            Arc::new(StringArray::from(vec![record.tenant_id.to_string()])),
            Arc::new(StringArray::from(vec![record.workspace_id.to_string()])),
            Arc::new(StringArray::from(vec![record.agent_id.as_str()])),
            Arc::new(Float32Array::from(vec![record.score])),
            Arc::new(StringArray::from(vec![
                record.comment.as_deref().unwrap_or_default(),
            ])),
            Arc::new(TimestampNanosecondArray::from(vec![Some(ts_to_nanos(
                record.created_at,
            ))])),
        ],
    )
    .map_err(|err| DomainError::Internal {
        message: format!("build feedback batch: {err}"),
    })?;

    table
        .add(batch)
        .execute()
        .await
        .map_err(|err| map_error("add_feedback", err))?;
    Ok(())
}

/// All feedback rows for one chunk of the bound tenant (chunk inspection,
/// REQ-ML-001 "readable via chunk inspection"; `None`-free empty for none).
pub async fn feedback_for_chunk(
    store: &LanceStore,
    ctx: &TenantContext,
    chunk_id: &ChunkId,
) -> Result<Vec<FeedbackRecord>, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(crate::schema::FEEDBACK).await?;
    let filter = tenant_scope(ctx.tenant_id()).and(col(COL_CHUNK_ID).eq(lit(chunk_id.to_string())));
    let stream = table
        .query()
        .only_if_expr(filter)
        .execute()
        .await
        .map_err(|err| map_error("feedback_for_chunk", err))?;
    collect_feedback(stream).await
}

/// All feedback rows of the tenant (export artifact, REQ-CG-005).
pub async fn all_feedback(
    store: &LanceStore,
    ctx: &TenantContext,
) -> Result<Vec<FeedbackRecord>, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(crate::schema::FEEDBACK).await?;
    let stream = table
        .query()
        .only_if_expr(tenant_scope(ctx.tenant_id()))
        .execute()
        .await
        .map_err(|err| map_error("all_feedback", err))?;
    collect_feedback(stream).await
}

async fn collect_feedback(
    stream: impl futures::Stream<Item = Result<RecordBatch, lancedb::Error>> + std::marker::Unpin,
) -> Result<Vec<FeedbackRecord>, DomainError> {
    let batches: Vec<RecordBatch> = stream
        .try_collect()
        .await
        .map_err(|err| map_error("collect feedback", err))?;
    let mut out = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            out.push(feedback_at(batch, row)?);
        }
    }
    Ok(out)
}

fn feedback_at(batch: &RecordBatch, row: usize) -> Result<FeedbackRecord, DomainError> {
    Ok(FeedbackRecord {
        chunk_id: id_at(batch, COL_CHUNK_ID, row)?,
        tenant_id: id_at(batch, COL_TENANT_ID, row)?,
        workspace_id: id_at(batch, COL_WORKSPACE_ID, row)?,
        agent_id: AgentId::new(string_at(batch, COL_AGENT_ID, row)?),
        score: batch
            .column_by_name(COL_SCORE)
            .ok_or_else(|| missing_column(COL_SCORE))?
            .as_primitive::<lancedb::arrow::arrow_array::types::Float32Type>()
            .value(row),
        comment: Some(string_at(batch, COL_COMMENT, row)?)
            .filter(|c| !c.is_empty())
            .or(None),
        created_at: crate::schema::nanos_to_ts(
            batch
                .column_by_name(COL_CREATED_AT)
                .ok_or_else(|| missing_column(COL_CREATED_AT))?
                .as_primitive::<lancedb::arrow::arrow_array::types::TimestampNanosecondType>()
                .value(row),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_testkit::TempStore;

    fn record(ts: &TempStore, chunk_id: ChunkId, score: f32) -> FeedbackRecord {
        FeedbackRecord {
            chunk_id,
            tenant_id: *ts.tenant_id(),
            workspace_id: *ts.workspace_id(),
            agent_id: AgentId::new("test-agent"),
            score,
            comment: Some("comentario".into()),
            created_at: chrono::Utc::now(),
        }
    }

    async fn open(ts: &TempStore) -> LanceStore {
        let store = LanceStore::open(&ts.ctx(), ts.root()).await.unwrap();
        store.ensure_schema().await.unwrap();
        store
    }

    #[tokio::test]
    async fn add_and_read_round_trip_with_attribution() {
        // REQ-ML-001: the signal persists with attribution (tenant,
        // workspace, agent, timestamp) and is readable via chunk inspection.
        let ts = TempStore::new();
        let store = open(&ts).await;
        let chunk_id = ChunkId::new();
        add_feedback(&store, &ts.ctx(), &record(&ts, chunk_id, 1.0))
            .await
            .unwrap();

        let rows = feedback_for_chunk(&store, &ts.ctx(), &chunk_id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chunk_id, chunk_id);
        assert_eq!(rows[0].score, 1.0);
        assert_eq!(rows[0].agent_id.as_str(), "test-agent");
        assert_eq!(rows[0].tenant_id, *ts.tenant_id());
        assert_eq!(rows[0].workspace_id, *ts.workspace_id());
        assert_eq!(rows[0].comment.as_deref(), Some("comentario"));

        // Unknown chunk → empty (the not-found semantics live in the app).
        assert!(
            feedback_for_chunk(&store, &ts.ctx(), &ChunkId::new())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rows_are_tenant_scoped() {
        let ts = TempStore::new();
        let store = open(&ts).await;
        add_feedback(&store, &ts.ctx(), &record(&ts, ChunkId::new(), 0.0))
            .await
            .unwrap();

        // Foreign context → TENANT_FORBIDDEN (defense in depth).
        let ts2 = TempStore::new();
        let err = feedback_for_chunk(&store, &ts2.ctx(), &ChunkId::new())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "TENANT_FORBIDDEN");
        // A second tenant's store sees none of the first tenant's rows.
        let store2 = open(&ts2).await;
        assert!(all_feedback(&store2, &ts2.ctx()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn all_feedback_returns_tenant_rows() {
        let ts = TempStore::new();
        let store = open(&ts).await;
        let a = ChunkId::new();
        let b = ChunkId::new();
        add_feedback(&store, &ts.ctx(), &record(&ts, a, 1.0))
            .await
            .unwrap();
        add_feedback(&store, &ts.ctx(), &record(&ts, b, 0.0))
            .await
            .unwrap();
        assert_eq!(all_feedback(&store, &ts.ctx()).await.unwrap().len(), 2);
    }
}
