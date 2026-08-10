//! Delete → Compact → Prune purge chain (T-022, discovery 2573).
//!
//! LanceDB is append-only per version: `delete` hides rows from queries
//! immediately, but the bytes stay on disk and the rows stay recoverable via
//! time travel (old versions) until a `Prune`. Right-to-erase therefore
//! REQUIRES the full chain (REQ-ML-004, REQ-CG-001):
//!
//! ```text
//! delete_by_* ──► compact ──► prune(older_than = 0)
//! ```
//!
//! * `delete` removes the rows from the latest version (immediately
//!   invisible to every query);
//! * `compact` rewrites the data files so the deleted rows leave the latest
//!   version's physical files;
//! * `prune` drops every version older than the current one, so the rows are
//!   no longer reachable through ANY version — unrecoverable.
//!
//! [`LanceStore::erase`] runs the chain for the whole tenant; the worker
//! (batch 10) and the erasure use case (batch 7) reuse these primitives.

use crate::schema::{ALL_TABLES, CHUNKS, COL_CHUNK_ID, COL_DOC_ID, created_before, tenant_scope};
use crate::store::{LanceStore, map_error, row_to_chunk};
use futures::TryStreamExt;
use lancedb::arrow::arrow_array::RecordBatch;
use lancedb::expr::{col, is_in, lit};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::table::{CompactionOptions, OptimizeAction};
use memento_domain::{ChoreId, ChunkId, DocId, DomainError, TenantContext, WorkspaceId};
use memento_ports::{DeleteReport, DeleteScope, LifecyclePort, SweepReport};

/// One historical dataset version (diagnostics / GDPR inspection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSummary {
    pub version: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

// --- deletes ------------------------------------------------------------------

/// Delete specific chunks within the bound tenant (REQ-ML-002).
pub async fn delete_chunks(
    store: &LanceStore,
    ctx: &TenantContext,
    ids: &[ChunkId],
) -> Result<DeleteReport, DomainError> {
    store.ensure_tenant(ctx)?;
    if ids.is_empty() {
        return Ok(empty_report());
    }
    let table = store.table(CHUNKS).await?;
    let filter = tenant_scope(ctx.tenant_id()).and(is_in(
        col(COL_CHUNK_ID),
        ids.iter().map(|id| lit(id.to_string())).collect::<Vec<_>>(),
    ));
    let result = table
        .delete(&filter)
        .await
        .map_err(|err| map_error("delete_chunks", err))?;
    Ok(report(result.num_deleted_rows))
}

/// Delete one document: its chunks, its docs-table row and its feedback.
pub async fn delete_doc(
    store: &LanceStore,
    ctx: &TenantContext,
    doc_id: &DocId,
) -> Result<DeleteReport, DomainError> {
    store.ensure_tenant(ctx)?;

    // Chunk ids of the doc (tenant-scoped) — needed to clean feedback.
    let chunk_ids = chunk_ids_for(
        store,
        tenant_scope(ctx.tenant_id()).and(col(COL_DOC_ID).eq(lit(doc_id.to_string()))),
    )
    .await?;

    let mut deleted = 0usize;
    if !chunk_ids.is_empty() {
        let table = store.table(CHUNKS).await?;
        let filter = tenant_scope(ctx.tenant_id()).and(is_in(
            col(COL_CHUNK_ID),
            chunk_ids
                .iter()
                .map(|id| lit(id.to_string()))
                .collect::<Vec<_>>(),
        ));
        deleted += table
            .delete(&filter)
            .await
            .map_err(|err| map_error("delete_doc chunks", err))?
            .num_deleted_rows as usize;
    }

    let docs = store.table(crate::schema::DOCS).await?;
    let docs_filter =
        tenant_scope(ctx.tenant_id()).and(col(COL_DOC_ID).eq(lit(doc_id.to_string())));
    deleted += docs
        .delete(&docs_filter)
        .await
        .map_err(|err| map_error("delete_doc docs", err))?
        .num_deleted_rows as usize;

    deleted += delete_feedback_for(store, ctx, &chunk_ids).await?;

    Ok(DeleteReport {
        deleted_count: deleted,
        freed_bytes: 0,
        chore_id: ChoreId::new(),
    })
}

