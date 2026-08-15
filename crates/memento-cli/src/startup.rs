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
//!
//! ## B5 daemon-first connect (REQ-DAEMON-003/004, design D6/R1)
//!
//! [`try_open`] is the daemon-aware entry point. It probes the named
//! pipe first; if no daemon is alive, it calls
//! [`DaemonSpawner::start`](crate::spawn::DaemonSpawner::start) to spawn
//! one and retries the connect. The result is a [`CliBackend`] tagged
//! union:
//!
//! * `Local(CliApp)` — the in-process AppService path (today's default
//!   when the daemon is disabled or unreachable).
//! * `Remote(DaemonClient)` — the pipe-backed path. For B5 only the
//!   `daemon start|stop|status` control plane consumes the Remote
//!   variant; the delegable commands (ingest, search, …) still go
//!   through `Local`. B6/B7 will route every delegable command through
//!   `Remote`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memento_application::{AppService, SystemClock};
use memento_domain::{DomainError, TenantContext, TenantId};
use memento_embed_fastembed::{FastEmbedEmbedder, FastReranker, ModelLoader, Reranker};
use memento_parse::ParseService;
use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
use memento_ports::{EmbedPort, ParsePort, RerankPort};
use memento_tenant::TenantResolverImpl;

use crate::spawn::{DaemonSpawner, SpawnError, SpawnerOptions};
use crate::transport::pipe_client::{ClientConfig, DaemonClient, DaemonError};

/// The backend a CLI command runs against: either the in-process
/// `AppService` (current behavior) or the daemon pipe client
/// (REQ-DAEMON-002/003). Tagged union — at most one variant is live
/// at a time per `try_open` invocation.
pub enum CliBackend {
    /// The in-process path: AppService opened against the bound tenant.
    Local(CliApp),
    /// The daemon pipe path: a `DaemonClient` whose welcome echoes the
    /// daemon's bound tenant + spawn config.
    // Boxed to keep the enum size down (PipeStream is non-trivial).
    Remote(Box<DaemonClient>),
}

/// Everything a CLI command needs after startup (Local path).
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

/// B5 daemon-aware entry point: probe the named pipe, lazy-spawn the
/// daemon on miss, retry the connect. Returns the tagged-union backend
/// the rest of the CLI consumes.
///
/// ## Semantics (REQ-DAEMON-003 / R3)
///
/// * `MEMENTO_NO_DAEMON=1` → `Local` (no pipe contact, standalone path
///   preserved byte-for-byte — REQ-DAEMON-004).
/// * `Disabled` (same env on the client config) → `Local`.
/// * `MissingEnv` → `Local`. The standalone path needs the same env
///   anyway, so we fall back instead of erroring out.
/// * Pipe connect Ok → `Remote`.
/// * `PipeNotFound` / `Timeout` → spawn via
///   [`DaemonSpawner::start`] + retry connect; if the spawn fails (no
///   binary, lock busy, …) we fall back to `Local` so the operator
///   command still produces useful output (the control plane surfaces
///   the tier via `status`).
/// * `ConfigMismatch` → surface a structured `DomainError` so the
///   caller can choose the right exit code (R3 — refuse, never silent
///   divergence).
/// * Everything else → `DomainError::Internal` (connect broken, …).
pub async fn try_open(root: &Path, no_embeddings: bool) -> Result<CliBackend, DomainError> {
    // Gate 1: explicit disable → Local (REQ-DAEMON-004).
    if std::env::var(crate::transport::pipe_client::NO_DAEMON_ENV)
        .ok()
        .as_deref()
        == Some("1")
    {
        return open(root, no_embeddings).await.map(CliBackend::Local);
    }

    // Gate 2: try the daemon first. If the env gate rejects (missing
    // MEMENTO_TOKEN / MEMENTO_TENANT / …) we cannot connect; fall back
    // to Local so the CLI still produces output.
    let config = match ClientConfig::from_env() {
        Ok(c) => c,
        Err(DaemonError::Disabled) => {
            return open(root, no_embeddings).await.map(CliBackend::Local);
        }
        Err(DaemonError::MissingEnv(_)) => {
            return open(root, no_embeddings).await.map(CliBackend::Local);
        }
        Err(err) => return Err(daemon_err_to_domain(&err)),
    };

    match DaemonClient::connect(&config).await {
        Ok(client) => Ok(CliBackend::Remote(Box::new(client))),
        Err(DaemonError::ConfigMismatch(reason)) => {
            // R3: refuse with a clean error — never silently diverge.
            Err(DomainError::InvalidInput {
                message: format!("daemon config mismatch: {reason}"),
            })
        }
        Err(DaemonError::PipeNotFound(_)) | Err(DaemonError::Timeout(_)) => {
            // No daemon → try to lazy-spawn.
            spawn_and_retry(root, &config, no_embeddings).await
        }
        Err(err) => Err(daemon_err_to_domain(&err)),
    }
}

