//! Embedding model loader (T-023).
//!
//! Wraps fastembed's `TextEmbedding` (MultilingualE5Base, 768 dims) behind a
//! small [`EmbeddingBackend`] trait so tests can inject deterministic stubs
//! (no ONNX download, per testing-capabilities). [`ModelLoader`] owns the
//! lazy, single-flight initialization and the `--no-embeddings` mode: when
//! embeddings are disabled the loader returns `Ok(None)` — explicit absent
//! vectors (REQ-MC-004) — instead of failing.

use fastembed::{
    EmbeddingModel, InitOptionsUserDefined, Pooling, TextEmbedding, TextInitOptions,
    TokenizerFiles, UserDefinedEmbeddingModel,
};
use memento_domain::DomainError;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Model version stamped on every chunk (REQ-MC-006) and checked on
/// retrieval (EMBEDDING_MODEL_MISMATCH surface).
pub const MODEL_VERSION: &str = "multilingual-e5-base-v0.0.3";
/// Version label when the int8-quantized user-defined model is active.
pub const MODEL_VERSION_QUANTIZED: &str = "multilingual-e5-base-int8-v0.0.3";
/// Env var pointing at an int8-quantized `model.onnx` (P2 quantize). Overrides
/// the default int8 model path; when unset, the loader uses the default int8
/// model at `models/int8/...` (see DEFAULT_QUANTIZED_PATH).
const QUANTIZED_MODEL_ENV: &str = "MEMENTO_QUANTIZED_MODEL";
/// Env var that forces the stock FP32 `MultilingualE5Base` download (opt-out
/// from the int8 default, e.g. for maximum quality or when the int8 model is
/// not provisioned on the host).
const FP32_MODEL_ENV: &str = "MEMENTO_FP32_MODEL";
/// Default path of the int8-quantized model produced by the quantize script.
const DEFAULT_QUANTIZED_PATH: &str = "models/int8/multilingual-e5-base-int8/model.onnx";

/// The version label for the active model mode (env toggle aware). Called at
/// loader construction so label and backend stay consistent. Returns the int8
/// label by default, falling back to the FP32 label when the int8 file is
/// absent or the FP32 opt-out is set.
fn active_model_version() -> &'static str {
    if std::env::var_os(FP32_MODEL_ENV).is_some() {
        return MODEL_VERSION;
    }
    let model_path = std::env::var_os(QUANTIZED_MODEL_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_QUANTIZED_PATH));
    if model_path.is_file() {
        MODEL_VERSION_QUANTIZED
    } else {
        MODEL_VERSION
    }
}
/// Embedding dimension (must match the lancedb chunks schema).
pub const EMBEDDING_DIM: usize = 768;
/// Texts per inference batch (fastembed internal batching, T-024 boundary).
pub const MAX_BATCH: usize = 8;
/// Max tokens fed to the model per text. Chunks are 256-300 tokens, so the
/// fastembed default (512) wastes ~40% of compute on padding/truncation.
pub const MAX_EMBED_LEN: usize = 320;
/// Max entries in the embed cache (hash→vector). Beyond this we clear to
/// bound memory; re-ingest/dedup workloads stay small in practice.
pub const MAX_CACHE_ENTRIES: usize = 100_000;

/// Fast content hash for the embed cache. DefaultHasher (SipHash) is
/// deterministic within a process run — exactly what the per-process cache
/// needs; no new dependency.
fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The int8 file is missing → fall back to stock FP32: warn (visible through
/// the CLI subscriber) + record the fallback counter (REQ-OBS-006, design
/// D3). The `metrics` macro is a no-op without a recorder, so this costs
/// nothing while `MEMENTO_METRICS` is off.
fn record_fp32_fallback(model_path: &Path) {
    tracing::warn!(
        "int8 model not found at {}; falling back to stock FP32 (run the quantize script to provision it)",
        model_path.display()
    );
    metrics::counter!("memento_embed_fallback_fp32_total").increment(1);
}

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
    model_version: &'static str,
}

