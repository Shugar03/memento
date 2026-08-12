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

/// Standard RRF fusion constant (k=60, Cormack et al.): scores flatten as k
/// grows, so lower k weights high ranks more. Spanish-tuned value may differ
/// from the English-centric default (see fix/rrf-bm25-es-tuning).
pub const DEFAULT_RRF_K: f32 = 60.0;

fn default_rrf_k() -> f32 {
    DEFAULT_RRF_K
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
    /// RRF fusion constant k (hybrid mode only). Defaults to the standard 60;
    /// per-query override for Spanish tuning.
    #[serde(default = "default_rrf_k")]
    pub rrf_k: f32,
    /// Cross-encoder rerank of the fused top-10 (A1, hybrid mode only,
    /// opt-in): reorders the candidates by deep relevance and returns the
    /// top-5. Requires the reranker capability (`MEMENTO_RERANK=1`); when
    /// unset the query keeps the fused order with a warning. Off by default.
    #[serde(default)]
    pub rerank: bool,
    pub filters: Option<SearchFilters>,
}

impl SearchQuery {
    /// Build a query. `rrf_enabled` defaults to `false`, `rrf_k` to the
    /// standard 60, `rerank` to `false`, filters to `None`.
    pub fn new(query: impl Into<String>, top_k: usize, workspace_id: WorkspaceId) -> Self {
        Self {
            query: query.into(),
            top_k,
            workspace_id,
            rrf_enabled: false,
            rrf_k: DEFAULT_RRF_K,
            rerank: false,
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