/// Lazy-spawn the daemon and retry the connect. On spawn failure
/// (no binary, lock busy) we fall back to Local so the operator
/// command still produces output; on connect failure post-spawn we
/// surface a structured domain error.
async fn spawn_and_retry(
    root: &Path,
    config: &ClientConfig,
    no_embeddings: bool,
) -> Result<CliBackend, DomainError> {
    let tenant_id: TenantId =
        config
            .tenant_id
            .parse()
            .map_err(|err| DomainError::InvalidInput {
                message: format!("invalid MEMENTO_TENANT: {err}"),
            })?;
    let opts = SpawnerOptions {
        root: root.to_path_buf(),
        tenant_id,
        no_embeddings,
        locale: config.locale.clone(),
    };
    match DaemonSpawner::start(&opts).await {
        Ok(_) => match DaemonClient::connect(config).await {
            Ok(client) => Ok(CliBackend::Remote(Box::new(client))),
            Err(err) => Err(daemon_err_to_domain(&err)),
        },
        Err(SpawnError::BinaryNotFound)
        | Err(SpawnError::LockBusy(_))
        | Err(SpawnError::Disabled)
        | Err(SpawnError::MissingEnv(_)) => {
            // Spawn could not proceed — fall back to Local. The operator
            // gets a working CLI; the daemon mode is opt-in (B6 closes
            // the config so MEMENTO_NO_DAEMON=1 becomes the EXPLICIT
            // escape hatch).
            tracing::warn!(
                tier = "spawn_fallback_to_local",
                "daemon spawn failed; falling back to in-process AppService"
            );
            open(root, no_embeddings).await.map(CliBackend::Local)
        }
        Err(err) => Err(spawn_err_to_domain(err)),
    }
}

/// Translate a [`DaemonError`] into the uniform [`DomainError`] surface
/// the CLI exit-code path (REQ-CL-005) already handles.
fn daemon_err_to_domain(err: &DaemonError) -> DomainError {
    match err {
        DaemonError::Disabled
        | DaemonError::MissingEnv(_)
        | DaemonError::PipeNotFound(_)
        | DaemonError::CookieMissing(_) => DomainError::InvalidInput {
            message: format!("{err}"),
        },
        DaemonError::ConfigMismatch(_) | DaemonError::AuthFailed(_) => DomainError::AuthFailed,
        DaemonError::Timeout(_) | DaemonError::Protocol(_) | DaemonError::Io(_) => {
            DomainError::Internal {
                message: format!("{err}"),
            }
        }
    }
}

/// Translate a [`SpawnError`] into the same uniform surface.
fn spawn_err_to_domain(err: SpawnError) -> DomainError {
    let message = format!("{err}");
    match err {
        SpawnError::Disabled
        | SpawnError::BinaryNotFound
        | SpawnError::MissingEnv(_)
        | SpawnError::LockBusy(_)
        | SpawnError::ReadinessTimeout(_) => DomainError::InvalidInput { message },
        SpawnError::SpawnFailedExit(_)
        | SpawnError::Shutdown(_)
        | SpawnError::Connect(_)
        | SpawnError::Io(_) => DomainError::Internal { message },
    }
}
