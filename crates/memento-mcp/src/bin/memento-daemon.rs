//! `memento-daemon` — one long-lived process per (root, tenant) that owns
//! the embedder, reranker, and LanceDB tables, and serves both transports
//! (CLI over named pipe, MCP over stdio once the dispatcher is wired in B4).
//!
//! B5 (REQ-DAEMON-003/009/010/013, design D5/R1/R2/R5/R7):
//!
//! * Removes the B2 skeleton [`daemonize_via_job_object`] helper (it
//!   created a Job Object inside the daemon and assigned itself — the
//!   opposite of what R1 prescribes).
//! * After readiness (cookie + pipe bound), runs
//!   [`check_startup_job_state`] to report the R1 startup-job state: the
//!   daemon may still be a member of the spawner's armed startup Job
//!   Object for a few milliseconds (the spawner disarms it right after
//!   observing readiness). Membership is expected; a disarmed job cannot
//!   kill the daemon, so the spawner's handle close is safe.
//! * Builds a [`DaemonState`] (the B5 dispatcher binding) and wires the
//!   accept loop to dispatch through [`dispatch_command_with_state`],
//!   closing the B4 skeleton by giving `sys.quiesce` / `sys.resume` /
//!   `sys.metrics` / `sys.shutdown` their real bodies (REQ-DAEMON-009/
//!   010/013, R2 / R5 / R7).
//! * Polls [`DaemonState::shutdown_requested`] between accepts; on
//!   `sys.shutdown`, breaks the loop and `process::exit(0)`s.

#![allow(clippy::needless_return)]

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use interprocess::os::windows::named_pipe::pipe_mode;
use memento_application::audit::AuditLogger;
use memento_application::{AppService, SystemClock};
use memento_domain::DomainError;
use memento_embed_fastembed::{FastEmbedEmbedder, FastReranker, ModelLoader, Reranker};
use memento_mcp::{
    daemon::{DaemonAuth, DaemonPipe, HandshakeError, pipe_name, server_handshake_with_timeout},
    dispatcher::{self, Command, DaemonState},
    frame,
    handshake::PROTOCOL_VERSION,
};
use memento_parse::ParseService;
use memento_tenant::TenantResolverImpl;
use rand_core::{OsRng, RngCore};
use tracing::{error, info, warn};

/// Default write bound for stalled-client handling (REQ-DAEMON-006). The
/// env `MEMENTO_DAEMON_PIPE_TIMEOUT` overrides at runtime.
const DEFAULT_PIPE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct StartupError(String);

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for StartupError {}

impl From<io::Error> for StartupError {
    fn from(err: io::Error) -> Self {
        StartupError(format!("io: {err}"))
    }
}
impl From<DomainError> for StartupError {
    fn from(err: DomainError) -> Self {
        StartupError(format!("domain: {err}"))
    }
}

fn required_env(name: &str) -> Result<String, StartupError> {
    env::var(name).map_err(|_| StartupError(format!("{name} is required")))
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok()
}

