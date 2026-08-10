//! Embedding model loader (T-023).
//!
//! Wraps fastembed's `TextEmbedding` (MultilingualE5Base, 768 dims) behind a
//! small [`EmbeddingBackend`] trait so tests can inject deterministic stubs
//! (no ONNX download, per testing-capabilities). [`ModelLoader`] owns the
//! lazy, single-flight initialization and the `--no-embeddings` mode: when
//! embeddings are disabled the loader returns `Ok(None)` — explicit absent
//! vectors (REQ-MC-004) — instead of failing.

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use memento_domain::DomainError;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Model version stamped on every chunk (REQ-MC-006) and checked on
/// retrieval (EMBEDDING_MODEL_MISMATCH surface).
pub const MODEL_VERSION: &str = "multilingual-e5-base-v0.0.3";
/// Embedding dimension (must match the lancedb chunks schema).
pub const EMBEDDING_DIM: usize = 768;
/// Texts per inference batch (fastembed internal batching, T-024 boundary).
pub const MAX_BATCH: usize = 64;

/// The embedding computation behind the loader (injectable for tests).
pub trait EmbeddingBackend: Send + Sync {
    /// Embed each text; output lines up with input order.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError>;
    /// The model version this backend produces embeddings for.
    fn model_version(&self) -> &'static str;
}

/// Real backend: fastembed `TextEmbedding` behind a mutex (embed takes
/// `&mut self`; the adapter is shared across tasks).
pub struct FastEmbedBackend {
    model: Mutex<TextEmbedding>,
}

impl FastEmbedBackend {
    /// Initialize the ONNX session. Model files are cached under `cache_dir`
    /// (design D8: `models/`); the first call downloads them (documented,
    /// avoidable with `--no-embeddings` — REQ-CG-004).
    pub fn try_new(cache_dir: PathBuf) -> Result<Self, DomainError> {
        // Pre-load the vendored ONNX Runtime dylib before any fastembed
        // call reaches the ort C ABI. See `dylib.rs` for the full
        // rationale (System32's 1.17.x is rejected by ort 2.0.0-rc.13
        // which is compiled against the 1.28 API). Idempotent: the
        // internal `OnceLock` absorbs concurrent first-callers.
        crate::dylib::ensure_loaded();
        let options = TextInitOptions::new(EmbeddingModel::MultilingualE5Base)
            .with_cache_dir(cache_dir)
            .with_show_download_progress(false);
        let model =
            TextEmbedding::try_new(options).map_err(|err| DomainError::EmbeddingFailed {
                message: format!("init fastembed: {err}"),
            })?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }
}

impl EmbeddingBackend for FastEmbedBackend {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError> {
        let mut model = self.model.lock().map_err(|_| DomainError::Internal {
            message: "embedding model mutex poisoned".into(),
        })?;
        model
            .embed(texts, Some(MAX_BATCH))
            .map_err(|err| DomainError::EmbeddingFailed {
                message: err.to_string(),
            })
    }

    fn model_version(&self) -> &'static str {
        MODEL_VERSION
    }
}

type BackendFactory = Arc<dyn Fn() -> Result<Arc<dyn EmbeddingBackend>, DomainError> + Send + Sync>;

/// Lazy, single-flight model loader with an explicit disabled mode.
///
/// * **Lazy**: the model initializes on the first `embed`/`backend` call.
/// * **Single-flight**: concurrent first calls serialize on an init mutex —
///   one initialization, everyone else observes its result (including its
///   failure, which is cached and replayed, not retried).
/// * **Disabled**: `--no-embeddings` → [`Self::embed`] returns `Ok(None)`
///   (REQ-MC-004) and never touches the network.
pub struct ModelLoader {
    cache_dir: PathBuf,
    enabled: bool,
    factory: Option<BackendFactory>,
    backend: OnceLock<Arc<dyn EmbeddingBackend>>,
    /// Serializes initialization (single-flight).
    init_lock: Mutex<()>,
    /// Cached initialization failure message (replayed, not retried).
    init_error: Mutex<Option<String>>,
}

impl ModelLoader {
    /// Create a loader. With `enabled = false` (`--no-embeddings`) no model
    /// is ever loaded and [`Self::embed`] returns `Ok(None)`.
    pub fn new(cache_dir: PathBuf, enabled: bool) -> Self {
        let factory_cache = cache_dir.clone();
        let factory: BackendFactory = Arc::new(move || {
            let backend = FastEmbedBackend::try_new(factory_cache.clone())?;
            Ok(Arc::new(backend) as Arc<dyn EmbeddingBackend>)
        });
        Self {
            cache_dir,
            enabled,
            factory: Some(factory),
            backend: OnceLock::new(),
            init_lock: Mutex::new(()),
            init_error: Mutex::new(None),
        }
    }

    /// Test injection: a loader whose backend is pre-built (no ONNX
    /// download). `enabled` is forced on — the point of a pre-built backend
    /// is to be used.
    pub fn from_backend(cache_dir: PathBuf, backend: Arc<dyn EmbeddingBackend>) -> Self {
        Self {
            cache_dir,
            enabled: true,
            factory: None,
            backend: OnceLock::from(backend),
            init_lock: Mutex::new(()),
            init_error: Mutex::new(None),
        }
    }