/// Delete everything in one workspace of the tenant.
pub async fn delete_workspace(
    store: &LanceStore,
    ctx: &TenantContext,
    workspace_id: &WorkspaceId,
) -> Result<DeleteReport, DomainError> {
    store.ensure_tenant(ctx)?;
    let scope = crate::schema::chunks_scope(ctx.tenant_id(), workspace_id);

    let chunk_ids = chunk_ids_for(store, scope.clone()).await?;

    let table = store.table(CHUNKS).await?;
    let mut deleted = table
        .delete(&scope)
        .await
        .map_err(|err| map_error("delete_workspace chunks", err))?
        .num_deleted_rows as usize;

    let docs = store.table(crate::schema::DOCS).await?;
    let docs_filter = crate::schema::tenant_scope(ctx.tenant_id())
        .and(crate::schema::workspace_scope(workspace_id));
    deleted += docs
        .delete(&docs_filter)
        .await
        .map_err(|err| map_error("delete_workspace docs", err))?
        .num_deleted_rows as usize;

    deleted += delete_feedback_for(store, ctx, &chunk_ids).await?;

    Ok(DeleteReport {
        deleted_count: deleted,
        freed_bytes: 0,
        chore_id: ChoreId::new(),
    })
}

/// Delete EVERYTHING in the tenant (the erase flow's first step, REQ-CG-001).
pub async fn delete_tenant(
    store: &LanceStore,
    ctx: &TenantContext,
) -> Result<DeleteReport, DomainError> {
    store.ensure_tenant(ctx)?;
    let scope = tenant_scope(ctx.tenant_id());

    let mut deleted = 0usize;
    for table_name in [
        CHUNKS,
        crate::schema::DOCS,
        crate::schema::FEEDBACK,
        crate::schema::SYMBOLS,
    ] {
        let table = store.table(table_name).await?;
        deleted += table
            .delete(&scope)
            .await
            .map_err(|err| map_error(&format!("delete_tenant {table_name}"), err))?
            .num_deleted_rows as usize;
    }

    Ok(DeleteReport {
        deleted_count: deleted,
        freed_bytes: 0,
        chore_id: ChoreId::new(),
    })
}

// --- purge chain ----------------------------------------------------------------

/// Compact all tenant tables (rewrite files; clean latest version).
pub async fn compact(store: &LanceStore, ctx: &TenantContext) -> Result<(), DomainError> {
    store.ensure_tenant(ctx)?;
    for table_name in ALL_TABLES {
        let table = store.table(table_name).await?;
        let rows = table
            .count_rows(None)
            .await
            .map_err(|err| map_error("count_rows", err))?;
        if rows == 0 {
            continue;
        }
        table
            .optimize(OptimizeAction::Compact {
                options: CompactionOptions::default(),
                remap_options: None,
            })
            .await
            .map_err(|err| map_error(&format!("compact {table_name}"), err))?;
    }
    Ok(())
}

/// Prune ALL old versions from every tenant table (aggressive; keeps only the
/// current version). This is what makes deleted rows unrecoverable (GDPR).
pub async fn prune(store: &LanceStore, ctx: &TenantContext) -> Result<(), DomainError> {
    store.ensure_tenant(ctx)?;
    for table_name in ALL_TABLES {
        let table = store.table(table_name).await?;
        table
            .optimize(OptimizeAction::Prune {
                // older_than = 0 → keep ONLY the current version. Lance
                // models the threshold as a chrono duration.
                older_than: Some(chrono::Duration::zero()),
                delete_unverified: Some(true),
                error_if_tagged_old_versions: Some(false),
            })
            .await
            .map_err(|err| map_error(&format!("prune {table_name}"), err))?;
    }
    Ok(())
}

/// Full tenant erasure chain: delete → compact → prune (discovery 2573).
/// After this returns, deleted rows are unrecoverable INCLUDING old versions.
pub async fn erase(store: &LanceStore, ctx: &TenantContext) -> Result<DeleteReport, DomainError> {
    store.ensure_tenant(ctx)?;
    let report = delete_tenant(store, ctx).await?;
    compact(store, ctx).await?;
    prune(store, ctx).await?;
    Ok(report)
}

/// Remove chunks older than `cutoff` (retention sweep, REQ-ML-003, design D5).
pub async fn sweep_expired(
    store: &LanceStore,
    ctx: &TenantContext,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<SweepReport, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(CHUNKS).await?;
    let filter = tenant_scope(ctx.tenant_id()).and(created_before(cutoff));
    let result = table
        .delete(&filter)
        .await
        .map_err(|err| map_error("sweep_expired", err))?;
    Ok(SweepReport {
        expired_count: result.num_deleted_rows as usize,
        freed_bytes: 0,
        chore_id: ChoreId::new(),
    })
}

// --- version inspection (diagnosis / GDPR evidence) -----------------------------

/// List the dataset versions of the chunks table (time-travel forensics).
pub async fn list_versions(
    store: &LanceStore,
    ctx: &TenantContext,
) -> Result<Vec<VersionSummary>, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(CHUNKS).await?;
    let versions = table
        .list_versions()
        .await
        .map_err(|err| map_error("list_versions", err))?;
    Ok(versions
        .into_iter()
        .map(|v| VersionSummary {
            version: v.version,
            timestamp: v.timestamp,
        })
        .collect())
}

