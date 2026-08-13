//! memento-application — Memento RS application use cases (cluster G).
//!
//! The use-case layer sits between the surfaces (MCP/CLI, batches 8-9) and
//! the adapters. [`AppService`] orchestrates the concrete adapters
//! (LanceDB store, parse boundary, embedder, chunker) exactly as the design
//! data-flow prescribes:
//!
//! ```text
//! surface ──► AppService (use cases; ctx guard REQ-TA-005)
//!                │              │
//!                ▼              ▼
//!           memento-parse  memento-tenant (startup: token→TenantContext)
//!           chunker        memento-embed-fastembed (stub in tests)
//!                │              │
//!                ▼              ▼
//!            chunk (text-splitter)   memento-lancedb (chunks/docs/feedback)
//!                └──────────────► batch add (atomic visibility REQ-MC-007)
//! ```
//!
//! One [`AppService`] is bound to exactly one tenant (REQ-TA-001/002): it is
//! opened with a resolved [`TenantContext`] and every method re-validates the
//! passed context against the bound tenant (`TENANT_FORBIDDEN` on mismatch,
//! defense in depth). Workspaces are NOT bound: they are per-call parameters
//! (mandatory workspace filter, REQ-MR-006).
//!
//! Modules:
//! * [`audit`] — per-tenant JSONL audit log (REQ-CG-003; core lands T-060,
//!   the full event matrix + no-secrets scan land T-066).
//! * [`ingest`] — ingest_text / ingest_document with limits, tenant-scoped
//!   content-hash dedup (REQ-MC-005) and atomic batch visibility
//!   (REQ-MC-007) — T-060.
//! * [`search`], [`context_fit`] — retrieval use cases (REQ-MR-*) — T-061.
//! * [`feedback`], [`delete`] — feedback + hard delete (REQ-ML-001/002) —
//!   T-062.
//! * [`tenant_config`], [`sweep`] — per-tenant retention + sweep
//!   (REQ-ML-003) — T-063.
//! * [`erase`] — right-to-erase flow (REQ-CG-001, design D4) — T-064.
//! * [`backup`], [`export`] — backup/restore + GDPR export (REQ-ML-005,
//!   REQ-CG-005) — T-065.
//! * [`code`] — KnowledgePort facade with the REQ-TA-005 context guard —
//!   T-067.

pub mod audit;
pub mod backup;
pub mod code;
pub mod context_fit;
pub mod delete;
pub mod erase;
pub mod export;
pub mod feedback;
pub mod ingest;
pub mod search;
pub mod sweep;
pub mod tenant_config;

use crate::audit::AuditLogger;
use memento_domain::{DomainError, TenantContext, WorkspaceId};
use memento_lancedb::LanceStore;
use memento_parse::chunk::Chunker;
use memento_ports::{EmbedPort, KnowledgePort, ParsePort, RerankPort};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::Instrument;

/// Open a `search` span carrying the tenant/agent/workspace context and an
/// empty `chore_id` slot (REQ-OBS-003, design D4). Absent fields are omitted,
/// never faked: callers `record("chore_id", ...)` only when a chore id
/// exists; the slot serializes as nothing otherwise.
pub(crate) fn search_span(ctx: &TenantContext, workspace_id: WorkspaceId) -> tracing::Span {
    tracing::info_span!(
        "search",
        tenant_id = %ctx.tenant_id(),
        agent_id = %ctx.agent_id(),
        workspace_id = %workspace_id,
        chore_id = tracing::field::Empty,
    )
}

/// The same context span for `ingest` (chore-tracked: the span records the
/// generated chore id once it exists).
pub(crate) fn ingest_span(ctx: &TenantContext, workspace_id: WorkspaceId) -> tracing::Span {
    tracing::info_span!(
        "ingest",
        tenant_id = %ctx.tenant_id(),
        agent_id = %ctx.agent_id(),
        workspace_id = %workspace_id,
        chore_id = tracing::field::Empty,
    )
}