impl FastEmbedBackend {
    /// Initialize the ONNX session. Model files are cached under `cache_dir`
    /// (design D8: `models/`); the first call downloads them (documented,
    /// avoidable with `--no-embeddings` — REQ-CG-004).
    ///
    /// When `MEMENTO_QUANTIZED_MODEL` is set, the int8-quantized user-defined
    /// model is used instead (P2): the onnx + tokenizer files are read from
    /// disk and committed from memory. The tokenizer files must sit next to
    /// the onnx file (same directory). Dim and graph I/O are identical to the
    /// stock `MultilingualE5Base`; only the weights are int8.
    pub fn try_new(cache_dir: PathBuf) -> Result<Self, DomainError> {
        // Pre-load the vendored ONNX Runtime dylib before any fastembed
        // call reaches the ort C ABI. See `dylib.rs` for the full
        // rationale (System32's 1.17.x is rejected by ort 2.0.0-rc.13
        // which is compiled against the 1.28 API). Idempotent: the
        // internal `OnceLock` absorbs concurrent first-callers.
        crate::dylib::ensure_loaded();
        // int8 is the default model (validated: same retrieval quality, -43%
        // RAM, 4x smaller weights). Opt out to the stock FP32 download with
        // MEMENTO_FP32_MODEL, or override the int8 path with
        // MEMENTO_QUANTIZED_MODEL. If the int8 file is missing (fresh clone
        // without the quantized model), fall back to FP32 gracefully.
        if std::env::var_os(FP32_MODEL_ENV).is_none() {
            let model_path = std::env::var_os(QUANTIZED_MODEL_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_QUANTIZED_PATH));
            if model_path.is_file() {
                let model = Self::try_new_user_defined(&model_path)?;
                return Ok(Self {
                    model: Mutex::new(model),
                    model_version: MODEL_VERSION_QUANTIZED,
                });
            }
            record_fp32_fallback(&model_path);
        }
        let options = TextInitOptions::new(EmbeddingModel::MultilingualE5Base)
            .with_cache_dir(cache_dir)
            .with_show_download_progress(false)
            .with_max_length(MAX_EMBED_LEN);
        let model =
            TextEmbedding::try_new(options).map_err(|err| DomainError::EmbeddingFailed {
                message: format!("init fastembed: {err}"),
            })?;
        Ok(Self {
            model: Mutex::new(model),
            model_version: MODEL_VERSION,
        })
    }

    /// Build a fastembed session from a user-defined (int8-quantized) ONNX
    /// plus the matching tokenizer files. Uses Mean pooling — the E5 family's
    /// pooling — and caps the tokenizer at [`MAX_EMBED_LEN`] so chunk sizing
    /// behavior matches the stock model (A-package).
    fn try_new_user_defined(model_path: &Path) -> Result<TextEmbedding, DomainError> {
        let model_dir = model_path
            .parent()
            .ok_or_else(|| DomainError::EmbeddingFailed {
                message: format!(
                    "MEMENTO_QUANTIZED_MODEL has no parent dir: {}",
                    model_path.display()
                ),
            })?;
        let read_bytes = |name: &str| -> Result<Vec<u8>, DomainError> {
            std::fs::read(model_dir.join(name)).map_err(|err| DomainError::EmbeddingFailed {
                message: format!("read {name} for user-defined model: {err}"),
            })
        };
        let onnx_file = std::fs::read(model_path).map_err(|err| DomainError::EmbeddingFailed {
            message: format!("read quantized onnx {}: {err}", model_path.display()),
        })?;
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: read_bytes("tokenizer.json")?,
            config_file: read_bytes("config.json")?,
            special_tokens_map_file: read_bytes("special_tokens_map.json")?,
            tokenizer_config_file: read_bytes("tokenizer_config.json")?,
        };
        let user_model =
            UserDefinedEmbeddingModel::new(onnx_file, tokenizer_files).with_pooling(Pooling::Mean);
        let options = InitOptionsUserDefined::new().with_max_length(MAX_EMBED_LEN);
        TextEmbedding::try_new_from_user_defined(user_model, options).map_err(|err| {
            DomainError::EmbeddingFailed {
                message: format!("init fastembed user-defined: {err}"),
            }
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
        self.model_version
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
    /// Embedding cache by text hash: skips repeated inference when the same
    /// text is re-embedded (re-ingest, dedup) — zero compute for cache hits.
    cache: Mutex<HashMap<u64, Vec<f32>>>,
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
            cache: Mutex::new(HashMap::new()),
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
            cache: Mutex::new(HashMap::new()),
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
    /// Reflects the active model mode (quantized env toggle vs stock).
    pub fn model_version(&self) -> Option<&'static str> {
        self.enabled.then_some(active_model_version())
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

        // Cache hit path: identical text → identical vector (deterministic
        // embedder). Avoids repeated ONNX inference on re-ingest/dedup.
        let mut result: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        let mut misses: Vec<(usize, &str)> = Vec::new();
        {
            let cache = self.cache.lock().map_err(|_| DomainError::Internal {
                message: "embedding cache lock poisoned".into(),
            })?;
            for (idx, text) in texts.iter().enumerate() {
                let key = hash_text(text);
                if let Some(vec) = cache.get(&key) {
                    // REQ-OBS-006 "cache hit (embed cache)": the text was
                    // embedded before — zero inference, counter only. The
                    // adapter has no tenant, so the counter is unlabeled
                    // (documented scope note, design D5).
                    metrics::counter!("memento_embed_cache_hits_total").increment(1);
                    result[idx] = Some(vec.clone());
                } else {
                    metrics::counter!("memento_embed_cache_misses_total").increment(1);
                    misses.push((idx, text));
                }
            }
        }

        if !misses.is_empty() {
            let miss_texts: Vec<&str> = misses.iter().map(|(_, t)| *t).collect();
            let embedded = backend.embed_batch(&miss_texts)?;
            let mut cache = self.cache.lock().map_err(|_| DomainError::Internal {
                message: "embedding cache lock poisoned".into(),
            })?;
            for ((idx, text), vec) in misses.iter().zip(embedded.iter()) {
                let key = hash_text(text);
                cache.insert(key, vec.clone());
                result[*idx] = Some(vec.clone());
            }
            if cache.len() > MAX_CACHE_ENTRIES {
                cache.clear();
            }
        }

        Ok(Some(
            result
                .into_iter()
                .map(|v| v.expect("every slot filled by hit or miss"))
                .collect(),
        ))
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

    struct CacheProbeBackend {
        calls: AtomicUsize,
    }

    impl CacheProbeBackend {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl EmbeddingBackend for CacheProbeBackend {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(texts
                .iter()
                .map(|t| memento_testkit::deterministic_embed(t, EMBEDDING_DIM))
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
            cache: Mutex::new(HashMap::new()),
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
            cache: Mutex::new(HashMap::new()),
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
    fn embedding_cache_skips_duplicate_inference() {
        let backend = Arc::new(CacheProbeBackend::new());
        let loader =
            ModelLoader::from_backend(PathBuf::new(), backend.clone() as Arc<dyn EmbeddingBackend>);

        let out1 = loader
            .embed(&["hola mundo", "adiós mundo"])
            .unwrap()
            .unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);

        let out2 = loader
            .embed(&["hola mundo", "adiós mundo"])
            .unwrap()
            .unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(out1, out2, "identical texts return identical vectors");

        let out3 = loader
            .embed(&["hola mundo", "texto nuevo"])
            .unwrap()
            .unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(out3[0], out1[0], "cached vector reused");
    }

    #[test]
    fn model_version_contract() {
        assert_eq!(MODEL_VERSION, "multilingual-e5-base-v0.0.3");
        assert_eq!(MODEL_VERSION_QUANTIZED, "multilingual-e5-base-int8-v0.0.3");
        assert_eq!(EMBEDDING_DIM, 768);
        assert_eq!(MAX_BATCH, 8);
        assert_eq!(MAX_EMBED_LEN, 320);
    }

    /// Serializes tests that mutate `MEMENTO_QUANTIZED_MODEL` (process-global
    /// env — same pattern as the tenant resolver's `ENV_LOCK`).
    static QUANTIZED_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn active_model_version_defaults_to_stock() {
        let _guard = QUANTIZED_ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var(QUANTIZED_MODEL_ENV);
        }
        assert_eq!(active_model_version(), MODEL_VERSION);
    }

    #[test]
    fn active_model_version_switches_to_quantized_label() {
        let _guard = QUANTIZED_ENV_LOCK.lock().unwrap();
        // Point at a real file so the `is_file()` gate passes (default behavior
        // is int8-first with FP32 fallback when the onnx is missing).
        let dir = std::env::temp_dir().join("memento-int8-label-test");
        std::fs::create_dir_all(&dir).unwrap();
        let model_path = dir.join("model.onnx");
        std::fs::write(&model_path, b"fake").unwrap();
        unsafe {
            std::env::set_var(QUANTIZED_MODEL_ENV, &model_path);
        }
        assert_eq!(active_model_version(), MODEL_VERSION_QUANTIZED);
    }

    #[test]
    fn active_model_version_falls_back_when_quantized_path_missing() {
        let _guard = QUANTIZED_ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(QUANTIZED_MODEL_ENV, "C:\\nonexistent\\int8\\model.onnx");
        }
        assert_eq!(active_model_version(), MODEL_VERSION);
    }

    /// Drop guard that removes a transient file tree (divergence test below
    /// creates the DEFAULT int8 path under the crate CWD; `models/` is
    /// gitignored, so this only ever touches local disk).
    struct CleanupGuard(PathBuf);

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = self.0.parent().map(|p| std::fs::remove_dir(p));
        }
    }

    #[test]
    fn active_model_version_returns_int8_when_default_model_present_without_env() {
        // REQ-OBS-012 divergence case 1 (loader side): int8 file PRESENT at
        // the default path and MEMENTO_QUANTIZED_MODEL unset → the loaded
        // model IS int8, so the label must be int8. This is the truth the
        // application will stamp (S3.6); today the app's env-only check
        // wrongly says FP32 in this exact case.
        let _guard = QUANTIZED_ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var(QUANTIZED_MODEL_ENV);
            std::env::remove_var(FP32_MODEL_ENV);
        }
        let path = PathBuf::from(DEFAULT_QUANTIZED_PATH);
        std::fs::create_dir_all(path.parent().expect("default dir")).unwrap();
        std::fs::write(&path, b"fake-int8").unwrap();
        let _cleanup = CleanupGuard(path.clone());

        assert_eq!(
            active_model_version(),
            MODEL_VERSION_QUANTIZED,
            "default int8 file present → int8 label (the loader truth)"
        );
    }

    #[test]
    fn active_model_version_forces_stock_label_on_fp32_opt_out() {
        // REQ-OBS-012: MEMENTO_FP32_MODEL is the explicit opt-out — even with
        // a valid int8 file on disk, the label is the stock FP32 one (the
        // user asked for FP32, so FP32 is the truth). No fallback event/counter
        // in this case: the divergence detection (S3.6) keys off FP32 label
        // WITHOUT the opt-out.
        let _guard = QUANTIZED_ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(FP32_MODEL_ENV, "1");
        }
        let dir = std::env::temp_dir().join("memento-int8-fp32-optout-test");
        std::fs::create_dir_all(&dir).unwrap();
        let model_path = dir.join("model.onnx");
        std::fs::write(&model_path, b"fake").unwrap();
        unsafe {
            std::env::set_var(QUANTIZED_MODEL_ENV, &model_path);
        }

        assert_eq!(
            active_model_version(),
            MODEL_VERSION,
            "FP32 opt-out wins over a present int8 file"
        );
        // SAFETY: restore the env (process-global) so parallel tests under
        // QUANTIZED_ENV_LOCK never observe this test's mutation.
        unsafe {
            std::env::remove_var(FP32_MODEL_ENV);
            std::env::remove_var(QUANTIZED_MODEL_ENV);
        }
    }

    /// Serializes tests that mutate `MEMENTO_METRICS` (process-global env).
    static METRICS_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn fp32_fallback_records_fallback_counter() {
        // REQ-OBS-006 (design D3): the FP32 fallback site records the
        // fallback counter, visible in the registry when MEMENTO_METRICS=1.
        let _guard = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
        unsafe { std::env::set_var("MEMENTO_METRICS", "1") };
        // Install the recorder before the side effect runs (no-op gate).
        let _ = memento_observability::metrics::ensure_recorder();

        record_fp32_fallback(&PathBuf::from("C:\\missing\\int8\\model.onnx"));
        record_fp32_fallback(&PathBuf::from("C:\\missing\\int8\\model.onnx"));
        let render = memento_observability::metrics::render();
        assert!(
            render.contains("memento_embed_fallback_fp32_total 2"),
            "fallback counter accumulates per fallback: {render}"
        );
        // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
        unsafe { std::env::remove_var("MEMENTO_METRICS") };
    }
}