/// Read the tenant's rows as they existed at `version` (old-version
/// inspection). Errors once the version has been pruned — which is the
/// GDPR evidence that deleted rows are no longer recoverable.
pub async fn version_snapshot(
    store: &LanceStore,
    ctx: &TenantContext,
    version: u64,
) -> Result<Vec<memento_domain::MemoryChunk>, DomainError> {
    store.ensure_tenant(ctx)?;
    let table = store.table(CHUNKS).await?;
    table
        .checkout(version)
        .await
        .map_err(|err| map_error(&format!("checkout version {version}"), err))?;

    let rows = async {
        let stream = table
            .query()
            .only_if_expr(tenant_scope(ctx.tenant_id()))
            .execute()
            .await
            .map_err(|err| map_error("version_snapshot", err))?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|err| map_error("version_snapshot", err))?;
        let mut chunks = Vec::new();
        for batch in &batches {
            for row in 0..batch.num_rows() {
                chunks.push(row_to_chunk(batch, row)?);
            }
        }
        Ok::<_, DomainError>(chunks)
    }
    .await;

    // Always restore the latest version, whatever happened above.
    let restore = table.checkout_latest().await;
    let chunks = rows?;
    restore.map_err(|err| map_error("checkout_latest", err))?;
    Ok(chunks)
}

// --- helpers ---------------------------------------------------------------------

fn empty_report() -> DeleteReport {
    DeleteReport {
        deleted_count: 0,
        freed_bytes: 0,
        chore_id: ChoreId::new(),
    }
}

fn report(rows: u64) -> DeleteReport {
    // freed_bytes stays 0 at delete time: space is only reclaimed by
    // compact/prune (documented in the module docs).
    DeleteReport {
        deleted_count: rows as usize,
        freed_bytes: 0,
        chore_id: ChoreId::new(),
    }
}

/// Chunk ids matching a tenant-scoped filter (used to cascade into feedback).
async fn chunk_ids_for(
    store: &LanceStore,
    filter: lancedb::expr::DfExpr,
) -> Result<Vec<ChunkId>, DomainError> {
    let table = store.table(CHUNKS).await?;
    let stream = table
        .query()
        .only_if_expr(filter)
        .execute()
        .await
        .map_err(|err| map_error("chunk_ids_for", err))?;
    let batches: Vec<RecordBatch> = stream
        .try_collect()
        .await
        .map_err(|err| map_error("chunk_ids_for", err))?;
    let mut ids = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            let chunk = row_to_chunk(batch, row)?;
            ids.push(chunk.id);
        }
    }
    Ok(ids)
}

async fn delete_feedback_for(
    store: &LanceStore,
    ctx: &TenantContext,
    chunk_ids: &[ChunkId],
) -> Result<usize, DomainError> {
    if chunk_ids.is_empty() {
        return Ok(0);
    }
    let table = store.table(crate::schema::FEEDBACK).await?;
    let filter = tenant_scope(ctx.tenant_id()).and(is_in(
        col(crate::schema::COL_CHUNK_ID),
        chunk_ids
            .iter()
            .map(|id| lit(id.to_string()))
            .collect::<Vec<_>>(),
    ));
    let deleted = table
        .delete(&filter)
        .await
        .map_err(|err| map_error("delete_feedback", err))?
        .num_deleted_rows as usize;
    Ok(deleted)
}

// --- port -------------------------------------------------------------------------

#[async_trait::async_trait]
impl LifecyclePort for LanceStore {
    async fn delete(
        &self,
        ctx: &TenantContext,
        scope: DeleteScope,
    ) -> Result<DeleteReport, DomainError> {
        match scope {
            DeleteScope::Chunk { id } => delete_chunks(self, ctx, &[id]).await,
            DeleteScope::Doc { id } => delete_doc(self, ctx, &id).await,
            DeleteScope::Workspace { id } => delete_workspace(self, ctx, &id).await,
            // A tenant scope naming a DIFFERENT tenant is a boundary
            // violation, not a no-op (REQ-TA-004).
            DeleteScope::Tenant { id } if &id == ctx.tenant_id() => delete_tenant(self, ctx).await,
            DeleteScope::Tenant { .. } => Err(DomainError::TenantForbidden),
        }
    }

    async fn compact(&self, ctx: &TenantContext) -> Result<(), DomainError> {
        compact(self, ctx).await
    }

    async fn prune(&self, ctx: &TenantContext) -> Result<(), DomainError> {
        prune(self, ctx).await
    }

    async fn sweep_expired(
        &self,
        ctx: &TenantContext,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<SweepReport, DomainError> {
        sweep_expired(self, ctx, cutoff).await
    }

    async fn erase(&self, ctx: &TenantContext) -> Result<DeleteReport, DomainError> {
        erase(self, ctx).await
    }
}
