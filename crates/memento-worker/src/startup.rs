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
use memento_domain::{DomainError, SourceKind, TenantContext, TenantId};
use memento_observability::sampler::{Clock as SamplerClock, Sampler, SystemProbe};
use memento_observability::EventSink;
use memento_ports::{ParsePort, ParsedDocument};
use memento_tenant::TenantResolverImpl;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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

/// Build the gated process sampler for the worker's bound tenant
/// (REQ-OBS-011, design D6).
///
/// Reads `MEMENTO_OBSERVE_SAMPLES`: returns `Some(Sampler)` only when the
/// var is exactly `"1"`, `None` otherwise (default off — zero I/O while
/// off). The events land in the bound tenant's file (`logs/<tid>.events.jsonl`)
/// via the same best-effort [`EventSink`] the application uses. The clock
/// and probe are injected (the real impls are `SystemClock`/`SysinfoProbe`),
/// so the worker never touches sysinfo directly — the [`SystemProbe`] trait
/// isolates that API churn (D6).
///
/// The sampler is worker-only by construction: the CLI and MCP entrypoints
/// never call this function.
pub fn build_sampler(
    root: &Path,
    tenant: &TenantId,
    interval: Duration,
    clock: Arc<dyn SamplerClock>,
    probe: Arc<dyn SystemProbe>,
) -> Option<Sampler> {
    if std::env::var("MEMENTO_OBSERVE_SAMPLES").ok().as_deref() != Some("1") {
        return None;
    }
    // Best-effort, same contract as the application's event sink: an
    // unwritable events file disables the sampler with a warn — it never
    // fails the worker startup.
    let sink = match EventSink::tenant(root, tenant) {
        Ok(sink) => sink,
        Err(err) => {
            tracing::warn!(%err, "sampler requested but the tenant events file could not be opened; disabled");
            return None;
        }
    };
    Some(Sampler::new(interval, clock, probe, sink))
}
