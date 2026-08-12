//! CLI startup: resolve the process-bound tenant and open the application
//! layer (REQ-TA-002/003, same identity flow as the MCP server).
//!
//! One deviation from the MCP startup (documented): if the anydoc converter
//! cannot be resolved on this host, the CLI still starts — the fallback
//! parser keeps md/txt and ingest_text working, and document formats that
//! need the subprocess fail with a structured bilingual error at call time.
//! The MCP server fails hard because it cannot serve documents at all
//! without anydoc; the CLI degrades per-command instead of bricking
//! `stats`/`health`/`search` on a host without Node.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memento_application::{AppService, SystemClock};
use memento_domain::{DomainError, TenantContext};
use memento_embed_fastembed::{FastEmbedEmbedder, FastReranker, ModelLoader, Reranker};
use memento_parse::ParseService;
use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
use memento_ports::{EmbedPort, ParsePort, RerankPort};
use memento_tenant::TenantResolverImpl;

/// Everything a CLI command needs after startup.
pub struct CliApp {
    /// The application use-case layer (batch 7) — every command delegates.
    pub app: Arc<AppService>,
    /// The process-bound tenant context (REQ-TA-001/002).
    pub ctx: TenantContext,
    /// Storage root (D8 layout).
    pub root: PathBuf,
    /// `--no-embeddings` mode (REQ-MC-004).
    pub no_embeddings: bool,
    /// The embedder (shared by the code index's semantic sidecar).
    embedder: Option<Arc<dyn EmbedPort>>,
}

impl CliApp {
    /// The embedder, when embeddings are enabled (code index semantic
    /// sidecar; `--no-embeddings` → literal-only, REQ-CK-008).
    pub fn embedder(&self) -> Option<Arc<dyn EmbedPort>> {
        self.embedder.clone()
    }
}

/// The production embedder for this root (lazy model load — nothing is
/// downloaded at startup; first embed triggers the single-flight load).
pub fn embedder_for(root: &Path, no_embeddings: bool) -> Option<Arc<dyn EmbedPort>> {
    if no_embeddings {
        None
    } else {
        Some(Arc::new(FastEmbedEmbedder::new(Arc::new(
            ModelLoader::new(root.join("models"), true),
        ))))
    }
}

/// The cross-encoder reranker for this root (A1): lazy model load behind the
/// `MEMENTO_RERANK` capability toggle — the loader itself is created always
/// (cheap), the ~543 MB int8 model only loads on the first rerank call and
/// only when the env toggle is set.
pub fn reranker_for(root: &Path) -> Arc<dyn RerankPort> {
    Arc::new(FastReranker::new(Arc::new(Reranker::new(
        root.to_path_buf(),
    ))))
}

/// The parse boundary: `ParseService::auto` when anydoc resolves, else a
/// degraded service whose subprocess commands fail structurally per call
/// (fallback md/txt + ingest_text keep working).
fn parse_boundary(root: &Path) -> Arc<dyn ParsePort> {
    match ParseService::auto(root.join("tmp")) {
        Ok(service) => Arc::new(service),
        Err(err) => {
            tracing::warn!(%err, "anydoc unavailable; document conversion fails per-call (fallback md/txt works)");
            Arc::new(ParseService::new(AnydocConfig {
                command: AnydocCommand {
                    program: "anydoc-unavailable".into(),
                    args: vec![],
                    env: vec![],
                },
                timeout: memento_parse::anydoc::DEFAULT_TIMEOUT,
                stdout_limit: memento_parse::anydoc::DEFAULT_STDOUT_LIMIT,
                staging_dir: root.join("tmp"),
            }))
        }
    }
}

/// Resolve the bound context from `MEMENTO_TOKEN` + `MEMENTO_AGENT_ID` and
/// open the application layer (REQ-MS-003 semantics: nothing is served
/// without valid credentials — REQ-CL-005 scenario).
///
/// # Errors
///
/// * `AuthFailed` — missing/invalid `MEMENTO_TOKEN` (uniform, REQ-TA-006).
/// * `InvalidInput` — missing `MEMENTO_AGENT_ID` (REQ-TA-003).
/// * `Io` — the store cannot be opened.
pub async fn open(root: &Path, no_embeddings: bool) -> Result<CliApp, DomainError> {
    let resolver = TenantResolverImpl::open(root);
    let ctx = resolver.resolve_from_env()?;
    let parse = parse_boundary(root);
    let embedder = embedder_for(root, no_embeddings);
    let reranker = reranker_for(root);
    let app = AppService::open(&ctx, root, parse, embedder.clone(), Arc::new(SystemClock))
        .await?
        .with_reranker(reranker);
    Ok(CliApp {
        app: Arc::new(app),
        ctx,
        root: root.to_path_buf(),
        no_embeddings,
        embedder,
    })
}
