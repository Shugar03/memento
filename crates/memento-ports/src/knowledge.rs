//! Code-knowledge port (REQ-CK-*, design layers L1..L4). All methods are
//! read-only; indexing happens through the CLI (`memento code index`).

use async_trait::async_trait;
use memento_domain::{DomainError, KnowledgeArtifact, TenantContext};
use serde::{Deserialize, Serialize};

/// Project overview: L4 architectural summary (design).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectOverview {
    pub project_id: String,
    pub summary: String,
    pub artifact_count: usize,
}

/// Code-knowledge boundary: L1 bundles, L2 symbol map, L3 graph, L4 summaries.
#[async_trait]
pub trait KnowledgePort: Send + Sync {
    /// Architectural summary of the project (L4).
    async fn project_overview(
        &self,
        ctx: &TenantContext,
        project_id: &str,
    ) -> Result<ProjectOverview, DomainError>;

    /// Look up one symbol (L2); `None` for unknown symbols (REQ-CK-004).
    async fn symbol_lookup(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Option<KnowledgeArtifact>, DomainError>;

    /// Who calls a symbol (L3, depth-2 traversal, REQ-CK-005).
    async fn callers_of(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Vec<String>, DomainError>;

    /// What a symbol calls (L3, depth-2 traversal, REQ-CK-005).
    async fn callees_of(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Vec<String>, DomainError>;

    /// Reverse impact: what would break if the symbol changes (REQ-CK-006).
    async fn impact(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        symbol: &str,
    ) -> Result<Vec<String>, DomainError>;

    /// Project dependencies with cycle detection (REQ-CK-007).
    async fn dependencies(
        &self,
        ctx: &TenantContext,
        project_id: &str,
    ) -> Result<Vec<String>, DomainError>;

    /// Search code by symbol or text (L2 + FTS; literal under
    /// `--no-embeddings`, REQ-CK-008).
    async fn search(
        &self,
        ctx: &TenantContext,
        project_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeArtifact>, DomainError>;

    /// Canonical `{nodes, edges}` graph (L3, REQ-CK-009).
    async fn graph_dump(
        &self,
        ctx: &TenantContext,
        project_id: &str,
    ) -> Result<serde_json::Value, DomainError>;
}
