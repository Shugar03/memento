//! Cross-encoder reranker adapter (A1, item 3 of the optimization package).
//!
//! Wraps fastembed's `TextRerank` (`bge-reranker-v2-m3`, int8-quantized,
//! multilingual) behind a small [`RerankBackend`] trait so tests can inject
//! deterministic stubs (no ONNX load). [`Reranker`] owns the lazy,
//! single-flight initialization and the `MEMENTO_RERANK` capability toggle,
//! mirroring the embedder's [`crate::model::ModelLoader`]:
//!
//! * **Lazy**: the ~543 MB int8 model loads on the first `rerank()` call.
//! * **Single-flight**: concurrent first calls serialize on an init mutex —
//!   one initialization, everyone else observes its result (including its
//!   failure, which is cached and replayed, not retried).
//! * **Disabled**: `MEMENTO_RERANK` unset → [`Reranker::rerank`] returns
//!   equal scores (a passthrough that preserves the fused order). The
//!   application layer decides whether the per-query `rerank: true` opt-in
//!   pays the inference cost.
//!
//! [`FastReranker`] exposes the loader through the [`memento_ports::RerankPort`]
//! trait (async, `spawn_blocking` — the cross-encoder is CPU-bound).

use async_trait::async_trait;
use fastembed::{
    OnnxSource, RerankInitOptionsUserDefined, TextRerank, TokenizerFiles, UserDefinedRerankingModel,
};
use memento_domain::DomainError;
use memento_ports::RerankPort;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Model version label stamped on reranked hits / surfaced by the port.
pub const RERANK_MODEL_VERSION: &str = "bge-reranker-v2-m3-int8-v0.0.1";
/// Env var that enables the reranker capability. Without it the model is
/// never loaded and per-query `rerank: true` requests degrade to the fused
/// order with a warning (env = capability, query = opt-in).
const RERANK_MODEL_ENV: &str = "MEMENTO_RERANK";
/// Env var pointing at an int8-quantized reranker `model.onnx`. Overrides the
/// default path under the storage root; mirrors the embedder's
/// `MEMENTO_QUANTIZED_MODEL` override.
const RERANK_MODEL_PATH_ENV: &str = "MEMENTO_RERANK_MODEL";
/// Default path of the int8-quantized `bge-reranker-v2-m3` produced by the
/// quantize script, relative to the storage root (design D8: `models/`).
const DEFAULT_RERANK_PATH: &str = "models/int8/bge-reranker-v2-m3-int8/model.onnx";
/// Max tokens fed to the cross-encoder per (query, text) pair. Chunks are
/// 256-300 tokens, so the fastembed default (512) is plenty and keeps the
/// padded batch small.
pub const MAX_RERANK_LEN: usize = 512;
/// Candidates per rerank inference batch. The fused top-10 is a single batch
/// (~300 tokens each → one padded seq), so the whole call is one session run.
const RERANK_BATCH: usize = 32;

/// The rerank computation behind the loader (injectable for tests).
pub trait RerankBackend: Send + Sync {
    /// Score each text against the query; `scores[i]` lines up with the input
    /// order (higher = more relevant). Empty input → empty output.
    fn rerank(&self, query: &str, texts: &[&str]) -> Result<Vec<f32>, DomainError>;
    /// The model version label this backend produces scores for.
    fn model_version(&self) -> &'static str;
}

/// Real backend: fastembed `TextRerank` behind a mutex (`rerank` takes
/// `&mut self`; the adapter is shared across tasks).
pub struct FastRerankBackend {
    model: Mutex<TextRerank>,
    model_version: &'static str,
}

