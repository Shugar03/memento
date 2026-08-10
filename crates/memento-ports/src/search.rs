//! Search port and its DTOs (REQ-MR-001/002/003/005/006).
//!
//! `SearchQuery.workspace_id` is MANDATORY (REQ-MR-006): a query without
//! workspace scope cannot be constructed — the field is required and there is
//! no `Default` implementation.

use async_trait::async_trait;
use memento_domain::{
    ChunkId, DocId, DomainError, MemoryChunk, Provenance, SourceKind, TenantContext, WorkspaceId,
};
use serde::{Deserialize, Serialize};

/// Search filters (MVP: optional; tenant/workspace scoping is implied by the
/// bound context, workspace filtering by `SearchQuery.workspace_id`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchFilters {
    /// Restrict to a single source document.
    pub doc_id: Option<DocId>,
    /// Restrict to a source kind.
    pub source: Option<SourceKind>,
}

/// A search request. The workspace is mandatory (REQ-MR-006): the caller must
/// always say which workspace to search.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub query: String,
    pub top_k: usize,
    pub workspace_id: WorkspaceId,
    /// Hybrid retrieval (vector + FTS fused with RRF); off by default
    /// (REQ-MR-002/003).
    pub rrf_enabled: bool,
    pub filters: Option<SearchFilters>,
}

impl SearchQuery {
    /// Build a query. `rrf_enabled` defaults to `false`, filters to `None`.
    pub fn new(query: impl Into<String>, top_k: usize, workspace_id: WorkspaceId) -> Self {
        Self {
            query: query.into(),
            top_k,
            workspace_id,
            rrf_enabled: false,
            filters: None,
        }
    }
}

/// A retrieval hit: the chunk plus its provenance (REQ-MC-006).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub chunk_id: ChunkId,
    pub text: String,
    pub score: f32,
    pub provenance: Provenance,
}

/// Retrieval boundary: full-text search always; hybrid (RRF) behind the
/// per-query toggle (REQ-MR-001/002/003). `get_chunk` is tenant-scoped and
/// foreign ids surface as `NOT_FOUND` (REQ-MR-005).
#[async_trait]
pub trait SearchPort: Send + Sync {
    /// Search the workspace for the query, returning ranked hits.
    async fn search(
        &self,
        ctx: &TenantContext,
        query: SearchQuery,
    ) -> Result<Vec<SearchHit>, DomainError>;

    /// Fetch one chunk by id within the bound tenant.
    async fn get_chunk(
        &self,
        ctx: &TenantContext,
        id: &ChunkId,
    ) -> Result<Option<MemoryChunk>, DomainError>;
}
