//! Ingest port and its DTOs (REQ-MC-001/002/007).

use async_trait::async_trait;
use memento_domain::{ChoreId, ChunkId, DocId, DomainError, SourceKind, TenantContext};
use serde::{Deserialize, Serialize};

/// Free-form metadata attached at ingest (REQ-MC-001).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metadata(pub serde_json::Map<String, serde_json::Value>);

/// Request for `IngestPort::ingest_text` (REQ-MC-001).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestTextRequest {
    pub text: String,
    /// Auto-generated when `None`.
    pub doc_id: Option<DocId>,
    pub metadata: Option<Metadata>,
}

/// Request for `IngestPort::ingest_document` (REQ-MC-002).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestDocumentRequest {
    pub blob: Vec<u8>,
    pub source_hint: SourceKind,
    /// Auto-generated when `None`.
    pub doc_id: Option<DocId>,
    pub metadata: Option<Metadata>,
}

/// Outcome of an ingest operation: produced chunk ids plus the chore id that
/// makes the operation observable (REQ-MC-007).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestResult {
    pub chunk_ids: Vec<ChunkId>,
    pub doc_id: DocId,
    pub chore_id: Option<ChoreId>,
}

/// Ingest boundary: the only writer of memory chunks (REQ-MC-001/002).
/// Implementations MUST be atomic (REQ-MC-007): chunks become visible to
/// retrieval only after the operation commits.
#[async_trait]
pub trait IngestPort: Send + Sync {
    /// Run the full pipeline (chunk -> embed -> store) on raw text.
    async fn ingest_text(
        &self,
        ctx: &TenantContext,
        req: IngestTextRequest,
    ) -> Result<IngestResult, DomainError>;

    /// Normalize a document blob and run the same pipeline (REQ-MC-002).
    async fn ingest_document(
        &self,
        ctx: &TenantContext,
        req: IngestDocumentRequest,
    ) -> Result<IngestResult, DomainError>;
}