/// Generate a 32-byte cryptographic nonce encoded as hex. Used as the
/// filesystem cookie nonce (REQ-DAEMON-012).
fn generate_nonce() -> String {
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    let mut hex = String::with_capacity(64);
    for byte in buf.iter() {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Write the cookie file `<root>/.daemon-<pid>.cookie` (REQ-DAEMON-012).
/// Atomic-ish: write to a temp file then rename to the final name.
fn write_cookie(root: &Path, nonce: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    let pid = std::process::id();
    let final_path = root.join(format!(".daemon-{pid}.cookie"));
    let tmp_path = root.join(format!(".daemon-{pid}.cookie.tmp"));
    std::fs::write(&tmp_path, nonce)?;
    if std::fs::rename(&tmp_path, &final_path).is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        std::fs::write(&final_path, nonce)?;
    }
    Ok(final_path)
}

/// R1 startup-job state check (REQ-DAEMON-003 GIVEN-3). The daemon may
/// briefly be a member of the spawner's startup Job Object: the spawner
/// binds the child to an armed `KILL_ON_JOB_CLOSE` job, waits for
/// readiness (cookie + pipe — which this daemon has just signaled), and
/// then disarms the flag and closes the handle. Membership at this
/// instant is EXPECTED — the disarmed job lets the daemon outlive the
/// spawner milliseconds later.
///
/// Returns `true` when the daemon is free (no job membership — spawned
/// standalone, or the spawner already released the job), `false` when it
/// is still inside the startup job (transient, expected). The function
/// never fails the daemon; only an API failure is worth a warning.
fn check_startup_job_state() -> bool {
    use windows::Win32::System::JobObjects::IsProcessInJob;
    use windows::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: GetCurrentProcess returns a pseudo-handle that is always
    // valid; passing `None` for the job handle asks whether the process is
    // in ANY job. The out-param is a windows_core::BOOL.
    let mut in_job = windows::core::BOOL::default();
    let call = unsafe { IsProcessInJob(GetCurrentProcess(), None, &mut in_job) };
    match call {
        Ok(()) if in_job.0 == 0 => {
            info!("post-readiness job check: daemon is not in any job (R1 released)");
            true
        }
        Ok(()) => {
            info!(
                "post-readiness job check: daemon is inside the startup job (expected; spawner disarms it now)"
            );
            false
        }
        Err(err) => {
            warn!(
                ?err,
                "post-readiness job check: IsProcessInJob failed; orphan protection cannot be asserted"
            );
            false
        }
    }
}

/// Build the production parse boundary (REQ-CL-007 — anydoc fallback
/// keeps md/txt + ingest_text working on hosts without Node).
fn parse_boundary(root: &Path) -> Arc<dyn memento_ports::ParsePort> {
    match ParseService::auto(root.join("tmp")) {
        Ok(service) => Arc::new(service),
        Err(err) => {
            tracing::warn!(%err, "anydoc unavailable; document conversion fails per-call (fallback md/txt works)");
            Arc::new(ParseService::new(memento_parse::anydoc::AnydocConfig {
                command: memento_parse::anydoc::AnydocCommand {
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

/// Production embedder for this root (lazy model load — nothing is
/// downloaded at startup; first embed triggers the single-flight load).
fn embedder_for(root: &Path, no_embeddings: bool) -> Option<Arc<dyn memento_ports::EmbedPort>> {
    if no_embeddings {
        None
    } else {
        Some(Arc::new(FastEmbedEmbedder::new(Arc::new(
            ModelLoader::new(root.join("models"), true),
        ))))
    }
}

/// Cross-encoder reranker (A1) — loader is cheap, the ~543 MB int8 model
/// loads on the first rerank call behind the `MEMENTO_RERANK` capability
/// toggle.
fn reranker_for(root: &Path) -> Arc<dyn memento_ports::RerankPort> {
    Arc::new(FastReranker::new(Arc::new(Reranker::new(
        root.to_path_buf(),
    ))))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), StartupError> {
    // --- env gate (REQ-DAEMON-002/005) --------------------------------------
    let root = PathBuf::from(required_env("MEMENTO_ROOT")?);
    let _token = required_env("MEMENTO_TOKEN")?;
    let _agent_id = required_env("MEMENTO_AGENT_ID")?;
    let _tenant_id = required_env("MEMENTO_TENANT")?;
    let no_embeddings = optional_env("MEMENTO_NO_EMBEDDINGS")
        .map(|v| v == "1")
        .unwrap_or(false);
    let locale = optional_env("MEMENTO_LOCALE");

    let pipe_timeout_secs: f64 = optional_env("MEMENTO_DAEMON_PIPE_TIMEOUT")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PIPE_TIMEOUT.as_secs_f64());
    let pipe_timeout = Duration::from_secs_f64(pipe_timeout_secs);

    // Resolve the bound TenantContext through the actual credential resolver
    // (REQ-DAEMON-005: token validated against the credential store at
    // startup, not in the daemon hot path).
    let resolver = TenantResolverImpl::open(&root);
    let ctx = resolver
        .resolve_from_env()
        .map_err(|err| StartupError(format!("resolve_from_env: {err}")))?;
    let bound_token_str = env::var("MEMENTO_TOKEN")
        .map_err(|_| StartupError("MEMENTO_TOKEN missing at startup".into()))?;

    // --- adapter Arcs (preserved across quiesce/resume, R2) ---------------
    let parse = parse_boundary(&root);
    let embedder = embedder_for(&root, no_embeddings);
    let reranker = if no_embeddings {
        None
    } else {
        Some(reranker_for(&root))
    };

    // --- cookie file (REQ-DAEMON-012) — readiness signal #1 ---------------
    let nonce = generate_nonce();
    let cookie_path = write_cookie(&root, &nonce)?;
    info!(?cookie_path, "daemon wrote cookie nonce");

    // --- AppService open (REQ-DAEMON-002, R2) -----------------------------
    let app = AppService::open(
        &ctx,
        &root,
        parse.clone(),
        embedder.clone(),
        Arc::new(SystemClock),
    )
    .await?;
    let app = match reranker.clone() {
        Some(r) => app.with_reranker(r),
        None => app,
    };
    let clock: Arc<dyn memento_application::Clock> = Arc::new(SystemClock);
    let state = Arc::new(DaemonState::new(
        root.clone(),
        ctx.clone(),
        parse.clone(),
        embedder.clone(),
        reranker.clone(),
        clock,
        app,
    ));

    // --- pipe bind (REQ-DAEMON-003/012, design D5) — readiness signal #2 --
    let tid = *ctx.tenant_id();
    let name = pipe_name(&root, &tid);
    let pipe = DaemonPipe::bind(&name).await?;
    info!(%name, "daemon bound named pipe");

    // --- R1 startup-job state (post-readiness) --------------------------
    // The daemon is fully bound to its (root, tenant) and listening on the
    // pipe — the spawner (`DaemonSpawner::start`) observes readiness now
    // and disarms + releases its startup Job Object. This check only
    // reports the transient state (and warns if the API itself fails).
    let detached = check_startup_job_state();
    if !detached {
        info!("R1: daemon still inside the startup job; spawner disarm + release is in flight");
    }

    // --- audit logger for auth failures (best-effort) --------------------
    // Wrapped in Arc so each spawned task can move a clone without copying
    // the AuditLogger (it doesn't implement Clone); tasks that fail to write
    // just log and drop.
    let audit_logger: Arc<Option<AuditLogger>> =
        Arc::new(AuditLogger::new(&root, ctx.tenant_id()).ok());

    // --- accept loop (REQ-DAEMON-002/003) ----------------------------------
    let auth = DaemonAuth {
        root: root.clone(),
        ctx: ctx.clone(),
        daemon_token: bound_token_str,
        cookie_path: cookie_path.clone(),
        no_embeddings,
        locale,
    };

    info!(
        pid = std::process::id(),
        proto = PROTOCOL_VERSION,
        r1_detached = detached,
        "daemon ready (B5: dispatcher + R1 self-detach wired)"
    );

    loop {
        if state.shutdown_requested() {
            info!(
                pid = std::process::id(),
                "daemon: sys.shutdown observed; exiting cooperatively"
            );
            // Reply OK has already been sent by the dispatcher; exit 0 is
            // the contract (REQ-DAEMON-013, R7).
            std::process::exit(0);
        }
        let conn = match pipe.accept().await {
            Ok(conn) => conn,
            Err(err) => {
                error!(?err, "accept loop failed; backing off");
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        let auth = auth.clone();
        let audit_logger = audit_logger.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let mut conn: interprocess::os::windows::named_pipe::tokio::PipeStream<
                pipe_mode::Bytes,
                pipe_mode::Bytes,
            > = conn;
            let result = server_handshake_with_timeout(&mut conn, &auth, pipe_timeout).await;
            let welcome = match result {
                Ok(w) => w,
                Err(ref err) => {
                    warn!(?err, "daemon handshake failed");
                    if let (Some(logger), HandshakeError::AuthFailed(reason)) =
                        (audit_logger.as_ref(), err)
                    {
                        logger.error(
                            &auth.ctx,
                            "daemon_handshake",
                            serde_json::json!({ "reason": reason }),
                            "AUTH_FAILED",
                            None,
                        );
                    }
                    let _ = result;
                    return;
                }
            };
            // Handshake complete — serve commands on this connection. The
            // dispatcher is request-scoped: the wire shape is one framed
            // request → one framed response, and the accept loop spawns a
            // new task per request. B7 wires the per-tool AppService
            // calls; today sys.* are the real path and mcp.* stays as a
            // routing marker.
            serve_request(&mut conn, state, &welcome).await;
        });
    }
}

/// Per-connection service loop: read one framed request, dispatch through
/// the B5 [`DaemonState`], write one framed response.
///
/// REQ-DAEMON-012 role gate (D4): a connection that presented
/// `Role::McpProxy` may only reach the public 15 tools — `sys.*` is
/// CLI-only. Refusals (and dispatch failures) are answered with a
/// structured error envelope so the client never hangs on a dead request.
async fn serve_request<S>(
    conn: &mut S,
    state: Arc<DaemonState>,
    welcome: &memento_mcp::handshake::Welcome,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let raw = match tokio::time::timeout(Duration::from_secs(5), frame::read_message(conn)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => {
            warn!(?err, "daemon: request read failed");
            return;
        }
        Err(_) => {
            warn!("daemon: request read timed out");
            return;
        }
    };
    let cmd: Command = match serde_json::from_slice(&raw) {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "daemon: request is not a valid Command");
            return;
        }
    };
    // Role gate (REQ-DAEMON-012): mcp_proxy connections are confined to
    // the public 15 tools. The refusal is a structured envelope — the
    // stdio proxy translates it into a tool-level error.
    if welcome.role == memento_mcp::handshake::Role::McpProxy && matches!(cmd, Command::Sys(_)) {
        warn!(
            path = %cmd.path(),
            "daemon: role gate refused sys.* for mcp_proxy"
        );
        let payload = serde_json::to_vec(&serde_json::json!({
            "status": "error",
            "code": "FORBIDDEN",
            "message": "sys.* is CLI-only (REQ-DAEMON-012); the MCP stdio proxy exposes the public 15 tools only",
            "exit_code": 2,
        }))
        .expect("refusal envelope serializes");
        if let Err(err) = frame::write_message(conn, &payload).await {
            warn!(?err, "daemon: refusal write failed");
        }
        return;
    }
    let value = match dispatcher::dispatch_command_with_state(&state, cmd).await {
        Ok(v) => v,
        Err(err) => {
            // Structured error envelope: the client gets a readable
            // failure instead of a hang (REQ-DAEMON-006: request fails,
            // daemon keeps serving).
            warn!(?err, "daemon: dispatch failed");
            let err_value: serde_json::Value = err.into();
            serde_json::json!({
                "status": "error",
                "code": err_value["code"],
                "message": err_value["message"],
                "exit_code": err_value["exit_code"],
            })
        }
    };
    let payload = dispatcher::serialize_response(&value);
    if let Err(err) = frame::write_message(conn, &payload).await {
        warn!(?err, "daemon: response write failed");
    }
}