    /// Whether embeddings are enabled (`--no-embeddings` mode → false).
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The model cache directory (design D8: `models/`).
    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }

    /// The model version, when enabled (None in `--no-embeddings` mode).
    pub fn model_version(&self) -> Option<&'static str> {
        self.enabled.then_some(MODEL_VERSION)
    }

    /// Access the backend, initializing it exactly once on first use
    /// (single-flight; failures are cached and replayed).
    pub fn backend(&self) -> Result<&Arc<dyn EmbeddingBackend>, DomainError> {
        if !self.enabled {
            return Err(DomainError::EmbeddingFailed {
                message: "embeddings disabled (--no-embeddings)".into(),
            });
        }
        if let Some(backend) = self.backend.get() {
            return Ok(backend);
        }

        // Single-flight: only one caller runs the factory; the rest wait on
        // the mutex and then observe the result (success or cached error).
        let _guard = self.init_lock.lock().map_err(|_| DomainError::Internal {
            message: "embedding init lock poisoned".into(),
        })?;
        if let Some(backend) = self.backend.get() {
            return Ok(backend);
        }
        if let Some(message) = self.init_error.lock().expect("init error lock").clone() {
            return Err(DomainError::EmbeddingFailed { message });
        }

        let factory = self.factory.as_ref().ok_or_else(|| DomainError::Internal {
            message: "loader has no backend factory".into(),
        })?;
        match factory() {
            Ok(backend) => {
                // Under the init lock we know the cell is still empty.
                let _ = self.backend.set(backend);
                Ok(self.backend.get().expect("backend set above"))
            }
            Err(err) => {
                let message = err.to_string();
                *self.init_error.lock().expect("init error lock") = Some(message.clone());
                Err(DomainError::EmbeddingFailed { message })
            }
        }
    }

    /// Embed texts. Returns `Ok(None)` when embeddings are disabled
    /// (explicit absent vectors, REQ-MC-004); `Ok(Some(vectors))` otherwise.
    pub fn embed(&self, texts: &[&str]) -> Result<Option<Vec<Vec<f32>>>, DomainError> {
        if !self.enabled {
            return Ok(None);
        }
        let backend = self.backend()?;
        Ok(Some(backend.embed_batch(texts)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counting stub backend: records how many times it was constructed.
    struct CountingBackend {
        dim: usize,
    }

    impl CountingBackend {
        fn new() -> Self {
            Self { dim: 768 }
        }
    }

    impl EmbeddingBackend for CountingBackend {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError> {
            Ok(texts
                .iter()
                .map(|t| memento_testkit::deterministic_embed(t, self.dim))
                .collect())
        }

        fn model_version(&self) -> &'static str {
            MODEL_VERSION
        }
    }

    #[test]
    fn no_embeddings_returns_none() {
        let loader = ModelLoader::new(PathBuf::from("nope"), false);
        assert!(!loader.is_enabled());
        assert_eq!(loader.model_version(), None);

        let out = loader
            .embed(&["hola"])
            .expect("disabled embed is not an error");
        assert!(out.is_none(), "explicit absent vectors (REQ-MC-004)");
    }

    #[test]
    fn disabled_loader_backend_errors() {
        let loader = ModelLoader::new(PathBuf::from("nope"), false);
        let result = loader.backend();
        assert!(result.is_err(), "no backend in disabled mode");
        let err = result.err().expect("error above");
        assert_eq!(err.code(), memento_domain::error::CODE_EMBEDDING_FAILED);
    }

    #[test]
    fn backend_initializes_once_and_is_shared() {
        let inits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&inits);
        let factory: BackendFactory = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(CountingBackend::new()) as Arc<dyn EmbeddingBackend>)
        });

        let loader = ModelLoader {
            cache_dir: PathBuf::new(),
            enabled: true,
            factory: Some(factory),
            backend: OnceLock::new(),
            init_lock: Mutex::new(()),
            init_error: Mutex::new(None),
        };

        let a = loader.backend().expect("first init");
        let b = loader.backend().expect("cached");
        assert!(
            Arc::ptr_eq(a, b),
            "same backend instance on repeated access"
        );
        assert_eq!(inits.load(Ordering::SeqCst), 1, "initialized exactly once");

        let out = loader
            .embed(&["uno", "dos"])
            .expect("embed")
            .expect("enabled");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn concurrent_backend_access_is_single_flight() {
        let inits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&inits);
        let factory: BackendFactory = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(CountingBackend::new()) as Arc<dyn EmbeddingBackend>)
        });

        let loader = Arc::new(ModelLoader {
            cache_dir: PathBuf::new(),
            enabled: true,
            factory: Some(factory),
            backend: OnceLock::new(),
            init_lock: Mutex::new(()),
            init_error: Mutex::new(None),
        });

        // 8 threads race on first access: exactly one initialization.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let loader = Arc::clone(&loader);
                std::thread::spawn(move || loader.backend().is_ok())
            })
            .collect();
        for handle in handles {
            assert!(
                handle.join().expect("thread"),
                "backend must initialize for all"
            );
        }
        assert_eq!(inits.load(Ordering::SeqCst), 1, "single-flight init");
    }

    #[test]
    fn injected_backend_is_used() {
        let loader = ModelLoader::from_backend(
            PathBuf::new(),
            Arc::new(CountingBackend::new()) as Arc<dyn EmbeddingBackend>,
        );
        assert!(loader.is_enabled());
        let out = loader.embed(&["memoria"]).expect("embed").expect("enabled");
        assert_eq!(out[0], memento_testkit::deterministic_embed("memoria", 768));
    }

    #[test]
    fn model_version_contract() {
        assert_eq!(MODEL_VERSION, "multilingual-e5-base-v0.0.3");
        assert_eq!(EMBEDDING_DIM, 768);
        assert_eq!(MAX_BATCH, 64);
    }
}