impl FastRerankBackend {
    /// Initialize the ONNX session from an int8-quantized user-defined
    /// `bge-reranker-v2-m3`: the onnx + tokenizer files are read from disk
    /// and committed from memory (same pattern as the embedder's
    /// `try_new_user_defined`). The tokenizer files must sit next to the onnx
    /// file (same directory).
    pub fn try_new(model_path: &Path) -> Result<Self, DomainError> {
        // Pre-load the vendored ONNX Runtime dylib before any fastembed call
        // reaches the ort C ABI. Idempotent (internal `OnceLock`).
        crate::dylib::ensure_loaded();
        let model_dir = model_path
            .parent()
            .ok_or_else(|| DomainError::RerankFailed {
                message: format!("rerank model has no parent dir: {}", model_path.display()),
            })?;
        let read_bytes = |name: &str| -> Result<Vec<u8>, DomainError> {
            std::fs::read(model_dir.join(name)).map_err(|err| DomainError::RerankFailed {
                message: format!("read {name} for rerank model: {err}"),
            })
        };
        let onnx_file = std::fs::read(model_path).map_err(|err| DomainError::RerankFailed {
            message: format!("read rerank onnx {}: {err}", model_path.display()),
        })?;
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: read_bytes("tokenizer.json")?,
            config_file: read_bytes("config.json")?,
            special_tokens_map_file: read_bytes("special_tokens_map.json")?,
            tokenizer_config_file: read_bytes("tokenizer_config.json")?,
        };
        let user_model =
            UserDefinedRerankingModel::new(OnnxSource::Memory(onnx_file), tokenizer_files);
        let options = RerankInitOptionsUserDefined::new().with_max_length(MAX_RERANK_LEN);
        let model = TextRerank::try_new_from_user_defined(user_model, options).map_err(|err| {
            DomainError::RerankFailed {
                message: format!("init fastembed rerank: {err}"),
            }
        })?;
        Ok(Self {
            model: Mutex::new(model),
            model_version: RERANK_MODEL_VERSION,
        })
    }
}

impl RerankBackend for FastRerankBackend {
    fn rerank(&self, query: &str, texts: &[&str]) -> Result<Vec<f32>, DomainError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut model = self.model.lock().map_err(|_| DomainError::Internal {
            message: "rerank model mutex poisoned".into(),
        })?;
        let owned: Vec<&str> = texts.to_vec();
        // `rerank` returns results sorted by score descending; re-align them
        // to the input order via the result.index.
        let results = model
            .rerank(query, owned, false, Some(RERANK_BATCH))
            .map_err(|err| DomainError::RerankFailed {
                message: err.to_string(),
            })?;
        let mut scores = vec![0.0; texts.len()];
        for result in results {
            scores[result.index] = result.score;
        }
        Ok(scores)
    }

    fn model_version(&self) -> &'static str {
        self.model_version
    }
}

type RerankFactory = Arc<dyn Fn() -> Result<Arc<dyn RerankBackend>, DomainError> + Send + Sync>;

/// Lazy, single-flight cross-encoder loader with an explicit disabled mode.
///
/// * **Lazy**: the model initializes on the first `rerank`/`backend` call.
/// * **Single-flight**: concurrent first calls serialize on an init mutex —
///   one initialization, everyone else observes its result (including its
///   failure, which is cached and replayed, not retried).
/// * **Disabled**: `MEMENTO_RERANK` unset → [`Self::rerank`] returns equal
///   scores (passthrough, preserves fused order) and never touches the model.
pub struct Reranker {
    cache_root: PathBuf,
    enabled: bool,
    factory: Option<RerankFactory>,
    backend: OnceLock<Arc<dyn RerankBackend>>,
    /// Serializes initialization (single-flight).
    init_lock: Mutex<()>,
    /// Cached initialization failure message (replayed, not retried).
    init_error: Mutex<Option<String>>,
}

