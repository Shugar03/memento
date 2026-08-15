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

/// REQ-DAEMON-013 policy constants: at most 3 spawn attempts per command
/// (1 spawn + ≤2 restarts) with 250 ms / 500 ms backoff. A daemon that
/// crashes on every start exhausts the budget and fails with
/// `DAEMON_UNAVAILABLE` — never an infinite spawn loop.
pub const MAX_DAEMON_SPAWN_ATTEMPTS: usize = 3;
/// Backoff between retry attempts (index 0 = after attempt 0, etc.).
pub const DAEMON_RETRY_BACKOFF_MS: [u64; 2] = [250, 500];

/// The outcome of one attempt in the bounded auto-restart loop
/// (REQ-DAEMON-013). The loop only retries [`RetryOutcome::Retryable`];
/// [`RetryOutcome::Fatal`] surfaces immediately.
pub enum RetryOutcome<T> {
    /// The operation succeeded — the loop returns the value.
    Done(T),
    /// A deterministic failure (CONFIG_MISMATCH, AUTH_FAILED, …) — no
    /// retry, the error surfaces as-is.
    Fatal(DomainError),
    /// A transient failure (spawn race, lock busy, broken pipe, …) —
    /// eligible for the bounded retry with backoff.
    Retryable(DomainError),
}

/// REQ-DAEMON-013 bounded auto-restart loop (pure policy — tests inject
/// attempt outcomes instead of spawning processes). Runs `attempt` up to
/// [`MAX_DAEMON_SPAWN_ATTEMPTS`] times, sleeping the 250/500 ms backoff
/// between retries; a crash loop (every attempt retryable-failing) ends
/// in `DAEMON_UNAVAILABLE` with the last error as detail.
pub async fn bounded_retry_loop<T, F, Fut>(mut attempt: F) -> Result<T, DomainError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = RetryOutcome<T>>,
{
    // One attempt per backoff slot, then one final attempt: total
    // MAX_DAEMON_SPAWN_ATTEMPTS = 3 (1 spawn + ≤2 retries).
    for backoff_ms in DAEMON_RETRY_BACKOFF_MS.iter() {
        match attempt().await {
            RetryOutcome::Done(value) => return Ok(value),
            RetryOutcome::Fatal(err) => return Err(err),
            RetryOutcome::Retryable(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(*backoff_ms)).await;
            }
        }
    }
    match attempt().await {
        RetryOutcome::Done(value) => Ok(value),
        RetryOutcome::Fatal(err) => Err(err),
        RetryOutcome::Retryable(err) => Err(DomainError::DaemonUnavailable {
            message: format!(
                "daemon unavailable after {MAX_DAEMON_SPAWN_ATTEMPTS} attempts: {err}"
            ),
        }),
    }
}

/// Whether a [`DaemonError`] is transient (daemon died / not yet ready)
/// and therefore retryable under REQ-DAEMON-013. Auth and config
/// mismatches are deterministic — retrying them cannot succeed.
pub fn is_retryable_daemon_error(err: &DaemonError) -> bool {
    matches!(
        err,
        DaemonError::PipeBroken(_)
            | DaemonError::PipeNotFound(_)
            | DaemonError::Timeout(_)
            | DaemonError::Io(_)
    )
}

/// Lazy-spawn the daemon and retry the connect with the REQ-DAEMON-013
/// bounded auto-restart policy: ≤2 retries with 250/500 ms backoff; a
/// crash loop (every attempt failing) ends in `DAEMON_UNAVAILABLE` —
/// never an infinite spawn loop and never a silent fallback to the
/// in-process path. The only Local fallbacks are the explicit REQ-DAEMON-004
/// gates (`MEMENTO_NO_DAEMON=1` / `--no-daemon` / missing env).
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
    bounded_retry_loop(|| attempt_spawn(root, config, no_embeddings, &opts)).await
}