/// The same context span for `context_fit`.
pub(crate) fn context_fit_span(ctx: &TenantContext, workspace_id: WorkspaceId) -> tracing::Span {
    tracing::info_span!(
        "context_fit",
        tenant_id = %ctx.tenant_id(),
        agent_id = %ctx.agent_id(),
        workspace_id = %workspace_id,
        chore_id = tracing::field::Empty,
    )
}

/// Embedding model version stamped on every chunk (REQ-MC-004). Mirrors
/// `memento_embed_fastembed::model::MODEL_VERSION`; duplicated here because
/// the embedder is injected as a port and the schema must know the model.
/// Reflects the P2 int8 toggle (`MEMENTO_QUANTIZED_MODEL`) so chunks ingested
/// under the quantized model carry the matching label.
pub fn embedding_model_version() -> &'static str {
    if std::env::var_os("MEMENTO_QUANTIZED_MODEL").is_some() {
        "multilingual-e5-base-int8-v0.0.3"
    } else {
        "multilingual-e5-base-v0.0.3"
    }
}

/// Max document blob accepted by `ingest_document` (design MC Q4: 10 MB).
pub const MAX_BLOB_BYTES: u64 = 10 * 1024 * 1024;

/// Max chunks produced by one ingest (design MC Q4: 10k chunks/doc).
pub const MAX_CHUNKS_PER_DOC: usize = 10_000;

/// Embedding batch size (design step 5: batch 64; the fastembed adapter
/// errors on larger batches — RESOURCE_EXHAUSTED).
pub const EMBED_BATCH: usize = 64;

/// Max entries in the query embed cache (B1). Beyond this the whole cache is
/// cleared to bound memory; query strings repeat far less than chunks, so a
/// cleared cache repopulates quickly. Mirrors the chunk cache cap in
/// `memento_embed_fastembed::model` (edit C).
pub(crate) const MAX_QUERY_CACHE_ENTRIES: usize = 100_000;

/// Injectable clock (REQ-ML-003, design D5): application code computes "now"
/// through this trait so retention tests can advance time without sleeping.
pub trait Clock: Send + Sync {
    /// The current wall-clock instant.
    fn now(&self) -> chrono::DateTime<chrono::Utc>;
}

/// The production clock: `Utc::now()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
}

/// Bounded query-embedding cache (B1): exact text hash → vector. Repeated /
/// near-repeated query strings skip ONNX inference entirely (~0 ms on hit;
/// each query embedding costs ~100 ms today). Mirrors the chunk embed cache
/// in `memento_embed_fastembed::model` (edit C). Exact match only — no fuzzy
/// or prefix matching: deterministic and safe. On overflow the whole cache is
/// cleared to bound memory.
pub(crate) struct QueryEmbedCache {
    inner: Mutex<HashMap<u64, Vec<f32>>>,
    cap: usize,
}

impl QueryEmbedCache {
    /// A cache that clears itself once it exceeds `cap` entries.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            cap,
        }
    }

    /// Look up a cached vector by its exact-text hash.
    pub(crate) fn get(&self, key: u64) -> Result<Option<Vec<f32>>, DomainError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| DomainError::Internal {
                message: "query embed cache lock poisoned".into(),
            })?
            .get(&key)
            .cloned())
    }

    /// Insert a vector; clears the whole cache when over the cap.
    pub(crate) fn insert(&self, key: u64, vec: Vec<f32>) -> Result<(), DomainError> {
        let mut inner = self.inner.lock().map_err(|_| DomainError::Internal {
            message: "query embed cache lock poisoned".into(),
        })?;
        inner.insert(key, vec);
        if inner.len() > self.cap {
            inner.clear();
        }
        Ok(())
    }

    /// Current entry count (tests).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().expect("cache lock").len()
    }
}

