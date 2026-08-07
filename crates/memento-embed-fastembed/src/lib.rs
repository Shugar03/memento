//! memento-embed-fastembed — embedding adapter (design D1/D8).
//!
//! MultilingualE5Small (384 dims) via fastembed with `ort-load-dynamic`
//! (no prebuilt onnxruntime for windows-gnu — see docs/dependencies.md).
//! [`model::ModelLoader`] is lazy, single-flight and `--no-embeddings`
//! aware (REQ-MC-004); [`embedder::FastEmbedEmbedder`] exposes it through
//! the [`EmbedPort`] trait with 64-text batches.

pub mod embedder;
pub mod model;

pub use embedder::{FastEmbedEmbedder, MAX_TEXTS_PER_CALL};
pub use model::{EMBEDDING_DIM, MAX_BATCH, MODEL_VERSION, EmbeddingBackend, FastEmbedBackend, ModelLoader};
