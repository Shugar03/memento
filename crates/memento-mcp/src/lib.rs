//! memento-mcp — Memento RS MCP stdio server (cluster H).
//!
//! The MCP surface is a THIN adapter (REQ-MS-006): it contains zero domain
//! behavior and delegates every operation to [`AppService`] (the use-case
//! layer, batch 7). It serves the 15-tool registry of REQ-MS-002
//! (`memory.*` in [`tools_memory`], `code.*` in [`tools_code`]) over the
//! MCP stdio transport (rmcp).
//!
//! Identity (REQ-MS-003): one process serves exactly one tenant
//! (REQ-TA-001). [`McpServer::startup`] resolves `MEMENTO_TOKEN` +
//! `MEMENTO_AGENT_ID` through the tenant resolver and refuses to start
//! without valid credentials — no tool call is ever processed
//! unauthenticated. The bound context is stored on the server and every
//! tool call passes it down; nothing in a request can override it
//! (REQ-TA-002), and the application layer re-validates it on every call
//! (REQ-TA-005 defense in depth).
//!
//! Bilingual surface (REQ-MS-004): tool descriptions come from the
//! memento-i18n ES-first tables; errors carry the ES primary message and
//! the EN fallback in one structured payload (see [`errors`]).
//!
//! Modules:
//! * [`errors`] — structured bilingual error conversion (REQ-MS-005).
//! * [`router`] — `ServerHandler` impl: capabilities, tools/list, tools/call.
//! * [`tools_memory`] — the 7 `memory.*` tools (T-072).
//! * [`tools_code`] — the 8 read-only `code.*` tools (T-073).

pub mod daemon;
pub mod dispatcher;
pub mod errors;
pub mod frame;
pub mod handshake;
pub mod job;
pub mod proxy;
pub mod router;
pub mod tools_code;
pub mod tools_memory;

use std::path::PathBuf;
use std::sync::Arc;

use memento_application::{AppService, SystemClock};
use memento_domain::{DomainError, SourceKind, TenantContext};
use memento_embed_fastembed::{FastEmbedEmbedder, FastReranker, ModelLoader, Reranker};
use memento_i18n::{I18n, Locale};
use memento_parse::ParseService;
use memento_tenant::TenantResolverImpl;
use rmcp::handler::server::router::tool::ToolRouter;

/// The MCP stdio server: one bound tenant, the 15-tool registry, zero
/// business logic (REQ-MS-006).
pub struct McpServer {
    /// The application use-case layer — every tool delegates here.
    app: Arc<AppService>,
    /// The process-bound tenant context (REQ-TA-001/002).
    ctx: TenantContext,
    /// The assembled tool registry (REQ-MS-002).
    router: ToolRouter<Self>,
    /// Bilingual strings (REQ-MS-004).
    i18n: I18n,
}

/// Production startup options (design D8 layout).
pub struct StartupOptions {
    /// Storage root (default `~/.memento`, design D8).
    pub root: PathBuf,
    /// anydoc staging directory.
    pub staging_dir: PathBuf,
    /// `--no-embeddings` mode: chunks stored without vectors (REQ-MC-004).
    pub no_embeddings: bool,
    /// Surface locale (ES-first default; EN is the fallback).
    pub locale: Option<Locale>,
}

impl McpServer {
    /// Resolve the process-bound context from the environment (REQ-MS-003,
    /// REQ-TA-002/003): `MEMENTO_TOKEN` + `MEMENTO_AGENT_ID`. Every
    /// failure — missing or invalid — refuses startup; nothing is served.
    pub fn resolve_startup_context(root: &std::path::Path) -> Result<TenantContext, DomainError> {
        TenantResolverImpl::open(root).resolve_from_env()
    }

    /// Production startup: credentials → bound context → application layer
    /// → server. Refuses to serve without valid credentials (REQ-MS-003).
    pub async fn startup(opts: StartupOptions) -> Result<Self, DomainError> {
        let ctx = Self::resolve_startup_context(&opts.root)?;
        let parse = ParseService::auto(opts.staging_dir)?;
        let embedder: Option<Arc<dyn memento_ports::EmbedPort>> = if opts.no_embeddings {
            None
        } else {
            Some(Arc::new(FastEmbedEmbedder::new(Arc::new(
                ModelLoader::new(opts.root.join("models"), true),
            ))))
        };
        // A1 cross-encoder reranker: lazy behind the MEMENTO_RERANK capability
        // toggle; the loader is cheap to construct and the ~543 MB int8 model
        // only loads on a per-query opt-in (SearchQuery.rerank).
        let reranker: Arc<dyn memento_ports::RerankPort> = Arc::new(FastReranker::new(Arc::new(
            Reranker::new(opts.root.clone()),
        )));
        let app = AppService::open(
            &ctx,
            &opts.root,
            Arc::new(parse),
            embedder,
            Arc::new(SystemClock),
        )
        .await?
        .with_reranker(reranker);
        Ok(Self::from_app(app, ctx, opts.locale.unwrap_or_default()))
    }

    /// Assemble a server around an already-bound application service. The
    /// context must belong to the service's tenant — production code
    /// reaches this only through [`Self::startup`]; tests inject testkit
    /// services.
    ///
    /// # Panics
    ///
    /// Panics if `ctx` does not belong to `app`'s bound tenant (a
    /// programming error: every tool call would fail the REQ-TA-005 guard).
    pub fn from_app(app: AppService, ctx: TenantContext, locale: Locale) -> Self {
        assert_eq!(
            app.store().tenant_id(),
            ctx.tenant_id(),
            "MCP server context must belong to the service tenant (REQ-TA-005)"
        );
        Self {
            app: Arc::new(app),
            router: router::tool_router(),
            ctx,
            i18n: I18n::load(locale),
        }
    }

    /// The bound tenant context (identity for every tool call).
    pub fn ctx(&self) -> &TenantContext {
        &self.ctx
    }

    /// The application service (tools delegate here; thin adapter).
    pub fn app(&self) -> &AppService {
        &self.app
    }

    /// The surface locale (ES-first, REQ-MS-004).
    pub fn locale(&self) -> Locale {
        self.i18n.locale()
    }

    /// The tool registry (tests inspect `list_all`).
    pub fn router(&self) -> &ToolRouter<Self> {
        &self.router
    }
}

/// Stable label for a source kind (mirrors the application layer's audit
/// label; kept here so MCP DTOs never leak the enum's debug shape).
pub(crate) fn source_label(source: &SourceKind) -> String {
    match source {
        SourceKind::Text => "text".to_string(),
        SourceKind::Markdown => "markdown".to_string(),
        SourceKind::Document(ext) => format!("document:{ext}"),
    }
}