/// The application use-case service, bound to one tenant (REQ-TA-001/002).
pub struct AppService {
    store: Arc<LanceStore>,
    parse: Arc<dyn ParsePort>,
    embedder: Option<Arc<dyn EmbedPort>>,
    /// Cross-encoder reranker (A1): an optional retrieval post-processor.
    /// `None` when no surface attached a reranker; even when attached, the
    /// capability gate (`MEMENTO_RERANK=1`) lives on the port itself.
    reranker: Option<Arc<dyn RerankPort>>,
    chunker: Arc<Chunker>,
    clock: Arc<dyn Clock>,
    audit: AuditLogger,
    root: PathBuf,
    /// Lazily-opened code facade (T-067): one `KnowledgePort` per bound
    /// tenant, created on first use.
    code: Mutex<Option<Arc<dyn KnowledgePort>>>,
    /// One-shot flag (B3 fix from obs 2663): the embedder has been warmed.
    /// `AppService::open` does the eager warm-up so the first user-facing
    /// `code.search` call is fast (the ONNX cold-start used to cost ~5 s).
    embed_warm: AtomicBool,
    /// Query embedding cache (B1): exact query text → vector, so repeated /
    /// near-repeated agent queries skip ONNX inference entirely (~0 ms on
    /// hit vs ~100 ms per query embedding today). Only the hybrid (RRF) path
    /// touches this.
    query_embed_cache: QueryEmbedCache,
}

impl std::fmt::Debug for AppService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Port objects have no Debug impl; log the identity-relevant fields.
        f.debug_struct("AppService")
            .field("root", &self.root)
            .field("tenant_id", self.store.tenant_id())
            .finish()
    }
}

