//! Memento RS port traits (hexagonal architecture, design D1/D2).
//!
//! Ports are the boundary between the domain and the adapters: every adapter
//! (lancedb, embed-fastembed, parse, okf, tenant) implements one or more of
//! these traits, and the application layer consumes only these shapes. All
//! traits are `#[async_trait]`, `Send + Sync`, and speak `memento_domain`
//! types + [`DomainError`] exclusively.

pub mod embed;
pub mod ingest;
pub mod knowledge;
pub mod lifecycle;
pub mod parse;
pub mod rerank;
pub mod search;
pub mod tenant_resolver;

pub use embed::EmbedPort;
pub use ingest::{IngestDocumentRequest, IngestPort, IngestResult, IngestTextRequest, Metadata};
pub use knowledge::{KnowledgePort, ProjectOverview};
pub use lifecycle::{DeleteReport, DeleteScope, LifecyclePort, SweepReport};
pub use parse::{ParsePort, ParsedDocument};
pub use rerank::RerankPort;
pub use search::{DEFAULT_RRF_K, SearchFilters, SearchHit, SearchPort, SearchQuery};
pub use tenant_resolver::TenantResolver;

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use memento_domain::{
        AgentId, ChunkId, DomainError, KnowledgeArtifact, MemoryChunk, SourceKind, TenantContext,
        TenantId, WorkspaceId,
    };

    #[test]
    fn search_query_requires_workspace() {
        // REQ-MR-006: a SearchQuery cannot exist without a workspace — the
        // field is mandatory, the constructor requires it, and there is no
        // Default impl (nothing can be built workspace-less).
        let workspace_id = WorkspaceId::new();
        let q = SearchQuery::new("hola mundo", 5, workspace_id);

        assert_eq!(q.workspace_id, workspace_id, "workspace preserved");
        assert_eq!(q.query, "hola mundo");
        assert_eq!(q.top_k, 5);
        assert!(!q.rrf_enabled, "RRF must default to off (REQ-MR-002)");
        assert_eq!(q.rrf_k, search::DEFAULT_RRF_K, "RRF k defaults to 60");
        assert!(!q.rerank, "rerank must default to off (A1 opt-in)");
        assert!(q.filters.is_none());

        // Type-level proof: the only constructor takes a WorkspaceId by value;
        // a missing workspace is unrepresentable (compile-only, no Option).
        let _ = SearchQuery::new("x", 1, WorkspaceId::new());
    }

    #[test]
    fn traits_compile() {
        // Compile-only proof: every trait is object-safe, async, and usable
        // through a `Box<dyn Trait>` (which requires Send + Sync supertraits).
        struct AllPorts;

        #[async_trait]
        impl SearchPort for AllPorts {
            async fn search(
                &self,
                _ctx: &TenantContext,
                _q: SearchQuery,
            ) -> Result<Vec<SearchHit>, DomainError> {
                unimplemented!()
            }
            async fn get_chunk(
                &self,
                _ctx: &TenantContext,
                _id: &ChunkId,
            ) -> Result<Option<MemoryChunk>, DomainError> {
                unimplemented!()
            }
        }

        #[async_trait]
        impl IngestPort for AllPorts {
            async fn ingest_text(
                &self,
                _ctx: &TenantContext,
                _req: IngestTextRequest,
            ) -> Result<IngestResult, DomainError> {
                unimplemented!()
            }
            async fn ingest_document(
                &self,
                _ctx: &TenantContext,
                _req: IngestDocumentRequest,
            ) -> Result<IngestResult, DomainError> {
                unimplemented!()
            }
        }

        #[async_trait]
        impl EmbedPort for AllPorts {
            async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError> {
                unimplemented!()
            }
        }

        #[async_trait]
        impl RerankPort for AllPorts {
            async fn rerank(&self, _query: &str, _texts: &[&str]) -> Result<Vec<f32>, DomainError> {
                unimplemented!()
            }
            fn is_enabled(&self) -> bool {
                true
            }
            fn model_version(&self) -> Option<&'static str> {
                None
            }
        }

        #[async_trait]
        impl TenantResolver for AllPorts {
            async fn resolve(
                &self,
                _token: &str,
                _agent_id: AgentId,
            ) -> Result<TenantContext, DomainError> {
                unimplemented!()
            }
            async fn rotate_token(&self, _tenant_id: &TenantId) -> Result<String, DomainError> {
                unimplemented!()
            }
        }

        #[async_trait]
        impl LifecyclePort for AllPorts {
            async fn delete(
                &self,
                _ctx: &TenantContext,
                _scope: DeleteScope,
            ) -> Result<DeleteReport, DomainError> {
                unimplemented!()
            }
            async fn compact(&self, _ctx: &TenantContext) -> Result<(), DomainError> {
                unimplemented!()
            }
            async fn prune(&self, _ctx: &TenantContext) -> Result<(), DomainError> {
                unimplemented!()
            }
            async fn sweep_expired(
                &self,
                _ctx: &TenantContext,
                _cutoff: chrono::DateTime<Utc>,
            ) -> Result<SweepReport, DomainError> {
                unimplemented!()
            }
            async fn erase(&self, _ctx: &TenantContext) -> Result<DeleteReport, DomainError> {
                unimplemented!()
            }
        }

        #[async_trait]
        impl KnowledgePort for AllPorts {
            async fn project_overview(
                &self,
                _ctx: &TenantContext,
                _project_id: &str,
            ) -> Result<ProjectOverview, DomainError> {
                unimplemented!()
            }
            async fn symbol_lookup(
                &self,
                _ctx: &TenantContext,
                _project_id: &str,
                _symbol: &str,
            ) -> Result<Option<KnowledgeArtifact>, DomainError> {
                unimplemented!()
            }
            async fn callers_of(
                &self,
                _ctx: &TenantContext,
                _project_id: &str,
                _symbol: &str,
            ) -> Result<Vec<String>, DomainError> {
                unimplemented!()
            }
            async fn callees_of(
                &self,
                _ctx: &TenantContext,
                _project_id: &str,
                _symbol: &str,
            ) -> Result<Vec<String>, DomainError> {
                unimplemented!()
            }
            async fn impact(
                &self,
                _ctx: &TenantContext,
                _project_id: &str,
                _symbol: &str,
            ) -> Result<Vec<String>, DomainError> {
                unimplemented!()
            }
            async fn dependencies(
                &self,
                _ctx: &TenantContext,
                _project_id: &str,
            ) -> Result<Vec<String>, DomainError> {
                unimplemented!()
            }
            async fn search(
                &self,
                _ctx: &TenantContext,
                _project_id: &str,
                _query: &str,
                _limit: usize,
            ) -> Result<Vec<KnowledgeArtifact>, DomainError> {
                unimplemented!()
            }
            async fn graph_dump(
                &self,
                _ctx: &TenantContext,
                _project_id: &str,
            ) -> Result<serde_json::Value, DomainError> {
                unimplemented!()
            }
        }

        #[async_trait]
        impl ParsePort for AllPorts {
            async fn parse(
                &self,
                _blob: &[u8],
                _hint: SourceKind,
            ) -> Result<ParsedDocument, DomainError> {
                unimplemented!()
            }
        }

        // Trait objects: requires Send + Sync supertraits on every trait.
        let _search: Box<dyn SearchPort> = Box::new(AllPorts);
        let _ingest: Box<dyn IngestPort> = Box::new(AllPorts);
        let _embed: Box<dyn EmbedPort> = Box::new(AllPorts);
        let _rerank: Box<dyn RerankPort> = Box::new(AllPorts);
        let _resolver: Box<dyn TenantResolver> = Box::new(AllPorts);
        let _lifecycle: Box<dyn LifecyclePort> = Box::new(AllPorts);
        let _knowledge: Box<dyn KnowledgePort> = Box::new(AllPorts);
        let _parse: Box<dyn ParsePort> = Box::new(AllPorts);

        // The DomainError taxonomy is the single error type across ports.
        let _: DomainError = DomainError::WorkspaceRequired;
    }
}