/// One spawn+connect attempt (REQ-DAEMON-013). A hard gate
/// (`MEMENTO_NO_DAEMON=1` / missing env) resolves to the Local one-shot
/// path (REQ-DAEMON-004); spawn/connect failures after the process was
/// started are retryable.
async fn attempt_spawn(
    root: &Path,
    config: &ClientConfig,
    no_embeddings: bool,
    opts: &SpawnerOptions,
) -> RetryOutcome<CliBackend> {
    match DaemonSpawner::start(opts).await {
        Ok(_) => match DaemonClient::connect(config).await {
            Ok(client) => RetryOutcome::Done(CliBackend::Remote(Box::new(client))),
            Err(err) => match &err {
                // Deterministic refusals — surface immediately (R3,
                // REQ-DAEMON-005): retrying cannot heal a token/cookie
                // mismatch or a diverging spawn config.
                DaemonError::ConfigMismatch(_)
                | DaemonError::AuthFailed(_)
                | DaemonError::Protocol(_)
                | DaemonError::CookieMissing(_) => RetryOutcome::Fatal(daemon_err_to_domain(&err)),
                // Transport failures right after spawn = the daemon is
                // not ready yet or died — retryable.
                _ => RetryOutcome::Retryable(daemon_err_to_domain(&err)),
            },
        },
        // REQ-DAEMON-004 gates: explicit disable / missing env → the
        // one-shot Local path (byte-compat, no pipe contact).
        Err(SpawnError::Disabled) | Err(SpawnError::MissingEnv(_)) => {
            match open(root, no_embeddings).await {
                Ok(app) => RetryOutcome::Done(CliBackend::Local(app)),
                Err(err) => RetryOutcome::Fatal(err),
            }
        }
        // Everything else (lock busy, binary missing, readiness timeout,
        // job object failure, crash) is transient: bounded retry.
        Err(err) => RetryOutcome::Retryable(spawn_err_to_domain(err)),
    }
}

/// REQ-DAEMON-013 mid-request death recovery: resolve a Remote backend
/// (spawning the daemon if needed, via [`try_open`]), run `dispatch`
/// against it, and if the wire breaks mid-request (`PipeBroken` — the
/// daemon died), respawn through the bounded retry policy and retry the
/// dispatch. A crash loop ends in `DAEMON_UNAVAILABLE`.
///
/// The caller must be a daemon-dispatch flow (delegable commands,
/// future MCP proxy); Local-only paths keep using [`open`]. The retry
/// bookkeeping mirrors [`bounded_retry_loop`] (same constants, same
/// semantics) — the loop is inlined because a `FnMut` closure cannot
/// return a future borrowing its own captured state.
pub async fn with_daemon_retry<F, T>(
    root: &Path,
    no_embeddings: bool,
    mut dispatch: F,
) -> Result<T, DomainError>
where
    F: FnMut(&mut DaemonClient) -> Result<T, DaemonError>,
{
    for backoff_ms in DAEMON_RETRY_BACKOFF_MS.iter() {
        match attempt_dispatch(root, no_embeddings, &mut dispatch).await {
            RetryOutcome::Done(value) => return Ok(value),
            RetryOutcome::Fatal(err) => return Err(err),
            RetryOutcome::Retryable(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(*backoff_ms)).await;
            }
        }
    }
    match attempt_dispatch(root, no_embeddings, &mut dispatch).await {
        RetryOutcome::Done(value) => Ok(value),
        RetryOutcome::Fatal(err) => Err(err),
        RetryOutcome::Retryable(err) => Err(DomainError::DaemonUnavailable {
            message: format!(
                "daemon unavailable after {MAX_DAEMON_SPAWN_ATTEMPTS} attempts: {err}"
            ),
        }),
    }
}

/// One connect+dispatch attempt for [`with_daemon_retry`].
async fn attempt_dispatch<F, T>(
    root: &Path,
    no_embeddings: bool,
    dispatch: &mut F,
) -> RetryOutcome<T>
where
    F: FnMut(&mut DaemonClient) -> Result<T, DaemonError>,
{
    match try_open(root, no_embeddings).await {
        Ok(CliBackend::Remote(mut client)) => match dispatch(&mut client) {
            Ok(value) => RetryOutcome::Done(value),
            Err(err) if is_retryable_daemon_error(&err) => {
                RetryOutcome::Retryable(daemon_err_to_domain(&err))
            }
            Err(err) => RetryOutcome::Fatal(daemon_err_to_domain(&err)),
        },
        Ok(CliBackend::Local(_)) => RetryOutcome::Fatal(DomainError::DaemonUnavailable {
            message: "daemon mode disabled (MEMENTO_NO_DAEMON=1); cannot dispatch over the pipe"
                .into(),
        }),
        Err(err) => RetryOutcome::Fatal(err),
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
        DaemonError::PipeBroken(_) => DomainError::DaemonUnavailable {
            message: format!("{err}"),
        },
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
        | SpawnError::JobObjectFailed(_)
        | SpawnError::Shutdown(_)
        | SpawnError::Connect(_)
        | SpawnError::Io(_) => DomainError::Internal { message },
    }
}