impl AppService {
    /// Open the service bound to `ctx`'s tenant under `root` (D8 layout):
    /// opens + ensures the LanceDB schema, loads the deterministic Spanish
    /// chunker, and opens the tenant audit log.
    ///
    /// # Errors
    ///
    /// * `Internal` — the embedded Spanish tokenizer cannot be parsed.
    /// * `Io` — the store cannot be opened or the audit log cannot be
    ///   created.
    pub async fn open(
        ctx: &TenantContext,
        root: impl AsRef<Path>,
        parse: Arc<dyn ParsePort>,
        embedder: Option<Arc<dyn EmbedPort>>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DomainError> {
        // REQ-OBS-003: tenant-open runs inside a context span. There is no
        // workspace at open time — the field is omitted, never faked.
        let span = tracing::info_span!(
            "tenant_open",
            tenant_id = %ctx.tenant_id(),
            agent_id = %ctx.agent_id(),
            chore_id = tracing::field::Empty,
        );
        async {
            // REQ-OBS-006 (design D2): install the Prometheus recorder once
            // when MEMENTO_METRICS=1 so the hot-path macros below actually
            // record (they are no-ops without a recorder — zero work when the
            // var is unset). OnceLock makes this free on every later call.
            let _ = memento_observability::metrics::ensure_recorder();
            metrics::counter!(
                "memento_tenant_open_total",
                "tenant_id" => ctx.tenant_id().to_string()
            )
            .increment(1);
            let root = root.as_ref().to_path_buf();
            let store = LanceStore::open(ctx, &root).await?;
            store.ensure_schema().await?;
            let chunker = Chunker::embedded()?;
            let audit = AuditLogger::new(&root, ctx.tenant_id())?;
            let service = Self {
                store: Arc::new(store),
                parse,
                embedder,
                reranker: None,
                chunker: Arc::new(chunker),
                clock,
                audit,
                root,
                code: Mutex::new(None),
                embed_warm: AtomicBool::new(false),
                query_embed_cache: QueryEmbedCache::new(MAX_QUERY_CACHE_ENTRIES),
            };
            // B3 fix (obs 2663): pre-warm the embedder at tenant open so the
            // first user-facing `code.search` does NOT pay the ~5 s ONNX cold
            // start. Idempotent on every process (AtomicBool absorbs re-entry).
            // Under `--no-embeddings` (embedder is None) this is a no-op.
            // A warmup failure (broken model, missing file) is logged but does
            // NOT fail service open: the first user call will see the embedder
            // error with the same surface. This preserves REQ-MC-007's
            // structured-error invariant for ingest with a broken embedder.
            if let Err(err) = service.warm_embedder().await {
                tracing::warn!(
                    ?err,
                    "embedder pre-warm failed; first call will pay the cold-start cost"
                );
            }
            Ok(service)
        }
        .instrument(span)
        .await
    }

    /// Pre-warm the embedder (B3 fix from obs 2663).
    ///
    /// The first `embed()` call on a fresh process pays ~5 s to load the
    /// E5Base model, the ONNX runtime, and the tokenizer. Running one
    /// no-op embedding here moves that cost off the user-facing critical
    /// path. Idempotent and cheap on subsequent calls (AtomicBool fast
    /// path). On a broken embedder the error propagates so callers can
    /// surface it; `AppService::open` logs-and-continues so the service
    /// stays open for non-embedding flows (REQ-MC-007 invariant).
    pub async fn warm_embedder(&self) -> Result<(), DomainError> {
        // REQ-OBS-003: the pre-warm runs inside its own span. No agent is
        // bound at service level — the field is omitted, never faked.
        let span = tracing::info_span!(
            "pre_warm",
            tenant_id = %self.store.tenant_id(),
            chore_id = tracing::field::Empty,
        );
        async {
            if self.embed_warm.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(embedder) = &self.embedder {
                embedder.embed(&[""]).await?;
            }
            // REQ-OBS-006: the pre-warm operation counter (only when a warm
            // actually ran; the AtomicBool absorbs re-entry).
            metrics::counter!(
                "memento_pre_warm_total",
                "tenant_id" => self.store.tenant_id().to_string()
            )
            .increment(1);
            self.embed_warm.store(true, Ordering::Release);
            Ok(())
        }
        .instrument(span)
        .await
    }

    /// The storage root this service is bound to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Attach a cross-encoder reranker (A1). Surfaces that want the rerank
    /// capability call this right after `open`; the reranker is `None` by
    /// default so every other caller keeps the pre-rerank behavior. The
    /// per-query opt-in (`SearchQuery.rerank`) decides whether a search pays
    /// the inference cost; the port's own `is_enabled()` (`MEMENTO_RERANK`)
    /// is the capability gate.
    pub fn with_reranker(mut self, reranker: Arc<dyn RerankPort>) -> Self {
        self.reranker = Some(reranker);
        self
    }

    /// The bound store (adapter-level access for advanced use cases).
    pub fn store(&self) -> &LanceStore {
        &self.store
    }

    /// The bound tenant directory: `<root>/db/tenants/<tid>` (D8).
    pub fn tenant_dir(&self) -> PathBuf {
        self.root
            .join("db")
            .join("tenants")
            .join(self.store.tenant_id().to_string())
    }

    /// Tenant-scoped content hash (REQ-MC-005): `sha256(tenant_id ‖ NUL ‖
    /// content)`. The tenant id inside the hash input guarantees that two
    /// tenants can never collide even with identical content — cross-tenant
    /// dedup is impossible by construction.
    pub(crate) fn content_hash(&self, content: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.store.tenant_id().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(content);
        let digest = hasher.finalize();
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for byte in digest {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
        out
    }

    /// Embed `texts` in batches of ≤ [`EMBED_BATCH`]. Under `--no-embeddings`
    /// (embedder is `None`) every chunk carries an explicitly absent vector
    /// (REQ-MC-004) and the pipeline does not fail.
    pub(crate) async fn embed_batch(
        &self,
        texts: &[String],
    ) -> Result<Vec<Option<Vec<f32>>>, DomainError> {
        let Some(embedder) = &self.embedder else {
            return Ok(texts.iter().map(|_| None).collect());
        };
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(EMBED_BATCH) {
            let refs: Vec<&str> = batch.iter().map(String::as_str).collect();
            let vectors = embedder.embed(&refs).await?;
            out.extend(vectors.into_iter().map(Some));
        }
        Ok(out)
    }

    /// Record an audit event for the current operation (best-effort by
    /// design — audit sink failures never fail data operations; they are
    /// traced loudly inside [`AuditLogger::record`]).
    pub(crate) fn record_audit(
        &self,
        ctx: &TenantContext,
        action: &str,
        target: Value,
        chore_id: Option<memento_domain::ChoreId>,
    ) {
        self.audit.ok(ctx, action, target, chore_id);
    }

    /// Defense in depth (REQ-TA-005): every use case re-validates that the
    /// caller's context belongs to the process-bound tenant. The store's own
    /// `ensure_tenant` enforces the same rule on every table op; this guard
    /// fires BEFORE any work in the application layer (e.g. config reads
    /// that never touch the store).
    pub(crate) fn ensure_bound_tenant(&self, ctx: &TenantContext) -> Result<(), DomainError> {
        if ctx.tenant_id() == self.store.tenant_id() {
            Ok(())
        } else {
            Err(DomainError::TenantForbidden)
        }
    }

    /// Path of this tenant's audit JSONL (tests inspect it).
    #[cfg(test)]
    pub(crate) fn audit_log_path(&self) -> PathBuf {
        self.audit.log_path()
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use memento_domain::TenantContext;
    use memento_parse::ParseService;
    use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
    use memento_testkit::{StubEmbedPort, TempStore, TestClock};
    use std::sync::Arc;

    /// Serializes tests that mutate `MEMENTO_METRICS` (process-global env —
    /// same pattern as the tenant resolver's `ENV_LOCK`). Every REQ-OBS-006
    /// test must hold this guard while setting the env var.
    pub static METRICS_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Parse the value of the metric line with the exact `line_prefix`
    /// (e.g. `memento_ingest_chunks_total{tenant_id="..."}`). `None` when
    /// the family/labeled series was not recorded at all.
    pub(crate) fn metric_value(render: &str, line_prefix: &str) -> Option<u64> {
        render
            .lines()
            .find(|line| line.starts_with(line_prefix))?
            .rsplit_once(' ')?
            .1
            .parse()
            .ok()
    }

    /// The testkit clock is a `Clock` for the application (trait local to
    /// this crate, type from the dev-dependency — allowed by the orphan
    /// rule). This is the injectable-clock seam retention tests use.
    impl Clock for TestClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            self.now()
        }
    }

    /// A real parse boundary on the FALLBACK path (md/txt passthrough —
    /// no subprocess involved; the anydoc command below is never invoked).
    pub(crate) fn real_fallback_parse() -> Arc<dyn ParsePort> {
        Arc::new(ParseService::new(AnydocConfig {
            command: AnydocCommand {
                program: "never-invoked".into(),
                args: vec![],
                env: vec![],
            },
            timeout: std::time::Duration::from_secs(1),
            stdout_limit: 1024,
            staging_dir: std::env::temp_dir(),
        }))
    }

    /// A parse boundary that always fails with a structured `Parse` error
    /// (REQ-MC-007 mid-normalization failure tests).
    pub(crate) fn failing_parse() -> Arc<dyn ParsePort> {
        struct Failing;
        #[async_trait::async_trait]
        impl ParsePort for Failing {
            async fn parse(
                &self,
                _blob: &[u8],
                _hint: memento_domain::SourceKind,
            ) -> Result<memento_ports::ParsedDocument, DomainError> {
                Err(DomainError::Parse {
                    message: "normalization failed (fake)".into(),
                })
            }
        }
        Arc::new(Failing)
    }

    /// An embedder that always fails (REQ-MC-007 embed-stage failure tests).
    pub(crate) fn failing_embed() -> Arc<dyn EmbedPort> {
        struct Failing;
        #[async_trait::async_trait]
        impl EmbedPort for Failing {
            async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, DomainError> {
                Err(DomainError::EmbeddingFailed {
                    message: "model inference failed (fake)".into(),
                })
            }
        }
        Arc::new(Failing)
    }

    /// The standard test app: real LanceDB on a temp dir, deterministic
    /// stub embedder (768-d), real fallback parse boundary, fixed clock.
    pub(crate) async fn test_app(ts: &TempStore, clock: TestClock) -> AppService {
        AppService::open(
            &ts.ctx(),
            ts.root(),
            real_fallback_parse(),
            Some(Arc::new(StubEmbedPort::default())),
            Arc::new(clock),
        )
        .await
        .expect("test app opens")
    }

    /// An app with NO embedder (`--no-embeddings` mode, REQ-MC-004).
    pub(crate) async fn test_app_no_embed(ts: &TempStore) -> AppService {
        AppService::open(
            &ts.ctx(),
            ts.root(),
            real_fallback_parse(),
            None,
            Arc::new(SystemClock),
        )
        .await
        .expect("test app opens")
    }

    /// An app whose parse boundary always fails (atomicity tests).
    pub(crate) async fn test_app_failing_parse(ts: &TempStore) -> AppService {
        AppService::open(
            &ts.ctx(),
            ts.root(),
            failing_parse(),
            Some(Arc::new(StubEmbedPort::default())),
            Arc::new(SystemClock),
        )
        .await
        .expect("test app opens")
    }

    /// An app whose embedder always fails (atomicity tests).
    pub(crate) async fn test_app_failing_embed(ts: &TempStore) -> AppService {
        AppService::open(
            &ts.ctx(),
            ts.root(),
            real_fallback_parse(),
            Some(failing_embed()),
            Arc::new(SystemClock),
        )
        .await
        .expect("test app opens")
    }

    /// A context for a SECOND workspace of the same tenant (isolation
    /// matrix, REQ-MR-006): the store is tenant-bound, so per-call contexts
    /// may vary the workspace as long as the tenant matches.
    pub(crate) fn other_workspace_ctx(ts: &TempStore) -> TenantContext {
        memento_domain::TenantContext::new_for_tests(
            *ts.tenant_id(),
            memento_domain::WorkspaceId::new(),
            memento_domain::AgentId::new("test-agent"),
        )
    }
}

#[cfg(test)]
mod metrics_tests {
    use super::test_util::{METRICS_ENV_LOCK, test_app};
    use memento_testkit::{TempStore, TestClock};

    #[tokio::test]
    async fn metrics_tenant_open_and_pre_warm_recorded_when_enabled() {
        // REQ-OBS-006: with MEMENTO_METRICS=1 the tenant-open operation and
        // the embedder pre-warm each record a labeled counter (no-op when
        // the var is unset — the recorder is only installed while on).
        let _guard = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
        unsafe { std::env::set_var("MEMENTO_METRICS", "1") };

        let ts = TempStore::new();
        let clock = TestClock::default();
        let app = test_app(&ts, clock).await;
        let tenant = ts.tenant_id().to_string();

        let render = memento_observability::metrics::render();
        assert!(
            render.contains(&format!("memento_tenant_open_total{{tenant_id=\"{tenant}\"}} 1")),
            "tenant_open counter recorded: {render}"
        );
        assert!(
            render.contains(&format!("memento_pre_warm_total{{tenant_id=\"{tenant}\"}} 1")),
            "pre_warm counter recorded: {render}"
        );

        // SAFETY: test-only env mutation, serialized by METRICS_ENV_LOCK.
        unsafe { std::env::remove_var("MEMENTO_METRICS") };
        let _ = app.root(); // keep `app` alive for the assertions above
    }
}
