//! Reranking port (A1 cross-encoder).
//!
//! The reranker is an OPTIONAL retrieval post-processor: the adapter loads a
//! cross-encoder ONNX model (`bge-reranker-v2-m3` int8) lazily and scores the
//! fused candidate chunks so the application can reorder the top-k by deep
//! relevance instead of fused rank alone. Two gates decide whether the cost
//! is ever paid:
//!
//! * **Capability** — `MEMENTO_RERANK=1` enables model loading ([`RerankPort::is_enabled`]).
//! * **Per-query opt-in** — `SearchQuery.rerank` opts a specific search into
//!   paying the +50-100ms (more on the ~568M-param model) inference cost.
//!
//! When the capability is off and a query still asks for rerank, the
//! application logs a warning and keeps the fused order — never a silent
//! degradation.

use async_trait::async_trait;
use memento_domain::DomainError;

/// Reranking boundary. Implementations score `(query, text)` pairs; a higher
/// score means the text is more relevant to the query. The adapter batches
/// internally and returns one score per input text, aligned with the input
/// order (NOT sorted).
#[async_trait]
pub trait RerankPort: Send + Sync {
    /// Score each candidate text against the query. `scores[i]` corresponds
    /// to `texts[i]`; higher = more relevant.
    async fn rerank(&self, query: &str, texts: &[&str]) -> Result<Vec<f32>, DomainError>;

    /// Whether the reranker capability is enabled on this process
    /// (`MEMENTO_RERANK=1`). When `false`, the model is never loaded and
    /// per-query opt-ins degrade to the fused order.
    fn is_enabled(&self) -> bool;

    /// The reranker model version label, when enabled.
    fn model_version(&self) -> Option<&'static str>;
}
