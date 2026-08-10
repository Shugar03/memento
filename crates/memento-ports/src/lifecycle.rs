//! Memory-lifecycle port: delete, maintenance, and erasure (REQ-ML-*,
//! REQ-CG-001, design D5).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use memento_domain::{ChoreId, ChunkId, DocId, DomainError, TenantContext, TenantId, WorkspaceId};
use serde::{Deserialize, Serialize};

/// What a delete operation removes (REQ-ML-002).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DeleteScope {
    Chunk { id: ChunkId },
    Doc { id: DocId },
    Workspace { id: WorkspaceId },
    Tenant { id: TenantId },
}

/// Outcome of a delete / erase operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeleteReport {
    pub deleted_count: usize,
    pub freed_bytes: u64,
    pub chore_id: ChoreId,
}

/// Outcome of a retention sweep (REQ-ML-003, design D5).
///
/// `audit_expired_count` is the number of audit JSONL lines removed by
/// the same sweep when the tenant has a per-tenant `audit_retention_days`
/// override (T-120); default behavior is `0` (audit retention matches
/// data retention and is reported by the same sweep).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepReport {
    pub expired_count: usize,
    pub freed_bytes: u64,
    pub chore_id: ChoreId,
    /// Audit JSONL lines removed by the sweep (T-120). `0` when the
    /// tenant opts out of audit retention (or when no line is past TTL).
    #[serde(default)]
    pub audit_expired_count: usize,
}

/// Lifecycle boundary: hard delete (REQ-ML-002), purge-chain maintenance
/// (delete -> Compact -> Prune, design D5), retention sweep with injectable
/// cutoff (REQ-ML-003), and tenant erasure (REQ-CG-001, design D4).
#[async_trait]
pub trait LifecyclePort: Send + Sync {
    /// Permanently delete within the bound tenant (cross-tenant scopes
    /// surface `NOT_FOUND`).
    async fn delete(
        &self,
        ctx: &TenantContext,
        scope: DeleteScope,
    ) -> Result<DeleteReport, DomainError>;

    /// Compact the store once (clean latest version for backups).
    async fn compact(&self, ctx: &TenantContext) -> Result<(), DomainError>;

    /// Prune old store versions.
    async fn prune(&self, ctx: &TenantContext) -> Result<(), DomainError>;

    /// Remove chunks older than `cutoff` (worker sweep, injectable clock).
    async fn sweep_expired(
        &self,
        ctx: &TenantContext,
        cutoff: DateTime<Utc>,
    ) -> Result<SweepReport, DomainError>;

    /// Full tenant erasure: delete -> Compact -> Prune + credential/key
    /// destruction handled by the caller chain (REQ-CG-001).
    async fn erase(&self, ctx: &TenantContext) -> Result<DeleteReport, DomainError>;
}
