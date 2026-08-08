//! Worker startup: the process-bound tenant + a worker `AppService`.
//!
//! Identity flow mirrors the CLI/MCP surfaces (REQ-TA-002/003): the env pair
//! `MEMENTO_TOKEN` and `MEMENTO_AGENT_ID` resolves the bound [`TenantContext`]
//! through [`TenantResolverImpl`]; nothing runs without valid credentials.
//!
//! One deliberate difference from the other surfaces: the worker opens the
//! application layer with a NEVER-INVOKED parse boundary and NO embedder.
//! Worker jobs (sweep / compact / prune / backup) never normalize documents
//! and never embed, so:
//!
//! * no ONNX model download happens on the server process (REQ-CG-004: the
//!   sole outbound op stays avoidable — the worker never triggers it);
//! * no anydoc/Node dependency is required on the host (the batch-9 CLI
//!   degradation note does not apply: there is no document surface here).

use memento_application::AppService;
use memento_domain::{DomainError, SourceKind, TenantContext};
use memento_ports::{ParsePort, ParsedDocument};
use memento_tenant::TenantResolverImpl;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Everything the worker needs after startup.
pub struct WorkerContext {
    /// The application use-case layer (cluster G) bound to this tenant.
    pub app: Arc<AppService>,
    /// The process-bound tenant context (REQ-TA-001/002).
    pub ctx: TenantContext,
    /// Storage root (D8 layout).
    pub root: PathBuf,
}

/// The parse boundary the worker never calls (see module docs). It fails
/// structurally if anything ever tries — an impossible path by construction.
struct NoParse;

#[async_trait::async_trait]
impl ParsePort for NoParse {
    async fn parse(&self, _blob: &[u8], _hint: SourceKind) -> Result<ParsedDocument, DomainError> {
        Err(DomainError::Parse {
            message: "the worker has no parse boundary (jobs never normalize documents)".into(),
        })
    }
}

/// Resolve the bound context from `MEMENTO_TOKEN` + `MEMENTO_AGENT_ID` and
/// open the worker's application layer (REQ-MS-003 semantics: nothing runs
/// without valid credentials).
///
/// # Errors
///
/// * `AuthFailed` — missing/invalid `MEMENTO_TOKEN` (uniform, REQ-TA-006).
/// * `InvalidInput` — missing `MEMENTO_AGENT_ID` (REQ-TA-003).
/// * `Io` — the store cannot be opened.
pub async fn open(root: &Path) -> Result<WorkerContext, DomainError> {
    let resolver = TenantResolverImpl::open(root);
    let ctx = resolver.resolve_from_env()?;
    let parse: Arc<dyn ParsePort> = Arc::new(NoParse);
    let app = AppService::open(
        &ctx,
        root,
        parse,
        None,
        Arc::new(memento_application::SystemClock),
    )
    .await?;
    Ok(WorkerContext {
        app: Arc::new(app),
        ctx,
        root: root.to_path_buf(),
    })
}
