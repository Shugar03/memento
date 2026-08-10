//! Embedding port (REQ-MC-004).

use async_trait::async_trait;
use memento_domain::DomainError;

/// Embedding boundary. The adapter batches internally (batch <= 64 in the
/// fastembed adapter); oversized batches surface `RESOURCE_EXHAUSTED`.
#[async_trait]
pub trait EmbedPort: Send + Sync {
    /// Embed each text; the output vectors line up with the input order.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError>;
}
