//! Feedback use case (T-062, REQ-ML-001).
//!
//! `mark_useful`: agents attach usefulness signals to chunks with an
//! optional reason. The signal is persisted with full attribution (agent,
//! timestamp, tenant, workspace) in the `feedback` table and is readable via
//! chunk inspection. MVP ranking effects are out of scope; the signal feeds
//! the context_fit bonus (design D6).

use crate::AppService;
use memento_domain::{ChunkId, DomainError, TenantContext};
use memento_lancedb::{FeedbackRecord, add_feedback, feedback_for_chunk};
use memento_ports::SearchPort;
use serde_json::json;

impl AppService {
    /// Record a usefulness signal for a chunk (REQ-ML-001).
    ///
    /// # Errors
    ///
    /// * `ChunkNotFound` — the chunk does not exist in this tenant
    ///   (REQ-ML-001 scenario 2; also covers cross-tenant ids — no leak).
    pub async fn feedback(
        &self,
        ctx: &TenantContext,
        chunk_id: ChunkId,
        useful: bool,
        reason: Option<String>,
    ) -> Result<(), DomainError> {
        // REQ-ML-001: feedback on an unknown chunk is a structured error.
        // The lookup is tenant-scoped, so a foreign id resolves to None.
        if self.store.get_chunk(ctx, &chunk_id).await?.is_none() {
            return Err(DomainError::ChunkNotFound { id: chunk_id });
        }
        let has_reason = reason.is_some();

        add_feedback(
            &self.store,
            ctx,
            &FeedbackRecord {
                chunk_id,
                tenant_id: *ctx.tenant_id(),
                workspace_id: *ctx.workspace_id(),
                agent_id: ctx.agent_id().clone(),
                score: if useful { 1.0 } else { 0.0 },
                comment: reason,
                created_at: self.clock.now(),
            },
        )
        .await?;

        self.record_audit(
            ctx,
            "feedback",
            json!({
                "chunk_id": chunk_id,
                "useful": useful,
                // Presence only — the reason text itself never enters the
                // audit (REQ-CG-003: ids + counts, never content).
                "has_reason": has_reason,
            }),
            None,
        );
        Ok(())
    }

    /// Read the feedback signals of one chunk (chunk inspection, REQ-ML-001).
    /// Empty for an unknown chunk (not-found semantics live in the write path
    /// and in `get_chunk`).
    pub async fn feedback_for(
        &self,
        ctx: &TenantContext,
        chunk_id: &ChunkId,
    ) -> Result<Vec<FeedbackRecord>, DomainError> {
        feedback_for_chunk(&self.store, ctx, chunk_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;
    use memento_ports::{IngestTextRequest, SearchQuery};
    use memento_testkit::{TempStore, TestClock};

    #[tokio::test]
    async fn feedback_persists_with_attribution_and_reads_back() {
        // REQ-ML-001 scenario 1: the signal persists with attribution and is
        // readable via chunk inspection.
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock.clone()).await;
        let result = app
            .ingest_text(
                &ts.ctx(),
                IngestTextRequest {
                    text: "la memoria es un río".into(),
                    doc_id: None,
                    metadata: None,
                },
            )
            .await
            .expect("ingest");
        let chunk_id = result.chunk_ids[0];

        app.feedback(&ts.ctx(), chunk_id, true, Some("muy útil".to_string()))
            .await
            .expect("feedback ok");

        let rows = app.feedback_for(&ts.ctx(), &chunk_id).await.expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].score, 1.0);
        assert_eq!(rows[0].chunk_id, chunk_id);
        assert_eq!(rows[0].tenant_id, *ts.tenant_id());
        assert_eq!(rows[0].workspace_id, *ts.workspace_id());
        assert_eq!(rows[0].agent_id, *ts.agent_id());
        assert_eq!(rows[0].created_at, clock.now(), "injectable clock stamped");
    }

    #[tokio::test]
    async fn feedback_on_unknown_chunk_is_not_found() {
        // REQ-ML-001 scenario 2: structured not-found, nothing written.
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let err = app
            .feedback(&ts.ctx(), ChunkId::new(), true, None)
            .await
            .expect_err("unknown chunk");
        assert_eq!(err.code(), "CHUNK_NOT_FOUND");
    }

    #[tokio::test]
    async fn negative_feedback_scores_zero() {
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let result = app
            .ingest_text(
                &ts.ctx(),
                IngestTextRequest {
                    text: "otra memoria más".into(),
                    doc_id: None,
                    metadata: None,
                },
            )
            .await
            .expect("ingest");
        app.feedback(&ts.ctx(), result.chunk_ids[0], false, None)
            .await
            .expect("ok");
        let rows = app
            .feedback_for(&ts.ctx(), &result.chunk_ids[0])
            .await
            .expect("read");
        assert_eq!(rows[0].score, 0.0);
    }

    #[tokio::test]
    async fn feedback_survives_search_round_trip() {
        // The signal is store-backed: a fresh app instance on the same
        // store still reads it (chunk inspection after reopen).
        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let result = app
            .ingest_text(
                &ts.ctx(),
                IngestTextRequest {
                    text: "recuerdo persistente".into(),
                    doc_id: None,
                    metadata: None,
                },
            )
            .await
            .expect("ingest");
        let chunk_id = result.chunk_ids[0];
        app.feedback(&ts.ctx(), chunk_id, true, None)
            .await
            .expect("ok");

        let reopened = test_app(&ts, TestClock::default()).await;
        let rows = reopened
            .feedback_for(&ts.ctx(), &chunk_id)
            .await
            .expect("read");
        assert_eq!(rows.len(), 1);
        // Still searchable.
        let hits = reopened
            .store()
            .search(
                &ts.ctx(),
                SearchQuery::new("recuerdo", 5, *ts.workspace_id()),
            )
            .await
            .expect("search");
        assert!(!hits.is_empty());
    }
}