#[cfg(test)]
mod tests {
    //! REQ-DAEMON-013 auto-restart policy tests (pure policy — no process
    //! spawning; the loop is exercised with injected attempt outcomes).

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// A real-daemon-free attempt counter for the retry-loop tests.
    fn counter() -> (Arc<AtomicUsize>, usize) {
        (Arc::new(AtomicUsize::new(0)), 0)
    }

    #[tokio::test]
    async fn bounded_retry_loop_succeeds_on_first_attempt_without_backoff() {
        let (attempts, _) = counter();
        let start = Instant::now();
        let result = bounded_retry_loop(|| {
            let _ = attempts.fetch_add(1, Ordering::SeqCst);
            async { RetryOutcome::Done("ok") }
        })
        .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "single attempt");
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "no backoff on first-attempt success: {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn bounded_retry_loop_recovers_within_two_retries_with_backoff() {
        // REQ-DAEMON-013 GIVEN: a transiently failing spawn (lock busy /
        // readiness race) recovers within the bounded ≤2 retries; the
        // 250 ms / 500 ms backoff is honored between attempts.
        let (attempts, _) = counter();
        let start = Instant::now();
        let result = bounded_retry_loop(|| {
            let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n < 3 {
                    RetryOutcome::<&str>::Retryable(DomainError::Internal {
                        message: format!("transient attempt {n}"),
                    })
                } else {
                    RetryOutcome::<&str>::Done("daemon-ready")
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), "daemon-ready");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "exactly 3 attempts (1 + 2 retries)"
        );
        assert!(
            start.elapsed() >= Duration::from_millis(700),
            "250 ms + 500 ms backoff honored: {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn bounded_retry_loop_crash_loop_ends_in_daemon_unavailable() {
        // REQ-DAEMON-013 GIVEN: a daemon that crashes on every start must
        // end in DAEMON_UNAVAILABLE after the bounded attempts — never an
        // infinite spawn loop.
        let (attempts, _) = counter();
        let result = bounded_retry_loop(|| {
            let _ = attempts.fetch_add(1, Ordering::SeqCst);
            async {
                RetryOutcome::<&str>::Retryable(DomainError::Internal {
                    message: "crash".into(),
                })
            }
        })
        .await;
        let err = result.expect_err("crash loop fails");
        assert!(
            matches!(err, DomainError::DaemonUnavailable { .. }),
            "DAEMON_UNAVAILABLE tier: {err:?}"
        );
        assert_eq!(err.exit_code(), 19, "REQ-CL-005 exit code");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "bounded: exactly 3 attempts, no 4th spawn"
        );
    }

    #[tokio::test]
    async fn bounded_retry_loop_fatal_failure_surfaces_immediately() {
        // Deterministic failures (CONFIG_MISMATCH, AUTH_FAILED) must not
        // be retried — they surface on the first attempt.
        let (attempts, _) = counter();
        let result = bounded_retry_loop(|| {
            let _ = attempts.fetch_add(1, Ordering::SeqCst);
            async { RetryOutcome::<&str>::Fatal(DomainError::AuthFailed) }
        })
        .await;
        assert!(matches!(result, Err(DomainError::AuthFailed)));
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "no retry on fatal");
    }

    #[test]
    fn pipe_broken_and_transport_failures_are_retryable() {
        // REQ-DAEMON-013: mid-request death (BROKEN_PIPE) and transport
        // hiccups are retryable; auth/config failures are not.
        assert!(is_retryable_daemon_error(&DaemonError::PipeBroken(
            "daemon died mid-request".into()
        )));
        assert!(is_retryable_daemon_error(&DaemonError::Timeout(
            Duration::from_secs(5)
        )));
        assert!(is_retryable_daemon_error(&DaemonError::PipeNotFound(
            "pipe".into()
        )));
        assert!(!is_retryable_daemon_error(&DaemonError::AuthFailed(
            "token".into()
        )));
        assert!(!is_retryable_daemon_error(&DaemonError::ConfigMismatch(
            "locale".into()
        )));
    }

    #[test]
    fn pipe_broken_maps_to_daemon_unavailable_tier() {
        // A broken pipe means the daemon is gone — the caller sees the
        // DAEMON_UNAVAILABLE tier, not a generic IO error.
        let err = daemon_err_to_domain(&DaemonError::PipeBroken("died".into()));
        assert!(
            matches!(err, DomainError::DaemonUnavailable { .. }),
            "pipe broken → DAEMON_UNAVAILABLE: {err:?}"
        );
        assert_eq!(err.code(), "DAEMON_UNAVAILABLE");
    }
}
