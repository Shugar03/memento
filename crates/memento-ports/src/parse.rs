//! Document parsing port (REQ-MC-002): the single normalization boundary to
//! Markdown (anydoc subprocess in the adapter, fallback for md/txt).

use async_trait::async_trait;
use memento_domain::{DomainError, SourceKind};
use serde::{Deserialize, Serialize};

/// A document normalized to Markdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedDocument {
    pub markdown: String,
    pub source_kind: SourceKind,
    pub metadata: serde_json::Value,
}

/// Parsing boundary. Failures are structured and stage-named (REQ-MC-007);
/// the adapter enforces the 60s timeout / 50MB stdout cap / argv validation
/// (threat matrix, T-030).
#[async_trait]
pub trait ParsePort: Send + Sync {
    /// Normalize a document blob to Markdown.
    async fn parse(&self, blob: &[u8], hint: SourceKind) -> Result<ParsedDocument, DomainError>;
}