impl Reranker {
    /// Create a loader. `cache_root` is the storage root (design D8: the
    /// model lives under `<root>/models/int8/...`, overrideable via
    /// `MEMENTO_RERANK_MODEL`). With `MEMENTO_RERANK` unset the loader is
    /// created but disabled — no model is ever loaded and [`Self::rerank`]
    /// passes through.
    pub fn new(cache_root: PathBuf) -> Self {
        let enabled = std::env::var_os(RERANK_MODEL_ENV).is_some();
        let model_path = std::env::var_os(RERANK_MODEL_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| cache_root.join(DEFAULT_RERANK_PATH));
        let factory_model_path = model_path.clone();
        let factory: RerankFactory = Arc::new(move || {
            let backend = FastRerankBackend::try_new(&factory_model_path)?;
            Ok(Arc::new(backend) as Arc<dyn RerankBackend>)
        });
        Self {
            cache_root,
            enabled,
            factory: Some(factory),
            backend: OnceLock::new(),
            init_lock: Mutex::new(()),
            init_error: Mutex::new(None),
        }
    }

    /// Test injection: a loader whose backend is pre-built (no ONNX load).
    /// `enabled` is forced on — the point of a pre-built backend is to be
    /// used.
    pub fn from_backend(backend: Arc<dyn RerankBackend>) -> Self {
        Self {
            cache_root: PathBuf::new(),
            enabled: true,
            factory: None,
            backend: OnceLock::from(backend),
            init_lock: Mutex::new(()),
            init_error: Mutex::new(None),
        }
    }

    /// Whether the reranker capability is enabled (`MEMENTO_RERANK=1`).
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The storage root this loader is bound to.
    pub fn cache_root(&self) -> &PathBuf {
        &self.cache_root
    }

    /// The model version, when enabled (None when the capability is off).
    pub fn model_version(&self) -> Option<&'static str> {
        self.enabled.then_some(RERANK_MODEL_VERSION)
    }

    /// Access the backend, initializing it exactly once on first use
    /// (single-flight; failures are cached and replayed).
    pub fn backend(&self) -> Result<&Arc<dyn RerankBackend>, DomainError> {
        if !self.enabled {
            return Err(DomainError::RerankFailed {
                message: "reranker disabled (set MEMENTO_RERANK=1 to enable)".into(),
            });
        }
        if let Some(backend) = self.backend.get() {
            return Ok(backend);
        }

        // Single-flight: only one caller runs the factory; the rest wait on
        // the mutex and then observe the result (success or cached error).
        let _guard = self.init_lock.lock().map_err(|_| DomainError::Internal {
            message: "rerank init lock poisoned".into(),
        })?;
        if let Some(backend) = self.backend.get() {
            return Ok(backend);
        }
        if let Some(message) = self.init_error.lock().expect("init error lock").clone() {
            return Err(DomainError::RerankFailed { message });
        }

        let factory = self.factory.as_ref().ok_or_else(|| DomainError::Internal {
            message: "reranker has no backend factory".into(),
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
                Err(DomainError::RerankFailed { message })
            }
        }
    }

    /// Score texts against the query. When disabled, returns equal scores
    /// (a passthrough that preserves the fused order) — the application
    /// layer logs the warning for per-query opt-ins under a disabled
    /// capability. When enabled, the model initializes lazily on first call.
    pub fn rerank(&self, query: &str, texts: &[&str]) -> Result<Vec<f32>, DomainError> {
        if !self.enabled {
            return Ok(vec![1.0; texts.len()]);
        }
        let backend = self.backend()?;
        backend.rerank(query, texts)
    }
}

/// [`RerankPort`] adapter over the fastembed reranker loader: inputs are
/// pushed onto the blocking pool (ONNX inference is CPU-bound).
pub struct FastReranker {
    loader: Arc<Reranker>,
}

impl FastReranker {
    pub fn new(loader: Arc<Reranker>) -> Self {
        Self { loader }
    }

    /// The underlying lazy loader (tests inspect the capability flag).
    pub fn loader(&self) -> &Reranker {
        &self.loader
    }
}

