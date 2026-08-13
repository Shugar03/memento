//! Embedding port (REQ-MC-004).

use async_trait::async_trait;
use memento_domain::DomainError;

/// Embedding boundary. The adapter batches internally (batch <= 64 in the
/// fastembed adapter); oversized batches surface `RESOURCE_EXHAUSTED`.
#[async_trait]
pub trait EmbedPort: Send + Sync {
    /// Embed each text; the output vectors line up with the input order.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError>;

    /// The version label of the model actually loaded by this embedder
    /// (REQ-OBS-012, design D3): the single source of truth for chunk
    /// provenance. `None` = unknown (default: existing implementors that
    /// predate the label contract keep compiling untouched — same precedent
    /// as [`memento_ports::RerankPort::model_version`]). The application
    /// stamps this label on every chunk; it must reflect the real loaded
    /// model, never an env-only guess.
    fn model_version(&self) -> Option<&'static str> {
        None
    }
}
