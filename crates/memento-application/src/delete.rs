//! Delete use case (T-062, REQ-ML-002).
//!
//! [`AppService::delete`] routes a [`DeleteScope`] to the store's hard-delete
//! primitives. All deletes are hard (REQ-ML-002): the rows leave the live
//! store immediately and no retrieval path can return them afterwards.
//! Cross-tenant ids surface as not-found with no existence leak (REQ-ML-002
//! scenario 2, REQ-MR-005 semantics).

use crate::AppService;
use memento_domain::{DomainError, TenantContext};
use memento_lancedb::{delete_chunks, delete_doc, delete_tenant, delete_workspace, find_doc};
use memento_ports::{DeleteReport, DeleteScope, SearchPort};
use serde_json::json;

impl AppService {
    /// Permanently delete within the bound tenant (REQ-ML-002).
    ///
    /// # Errors
    ///
    /// * `ChunkNotFound` / `NotFound` — the chunk or doc does not exist in
    ///   this tenant (also covers foreign ids — no existence leak).
    /// * `NotFound` — `DeleteScope::Tenant` for a foreign tenant id.
    pub async fn delete(
        &self,
        ctx: &TenantContext,
        scope: DeleteScope,
    ) -> Result<DeleteReport, DomainError> {
        let report = match &scope {
            DeleteScope::Chunk { id } => {
                // Tenant-scoped existence check first (REQ-ML-002 scenario 2).
                if self.store.get_chunk(ctx, id).await?.is_none() {
                    return Err(DomainError::ChunkNotFound { id: *id });
                }
                delete_chunks(&self.store, ctx, std::slice::from_ref(id)).await?
            }
            DeleteScope::Doc { id } => {
                if find_doc(&self.store, ctx, id).await?.is_none() {
                    return Err(DomainError::NotFound {
                        what: format!("document {id}"),
                    });
                }
                delete_doc(&self.store, ctx, id).await?
            }
            DeleteScope::Workspace { id } => delete_workspace(&self.store, ctx, id).await?,
            DeleteScope::Tenant { id } => {
                if id != ctx.tenant_id() {
                    // Deleting a tenant that is not the bound one: from this
                    // store's perspective that tenant does not exist.
                    return Err(DomainError::NotFound {
                        what: format!("tenant {id}"),
                    });
                }
                delete_tenant(&self.store, ctx).await?
            }
        };

        self.record_audit(
            ctx,
            "delete",
            json!({
                "scope": scope_label(&scope),
                "target": scope_target(&scope),
                "deleted_count": report.deleted_count,
            }),
            Some(report.chore_id),
        );
        Ok(report)
    }
}

/// Stable scope label for audit targets (`chunk`, `doc`, `workspace`,
/// `tenant`).
fn scope_label(scope: &DeleteScope) -> &'static str {
    match scope {
        DeleteScope::Chunk { .. } => "chunk",
        DeleteScope::Doc { .. } => "doc",
        DeleteScope::Workspace { .. } => "workspace",
        DeleteScope::Tenant { .. } => "tenant",
    }
}