#[async_trait]
impl RerankPort for FastReranker {
    async fn rerank(&self, query: &str, texts: &[&str]) -> Result<Vec<f32>, DomainError> {
        // Defense in depth: disabled → passthrough, never a hard error.
        if !self.loader.is_enabled() {
            return Ok(texts.iter().map(|_| 1.0).collect());
        }
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let backend = self.loader.backend()?.clone();
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        let q = query.to_string();

        match tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            backend.rerank(&q, &refs)
        })
        .await
        {
            Ok(result) => result,
            Err(err) => Err(DomainError::Internal {
                message: format!("rerank task failed: {err}"),
            }),
        }
    }

    fn is_enabled(&self) -> bool {
        self.loader.is_enabled()
    }

    fn model_version(&self) -> Option<&'static str> {
        self.loader.model_version()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stub backend with scripted scores (aligned to input order).
    struct ScriptedBackend {
        scores: Vec<f32>,
    }

    impl RerankBackend for ScriptedBackend {
        fn rerank(&self, _query: &str, texts: &[&str]) -> Result<Vec<f32>, DomainError> {
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| self.scores[i])
                .collect())
        }

        fn model_version(&self) -> &'static str {
            RERANK_MODEL_VERSION
        }
    }

    struct CountingBackend;

    impl RerankBackend for CountingBackend {
        fn rerank(&self, _query: &str, texts: &[&str]) -> Result<Vec<f32>, DomainError> {
            Ok((0..texts.len()).map(|i| i as f32).collect())
        }

        fn model_version(&self) -> &'static str {
            RERANK_MODEL_VERSION
        }
    }

    struct FailingBackend;

    impl RerankBackend for FailingBackend {
        fn rerank(&self, _query: &str, _texts: &[&str]) -> Result<Vec<f32>, DomainError> {
            Err(DomainError::RerankFailed {
                message: "onnx exploded".into(),
            })
        }

        fn model_version(&self) -> &'static str {
            RERANK_MODEL_VERSION
        }
    }

    /// Serializes tests that mutate `MEMENTO_RERANK` (process-global env —
    /// same pattern as the tenant resolver's `ENV_LOCK` and the embedder's
    /// `QUANTIZED_ENV_LOCK`).
    static RERANK_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn disabled_reranker_passthrough() {
        let _guard = RERANK_ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var(RERANK_MODEL_ENV);
        }
        let loader = Reranker::new(PathBuf::from("nope"));
        assert!(!loader.is_enabled());
        assert_eq!(loader.model_version(), None);

        let scores = loader
            .rerank("q", &["a", "b", "c"])
            .expect("disabled rerank is not an error");
        assert_eq!(scores, vec![1.0, 1.0, 1.0], "passthrough, equal scores");
        // The backend must never be constructed in disabled mode.
        assert!(loader.backend().is_err(), "no backend in disabled mode");
    }

    #[test]
    fn enabled_requires_env() {
        let _guard = RERANK_ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(RERANK_MODEL_ENV, "1");
        }
        let loader = Reranker::new(PathBuf::from("nope"));
        assert!(loader.is_enabled());
        assert_eq!(loader.model_version(), Some(RERANK_MODEL_VERSION));
    }

    #[test]
    fn backend_initializes_once_and_is_shared() {
        let inits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&inits);
        let factory: RerankFactory = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(CountingBackend) as Arc<dyn RerankBackend>)
        });

        let loader = Reranker {
            cache_root: PathBuf::new(),
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

        let scores = loader.rerank("q", &["uno", "dos"]).expect("rerank enabled");
        assert_eq!(scores.len(), 2);
    }

    #[test]
    fn concurrent_backend_access_is_single_flight() {
        let inits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&inits);
        let factory: RerankFactory = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(CountingBackend) as Arc<dyn RerankBackend>)
        });

        let loader = Arc::new(Reranker {
            cache_root: PathBuf::new(),
            enabled: true,
            factory: Some(factory),
            backend: OnceLock::new(),
            init_lock: Mutex::new(()),
            init_error: Mutex::new(None),
        });

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let loader = Arc::clone(&loader);
                std::thread::spawn(move || loader.backend().is_ok())
            })
            .collect();
        for handle in handles {
            assert!(handle.join().expect("thread"), "backend must init");
        }
        assert_eq!(inits.load(Ordering::SeqCst), 1, "single-flight init");
    }

    #[test]
    fn init_error_cached_not_retried() {
        let inits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&inits);
        let factory: RerankFactory = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(DomainError::RerankFailed {
                message: "model missing".into(),
            })
        });

        let loader = Reranker {
            cache_root: PathBuf::new(),
            enabled: true,
            factory: Some(factory),
            backend: OnceLock::new(),
            init_lock: Mutex::new(()),
            init_error: Mutex::new(None),
        };

        let err1 = loader.backend().err().expect("first init fails");
        let err2 = loader.backend().err().expect("cached error");
        assert_eq!(err1.code(), memento_domain::error::CODE_RERANK_FAILED);
        assert_eq!(err1.to_string(), err2.to_string(), "same error replayed");
        assert_eq!(inits.load(Ordering::SeqCst), 1, "never retried");
    }

    #[test]
    fn rerank_scores_are_ordered() {
        let loader = Reranker::from_backend(Arc::new(ScriptedBackend {
            scores: vec![0.2, 0.9, 0.5],
        }) as Arc<dyn RerankBackend>);
        let scores = loader
            .rerank("q", &["a", "b", "c"])
            .expect("rerank with stub");
        assert_eq!(scores, vec![0.2, 0.9, 0.5], "scores align to input order");
    }

    #[tokio::test]
    async fn port_disabled_returns_passthrough() {
        // Drop the env lock before any await (never hold a MutexGuard across
        // an await point).
        {
            let _guard = RERANK_ENV_LOCK.lock().unwrap();
            unsafe {
                std::env::remove_var(RERANK_MODEL_ENV);
            }
        }
        let port = FastReranker::new(Arc::new(Reranker::new(PathBuf::from("nope"))));
        assert!(!port.is_enabled());
        assert_eq!(port.model_version(), None);
        let scores = port
            .rerank("q", &["a", "b"])
            .await
            .expect("disabled rerank is not an error");
        assert_eq!(scores, vec![1.0, 1.0]);
    }

    #[tokio::test]
    async fn port_scores_via_spawn_blocking() {
        let loader = Reranker::from_backend(Arc::new(ScriptedBackend {
            scores: vec![0.7, 0.1],
        }) as Arc<dyn RerankBackend>);
        let port = FastReranker::new(Arc::new(loader));
        let scores = port
            .rerank("q", &["a", "b"])
            .await
            .expect("rerank through port");
        assert_eq!(scores, vec![0.7, 0.1]);
    }

    #[tokio::test]
    async fn port_backend_failure_propagates_structured() {
        let loader = Reranker::from_backend(Arc::new(FailingBackend) as Arc<dyn RerankBackend>);
        let port = FastReranker::new(Arc::new(loader));
        let err = port.rerank("q", &["a"]).await.expect_err("failure");
        assert_eq!(err.code(), memento_domain::error::CODE_RERANK_FAILED);
        assert!(err.to_string().contains("onnx exploded"));
    }

    #[test]
    fn empty_input_is_ok() {
        let loader = Reranker::from_backend(Arc::new(CountingBackend) as Arc<dyn RerankBackend>);
        let scores = loader.rerank("q", &[]).expect("empty rerank");
        assert!(scores.is_empty());
    }

    #[test]
    fn model_version_contract() {
        assert_eq!(RERANK_MODEL_VERSION, "bge-reranker-v2-m3-int8-v0.0.1");
        assert_eq!(MAX_RERANK_LEN, 512);
    }
}
