//! EmbedPort adapter (T-024).
//!
//! [`FastEmbedEmbedder`] turns the [`ModelLoader`] into an async port:
//! inputs are split into batches of [`MAX_BATCH`] and pushed onto the
//! blocking pool (ONNX inference is CPU-bound). Failures propagate as
//! structured `EMBEDDING_FAILED` errors; oversized calls surface
//! `RESOURCE_EXHAUSTED` before touching the model.

use crate::model::{MAX_BATCH, ModelLoader};
use async_trait::async_trait;
use memento_domain::DomainError;
use memento_ports::EmbedPort;
use std::sync::Arc;

/// Hard cap per embed call (matches the ingest limit of 10k chunks/doc,
/// REQ-MC-004 — the application batches at this boundary).
pub const MAX_TEXTS_PER_CALL: usize = 10_000;

/// [`EmbedPort`] adapter over the fastembed model loader.
pub struct FastEmbedEmbedder {
    loader: Arc<ModelLoader>,
}

impl FastEmbedEmbedder {
    pub fn new(loader: Arc<ModelLoader>) -> Self {
        Self { loader }
    }

    /// Whether embeddings are enabled (`--no-embeddings` mode → false).
    pub fn is_enabled(&self) -> bool {
        self.loader.is_enabled()
    }

    /// The active model version, when embeddings are enabled.
    pub fn model_version(&self) -> Option<&'static str> {
        self.loader.model_version()
    }
}

#[async_trait]
impl EmbedPort for FastEmbedEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError> {
        // Defense in depth: the application must not call embed in
        // --no-embeddings mode (REQ-MC-004 explicit-absence contract).
        if !self.loader.is_enabled() {
            return Err(DomainError::EmbeddingFailed {
                message: "embeddings disabled (--no-embeddings)".into(),
            });
        }
        if texts.len() > MAX_TEXTS_PER_CALL {
            return Err(DomainError::ResourceExhausted {
                message: format!(
                    "embed call has {} texts, cap is {MAX_TEXTS_PER_CALL}",
                    texts.len()
                ),
            });
        }
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let backend = self.loader.backend()?.clone();
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();

        let result = tokio::task::spawn_blocking(move || {
            let mut out = Vec::with_capacity(owned.len());
            for batch in owned.chunks(MAX_BATCH) {
                let batch_refs: Vec<&str> = batch.iter().map(String::as_str).collect();
                out.extend(backend.embed_batch(&batch_refs)?);
            }
            Ok::<Vec<Vec<f32>>, DomainError>(out)
        })
        .await
        .map_err(|err| DomainError::Internal {
            message: format!("embed task failed: {err}"),
        })?;

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EmbeddingBackend, MODEL_VERSION};
    use std::sync::Mutex;

    /// Stub backend that records the sizes of the batches it receives.
    struct RecordingBackend {
        calls: Mutex<Vec<usize>>,
        dim: usize,
    }

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                dim: 384,
            }
        }

        fn call_sizes(&self) -> Vec<usize> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl EmbeddingBackend for RecordingBackend {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError> {
            self.calls.lock().expect("lock").push(texts.len());
            Ok(texts
                .iter()
                .map(|t| memento_testkit::deterministic_embed(t, self.dim))
                .collect())
        }

        fn model_version(&self) -> &'static str {
            MODEL_VERSION
        }
    }

    /// Backend that always fails (structured error propagation).
    struct FailingBackend;

    impl EmbeddingBackend for FailingBackend {
        fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError> {
            Err(DomainError::EmbeddingFailed {
                message: "onnx exploded".into(),
            })
        }

        fn model_version(&self) -> &'static str {
            MODEL_VERSION
        }
    }

    fn embedder_with(backend: Arc<dyn EmbeddingBackend>) -> FastEmbedEmbedder {
        let loader = ModelLoader::from_backend(std::path::PathBuf::new(), backend);
        FastEmbedEmbedder::new(Arc::new(loader))
    }

    #[tokio::test]
    async fn embeds_in_batches_of_64() {
        let backend = Arc::new(RecordingBackend::new());
        let embedder = embedder_with(backend.clone());

        let texts: Vec<String> = (0..65).map(|i| format!("texto {i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let out = embedder.embed(&refs).await.expect("embed");

        assert_eq!(out.len(), 65, "all texts embedded");
        assert_eq!(out[0], memento_testkit::deterministic_embed("texto 0", 384));
        assert_eq!(
            backend.call_sizes(),
            vec![64, 1],
            "batches split at the 64 boundary"
        );
    }

    #[tokio::test]
    async fn empty_input_is_ok() {
        let backend = Arc::new(RecordingBackend::new());
        let embedder = embedder_with(backend.clone());
        let out = embedder.embed(&[]).await.expect("empty embed");
        assert!(out.is_empty());
        assert_eq!(backend.call_sizes(), Vec::<usize>::new());
    }

    #[tokio::test]
    async fn oversized_batch_is_resource_exhausted() {
        let embedder = embedder_with(Arc::new(RecordingBackend::new()));
        let texts: Vec<&str> = vec!["x"; MAX_TEXTS_PER_CALL + 1];
        let err = embedder.embed(&texts).await.expect_err("cap enforced");
        assert_eq!(err.code(), memento_domain::error::CODE_RESOURCE_EXHAUSTED);
    }

    #[tokio::test]
    async fn backend_failure_propagates_structured() {
        let embedder = embedder_with(Arc::new(FailingBackend));
        let err = embedder.embed(&["algo"]).await.expect_err("failure");
        assert_eq!(err.code(), memento_domain::error::CODE_EMBEDDING_FAILED);
        assert!(err.to_string().contains("onnx exploded"));
    }

    #[tokio::test]
    async fn disabled_embedder_errors_structured() {
        let loader = ModelLoader::new(std::path::PathBuf::new(), false);
        let embedder = FastEmbedEmbedder::new(Arc::new(loader));
        assert!(!embedder.is_enabled());
        assert_eq!(embedder.model_version(), None);

        let err = embedder.embed(&["algo"]).await.expect_err("disabled");
        assert_eq!(err.code(), memento_domain::error::CODE_EMBEDDING_FAILED);
    }
}
