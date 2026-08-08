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
use memento_domain::{DomainError, TenantContext};
use memento_lancedb::LanceStore;
use memento_parse::chunk::Chunker;
use memento_ports::{EmbedPort, KnowledgePort, ParsePort};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Embedding model version stamped on every chunk (REQ-MC-004). Mirrors
/// `memento_embed_fastembed::model::MODEL_VERSION`; duplicated here because
/// the embedder is injected as a port and the schema must know the model.
pub const EMBEDDING_MODEL_VERSION: &str = "multilingual-e5-small-v0.0.3";

/// Max document blob accepted by `ingest_document` (design MC Q4: 10 MB).
pub const MAX_BLOB_BYTES: u64 = 10 * 1024 * 1024;

/// Max chunks produced by one ingest (design MC Q4: 10k chunks/doc).
pub const MAX_CHUNKS_PER_DOC: usize = 10_000;

/// Embedding batch size (design step 5: batch 64; the fastembed adapter
/// errors on larger batches — RESOURCE_EXHAUSTED).
pub const EMBED_BATCH: usize = 64;

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

/// The application use-case service, bound to one tenant (REQ-TA-001/002).
pub struct AppService {
    store: Arc<LanceStore>,
    parse: Arc<dyn ParsePort>,
    embedder: Option<Arc<dyn EmbedPort>>,
    chunker: Arc<Chunker>,
    clock: Arc<dyn Clock>,
    audit: AuditLogger,
    root: PathBuf,
    /// Lazily-opened code facade (T-067): one `KnowledgePort` per bound
    /// tenant, created on first use.
    code: Mutex<Option<Arc<dyn KnowledgePort>>>,
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
        let root = root.as_ref().to_path_buf();
        let store = LanceStore::open(ctx, &root).await?;
        store.ensure_schema().await?;
        let chunker = Chunker::embedded()?;
        let audit = AuditLogger::new(&root, ctx.tenant_id())?;
        Ok(Self {
            store: Arc::new(store),
            parse,
            embedder,
            chunker: Arc::new(chunker),
            clock,
            audit,
            root,
            code: Mutex::new(None),
        })
    }

    /// The storage root this service is bound to.
    pub fn root(&self) -> &Path {
        &self.root
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
    /// stub embedder (384-d), real fallback parse boundary, fixed clock.
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
