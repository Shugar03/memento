//! memento-embed-fastembed — embedding + cross-encoder rerank adapters
//! (design D1/D8, A1).
//!
//! MultilingualE5Base (768 dims) via fastembed with `ort-load-dynamic`
//! (no prebuilt onnxruntime for windows-gnu — see docs/dependencies.md).
//! [`model::ModelLoader`] is lazy, single-flight and `--no-embeddings`
//! aware (REQ-MC-004); [`embedder::FastEmbedEmbedder`] exposes it through
//! the [`EmbedPort`] trait with 64-text batches.
//!
//! The cross-encoder reranker (A1) mirrors that loader: [`rerank::Reranker`]
//! lazily loads `bge-reranker-v2-m3` (int8, multilingual) behind the
//! `MEMENTO_RERANK` capability toggle and [`rerank::FastReranker`] exposes it
//! through the [`RerankPort`] trait.

pub mod dylib;
pub mod embedder;
pub mod model;
pub mod rerank;

pub use embedder::{FastEmbedEmbedder, MAX_TEXTS_PER_CALL};
pub use model::{
    EMBEDDING_DIM, EmbeddingBackend, FastEmbedBackend, MAX_BATCH, MODEL_VERSION, ModelLoader,
};
pub use rerank::{
    FastRerankBackend, FastReranker, MAX_RERANK_LEN, RERANK_MODEL_VERSION, RerankBackend, Reranker,
};