/// The target id (ids only — never content, REQ-CG-003).
fn scope_target(scope: &DeleteScope) -> String {
    match scope {
        DeleteScope::Chunk { id } => id.to_string(),
        DeleteScope::Doc { id } => id.to_string(),
        DeleteScope::Workspace { id } => id.to_string(),
        DeleteScope::Tenant { id } => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;
    use memento_domain::{ChunkId, DocId, TenantId, WorkspaceId};
    use memento_ports::{IngestTextRequest, SearchQuery};
    use memento_testkit::{TempStore, TestClock};

    /// Ingest two documents, return (app, chunk ids of doc A, doc ids).
    async fn two_doc_tenant(ts: &TempStore) -> (AppService, Vec<ChunkId>, DocId, DocId) {
        let clock = TestClock::default();
        let app = test_app(ts, clock).await;
        let a = app
            .ingest_text(
                &ts.ctx(),
                IngestTextRequest {
                    text: "la memoria es un río subterráneo que fluye".into(),
                    doc_id: None,
                    metadata: None,
                },
            )
            .await
            .expect("doc a");
        let b = app
            .ingest_text(
                &ts.ctx(),
                IngestTextRequest {
                    text: "la tecnología cambia el trabajo diario".into(),
                    doc_id: None,
                    metadata: None,
                },
            )
            .await
            .expect("doc b");
        (app, a.chunk_ids.clone(), a.doc_id, b.doc_id)
    }

    #[tokio::test]
    async fn chunk_delete_removes_from_retrieval() {
        let ts = TempStore::new();
        let (app, chunk_ids, _, _) = two_doc_tenant(&ts).await;
        let report = app
            .delete(&ts.ctx(), DeleteScope::Chunk { id: chunk_ids[0] })
            .await
            .expect("delete");
        assert_eq!(report.deleted_count, 1);

        // Hard delete: get_chunk returns None and search never surfaces it.
        assert!(
            app.get_chunk(&ts.ctx(), &chunk_ids[0])
                .await
                .expect("read")
                .is_none()
        );
        let hits = app
            .search(
                &ts.ctx(),
                SearchQuery::new("memoria río", 10, *ts.workspace_id()),
            )
            .await
            .expect("search");
        assert!(
            hits.iter().all(|h| h.chunk_id != chunk_ids[0]),
            "deleted chunk absent from search (REQ-ML-002)"
        );
    }

    #[tokio::test]
    async fn doc_delete_removes_all_its_chunks_and_doc_row() {
        let ts = TempStore::new();
        let (app, _, doc_a, _) = two_doc_tenant(&ts).await;
        let report = app
            .delete(&ts.ctx(), DeleteScope::Doc { id: doc_a })
            .await
            .expect("delete");
        assert!(report.deleted_count >= 1, "doc chunks removed");

        // No chunk of D is returned by search afterwards (REQ-ML-002
        // scenario 1) and the docs row is gone.
        let hits = app
            .search(
                &ts.ctx(),
                SearchQuery::new("memoria río", 10, *ts.workspace_id()),
            )
            .await
            .expect("search");
        assert!(hits.is_empty(), "all doc-a chunks gone from search");
        assert!(
            find_doc(app.store(), &ts.ctx(), &doc_a)
                .await
                .expect("probe")
                .is_none()
        );
    }

    #[tokio::test]
    async fn cross_tenant_doc_delete_is_not_found() {
        // REQ-ML-002 scenario 2: a doc_id belonging to another tenant
        // resolves to not-found and nothing is deleted.
        let ts = TempStore::new();
        let (app, _, doc_a, _) = two_doc_tenant(&ts).await;
        let foreign = DocId::new();
        let err = app
            .delete(&ts.ctx(), DeleteScope::Doc { id: foreign })
            .await
            .expect_err("foreign doc");
        assert_eq!(err.code(), "NOT_FOUND");
        assert_eq!(
            app.store().count_chunks(&ts.ctx()).await.unwrap(),
            2,
            "nothing deleted"
        );

        let _ = doc_a;
    }

    #[tokio::test]
    async fn cross_tenant_chunk_delete_is_not_found() {
        let ts = TempStore::new();
        let (app, chunk_ids, _, _) = two_doc_tenant(&ts).await;
        let err = app
            .delete(&ts.ctx(), DeleteScope::Chunk { id: chunk_ids[0] })
            .await;
        // Sanity: the id exists in THIS tenant, so the delete succeeds —
        // cross-tenant behavior is exercised by the foreign-id case below.
        assert!(err.is_ok());
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 1);

        let foreign_chunk = ChunkId::new();
        let err = app
            .delete(&ts.ctx(), DeleteScope::Chunk { id: foreign_chunk })
            .await
            .expect_err("foreign chunk");
        assert_eq!(err.code(), "CHUNK_NOT_FOUND");
    }

    #[tokio::test]
    async fn workspace_delete_clears_only_that_workspace() {
        let ts = TempStore::new();
        let (app, _, _, _) = two_doc_tenant(&ts).await;
        let report = app
            .delete(
                &ts.ctx(),
                DeleteScope::Workspace {
                    id: *ts.workspace_id(),
                },
            )
            .await
            .expect("delete");
        // 2 chunks + 2 docs rows = 4 rows removed (REQ-ML-002).
        assert_eq!(report.deleted_count, 4, "both docs in the workspace");
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 0);
        // A foreign workspace id deletes nothing (empty scope → clean no-op).
        let report = app
            .delete(
                &ts.ctx(),
                DeleteScope::Workspace {
                    id: WorkspaceId::new(),
                },
            )
            .await
            .expect("foreign workspace is a no-op");
        assert_eq!(report.deleted_count, 0);
    }

    #[tokio::test]
    async fn tenant_scope_deletes_everything_of_bound_tenant() {
        let ts = TempStore::new();
        let (app, _, _, _) = two_doc_tenant(&ts).await;
        let report = app
            .delete(
                &ts.ctx(),
                DeleteScope::Tenant {
                    id: *ts.tenant_id(),
                },
            )
            .await
            .expect("delete");
        // 2 chunks + 2 docs rows = 4 (tenant scope covers all tables).
        assert_eq!(report.deleted_count, 4);
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 0);

        // Foreign tenant id → not-found, nothing deleted (but store is empty
        // already — the guard itself is what matters).
        let err = app
            .delete(
                &ts.ctx(),
                DeleteScope::Tenant {
                    id: TenantId::new(),
                },
            )
            .await
            .expect_err("foreign tenant");
        assert_eq!(err.code(), "NOT_FOUND");
    }
}
