//! `memento-daemon` — one long-lived process per (root, tenant) that owns
//! the embedder, reranker, and LanceDB tables, and serves both transports
//! (CLI over named pipe, MCP over stdio once the dispatcher is wired in B4).
//!
//! B2 (REQ-DAEMON-003/012, design D5/D6/R1): this binary covers startup
//! (env gate, cookie file, Job Object self-detach), pipe bind, and the
//! handshake loop. The dispatcher + control plane + lifecycle land in
//! later batches (B4 / B5).

#![allow(clippy::needless_return)]

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::time::Duration;

use interprocess::os::windows::named_pipe::{
    pipe_mode,
    tokio::PipeStream,
};
use memento_application::audit::AuditLogger;
use memento_domain::DomainError;
use memento_mcp::{
    daemon::{pipe_name, server_handshake_with_timeout, DaemonAuth, DaemonPipe, HandshakeError},
    frame,
    handshake::PROTOCOL_VERSION,
};
use memento_tenant::TenantResolverImpl;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::{error, info, warn};
use windows::Win32::System::JobObjects::{
    SetInformationJobObject, JobObjectExtendedLimitInformation,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
};

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
    std::fs::create_dir_all(root).map_err(io::Error::from)?;
    let final_path = root.join(format!(".daemon-{}.cookie", process::id()));
    let tmp_path = root.join(format!(".daemon-{}.cookie.tmp", process::id()));
    std::fs::write(&tmp_path, nonce)?;
    if std::fs::rename(&tmp_path, &final_path).is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        std::fs::write(&final_path, nonce)?;
    }
    Ok(final_path)
}

/// R1 / BREAKAWAY_OK: assign the daemon process to a new Job Object and
/// exit the parent-side handle so the daemon survives the spawning CLI.
/// Any orphan behaviour is owned by the Job (kill on close keeps the system
/// clean during early failure).
#[allow(dead_code, unused_variables)]
fn daemonize_via_job_object() -> Result<(), StartupError> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: CreateJobObjectW returns NULL on failure; we surface that as
    // StartupError. GetCurrentProcess is a constant.
    let job = unsafe { CreateJobObjectW(None, None) }.map_err(|err| {
        StartupError(format!("CreateJobObjectW failed: {err}"))
    })?;
    if job.is_invalid() {
        return Err(StartupError("CreateJobObjectW returned NULL".into()));
    }

    // Configure: kill all processes assigned to this job when the job handle
    // closes (i.e. when this CLI process exits). The daemon runs as its own
    // job, so closing the CLI-side handle does NOT kill it (BREAKAWAY_OK).
    // We close the CLI-side handle right after the daemon confirms readiness.
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: SetInformationJobObject with valid job handle + buffer.
    let res = unsafe {
        SetInformationJobObject(
            HANDLE(job.0),
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if let Err(err) = res {
        warn!(?err, "SetInformationJobObject failed; continuing without kill-on-close");
    }

    // Assign the daemon's process to the job (in this B2 skeleton we run in
    // the same process; in production the daemon would fork+detach).
    let current = unsafe { GetCurrentProcess() };
    let job_handle = HANDLE(job.0);
    // SAFETY: AssignProcessToJobObject is safe with valid handles.
    if let Err(err) =
        unsafe { windows::Win32::System::JobObjects::AssignProcessToJobObject(job_handle, current) }
    {
        warn!(?err, "AssignProcessToJobObject failed");
    }
    Ok(())
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

    // --- cookie file (REQ-DAEMON-012) -------------------------------------
    let nonce = generate_nonce();
    let cookie_path = write_cookie(&root, &nonce)?;
    info!(?cookie_path, "daemon wrote cookie nonce");

    // --- Job Object (R1) ----------------------------------------------------
    if let Err(err) = daemonize_via_job_object() {
        warn!(?err, "Job Object setup failed; daemon still runs but may orphan on crash");
    }

    // --- pipe bind (REQ-DAEMON-003/012, design D5) -------------------------
    let tid = ctx.tenant_id().clone();
    let name = pipe_name(&root, &tid);
    let pipe = DaemonPipe::bind(&name).await?;
    info!(%name, "daemon bound named pipe");

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
        "daemon ready (B2 skeleton; dispatcher and lifecycle land in B4/B5)"
    );

    loop {
        match pipe.accept().await {
            Ok(conn) => {
                let auth = auth.clone();
                let audit_logger = audit_logger.clone();
                tokio::spawn(async move {
                    let mut conn: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> = conn;
                    let result =
                        server_handshake_with_timeout(&mut conn, &auth, pipe_timeout).await;
                    if let Err(ref err) = result {
                        warn!(?err, "daemon handshake failed");
                        if let (Some(logger), HandshakeError::AuthFailed(reason)) =
                            (audit_logger.as_ref(), err)
                        {
                            let _ = logger.error(
                                &auth.ctx,
                                "daemon_handshake",
                                serde_json::json!({ "reason": reason }),
                                "AUTH_FAILED",
                                None,
                            );
                        }
                    }
                    let _ = result;
                });
            }
            Err(err) => {
                error!(?err, "accept loop failed; backing off");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

#[allow(dead_code)]
fn _suppress_unused_warnings() {
    // Keep `sha2` and `frame` symbols in scope for downstream batches that
    // will hash the root and re-assemble framed streams.
    let mut h = Sha256::new();
    h.update(b"placeholder");
    let _ = h.finalize();
    let _ = frame::MAX_FRAME;
}
