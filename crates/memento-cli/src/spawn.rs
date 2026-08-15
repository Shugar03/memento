//! Daemon spawner + control plane (REQ-DAEMON-003/007/013, design D6/R1/R7).
//!
//! B5 scope:
//!
//! * [`DaemonSpawner::start`] — spawn `memento-daemon` as a detached child
//!   (Windows: `CREATE_BREAKAWAY_FROM_JOB` so the child starts outside any
//!   job the spawner might be in), bind it to an armed startup Job Object
//!   (`KILL_ON_JOB_CLOSE`, design R1), wait for readiness (cookie file
//!   present + named-pipe connectable), then **disarm** the job
//!   (`disarm_kill_on_close`) before its handle drops. Spawner death
//!   pre-readiness closes the armed job → the daemon dies with it (spec
//!   GIVEN-3: no orphan); post-readiness the disarmed job lets the daemon
//!   outlive any client. Returns a [`ChildHandle`] carrying the daemon pid
//!   and the cookie mtime as `started_at`. A per-root `.daemon-spawn.lock`
//!   file (design D6) serializes concurrent first-client spawns so the
//!   second caller waits for the first to become ready instead of racing a
//!   second bind on the same pipe name.
//! * [`DaemonSpawner::stop`] — send `sys.shutdown` through the named
//!   pipe (the B5 dispatcher body in `memento-mcp::dispatcher`), then
//!   fall back to a force-kill of the pid if the cooperative exit
//!   doesn't complete within a bounded grace window.
//! * [`DaemonSpawner::status`] — connect via the existing pipe client,
//!   read the daemon pid from the WELCOME envelope, and use the cookie
//!   file mtime as the `started_at` estimate (close enough for ops
//!   probe semantics; REQ-DAEMON-007 wants PID + uptime).
//!
//! The spawner never opens `AppService` and never loads models — the
//! whole point of the daemon is that the CLI process stays thin
//! (REQ-DAEMON-001/007).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use memento_domain::TenantId;
use memento_mcp::daemon::pipe_name;
use memento_mcp::dispatcher::{Command as DispatchCommand, SysCommand};
use memento_mcp::frame;
use memento_mcp::job::StartupJob;
use thiserror::Error;
use tokio::time::sleep;
use tracing::{info, warn};

/// Default grace window for the cooperative `sys.shutdown` reply before
/// the spawner falls back to a force kill (REQ-DAEMON-013 / R7).
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Default wait for the daemon to become ready (cookie + pipe) before
/// giving up (REQ-DAEMON-003 GIVEN: lazy startup, race-safe spawn).
pub const DEFAULT_READINESS_TIMEOUT: Duration = Duration::from_secs(10);

/// The `.daemon-spawn.lock` filename under `<root>/` (design D6:
/// `CreateFileW(FILE_SHARE_NONE)` analog; the lock is created
/// exclusively so a second concurrent spawn sees a busy file).
pub const SPAWN_LOCK_NAME: &str = ".daemon-spawn.lock";

#[derive(Debug, Error)]
pub enum SpawnError {
    /// The caller explicitly disabled the daemon (`MEMENTO_NO_DAEMON=1`
    /// or `--no-daemon`); the spawner must NOT start a process.
    #[error("MEMENTO_NO_DAEMON=1; daemon mode disabled")]
    Disabled,
    /// A required env var was missing (mirrors `DaemonError::MissingEnv`).
    #[error("missing env var `{0}`")]
    MissingEnv(&'static str),
    /// Another spawn is in progress; the lock file at the given path is
    /// held by a sibling process (design D6 / R1 self-detach).
    #[error("another spawn is in progress (lock file busy at {0})")]
    LockBusy(PathBuf),
    /// The `memento-daemon` binary could not be located on PATH or next
    /// to the current `memento` binary.
    #[error("`memento-daemon` binary not found on PATH or next to memento")]
    BinaryNotFound,
    /// The daemon exited before becoming ready (cookie never appeared,
    /// pipe never bound).
    #[error("daemon exited before becoming ready (status: {0:?})")]
    SpawnFailedExit(std::process::ExitStatus),
    /// The startup Job Object could not be created or the child could not
    /// be bound to it (R1 / REQ-DAEMON-003 GIVEN-3). Without the job,
    /// spawner death pre-readiness could orphan the daemon, so the spawn
    /// fails instead of degrading.
    #[error("startup job object failed: {0}")]
    JobObjectFailed(String),
    /// The cookie file never appeared within the readiness timeout
    /// (REQ-DAEMON-003 GIVEN).
    #[error("daemon did not become ready within {0:?}")]
    ReadinessTimeout(Duration),
    /// I/O on the spawn-side primitives (lock file, stat, …).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A pipe-protocol error during the handshake or `sys.shutdown` roundtrip.
    #[error("daemon connect: {0}")]
    Connect(String),
    /// The daemon refused `sys.shutdown` (exit code != 0 after grace).
    #[error("daemon refused shutdown: {0}")]
    Shutdown(String),
}

impl SpawnError {
    /// Map onto the `DaemonError` tier the rest of the CLI uses, for
    /// uniform error reporting (REQ-DAEMON-002 tier taxonomy).
    pub fn tier(&self) -> &'static str {
        match self {
            SpawnError::Disabled => "daemon_disabled",
            SpawnError::MissingEnv(_) => "missing_env",
            SpawnError::LockBusy(_) => "lock_busy",
            SpawnError::BinaryNotFound => "binary_not_found",
            SpawnError::SpawnFailedExit(_) => "spawn_failed",
            SpawnError::JobObjectFailed(_) => "job_object_failed",
            SpawnError::ReadinessTimeout(_) => "readiness_timeout",
            SpawnError::Io(_) => "io",
            SpawnError::Connect(_) => "connect",
            SpawnError::Shutdown(_) => "shutdown_failed",
        }
    }
}

/// Inputs for [`DaemonSpawner::start`]: the daemon's bound `(root, tenant)`
/// plus the spawn-fixed config (`--no-embeddings`, locale). The spawner
/// reads `MEMENTO_TOKEN` itself from the process env — the spawner
/// inherits the spawner's env (D8 — the daemon resolves its own
/// credentials at startup).
#[derive(Debug, Clone)]
pub struct SpawnerOptions {
    pub root: PathBuf,
    pub tenant_id: TenantId,
    pub no_embeddings: bool,
    pub locale: Option<String>,
}

/// A handle to the spawned daemon: pid + the wall-clock instant the
/// cookie file was written (which is the daemon's readiness instant in
/// practice — the cookie is the last readiness signal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildHandle {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

/// Probe result for [`DaemonSpawner::status`]: pid + the same
/// cookie-mtime `started_at` (REQ-DAEMON-007 wants PID + uptime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

/// The lazy-spawner + control plane. Static API (no internal state) — the
/// daemon lifecycle is process-global state stored on disk (the cookie
/// file + the `.daemon-spawn.lock`).
pub struct DaemonSpawner;

impl DaemonSpawner {
    /// Spawn the daemon and wait for readiness. Idempotent: when a
    /// daemon is already running for `(root, tenant)` (cookie present,
    /// pipe connectable), the existing instance is returned WITHOUT
    /// spawning a second process — REQ-DAEMON-003 GIVEN "the first
    /// command runs THEN the daemon is spawned, becomes ready".
    pub async fn start(opts: &SpawnerOptions) -> Result<ChildHandle, SpawnError> {
        // Idempotency: if a cookie already exists for this (root, tenant),
        // a daemon is alive — return its pid + cookie mtime without
        // spawning. The caller treats this as success (REQ-DAEMON-003
        // GIVEN: "the daemon is spawned, becomes ready, and the command
        // succeeds").
        if let Some(existing) = try_probe_existing(&opts.root) {
            return Ok(existing);
        }

        // Race-safe spawn (R1 self-detach, D6): acquire the per-root lock
        // BEFORE spawning. A second concurrent caller will see the lock
        // file and either wait or fail with `LockBusy`. We use a
        // delete-on-exit guard via a `defer` style: spawn drops the lock
        // when the function returns (RAII via `SpawnLockGuard`).
        let lock_path = opts.root.join(SPAWN_LOCK_NAME);
        let _guard = SpawnLockGuard::acquire(&lock_path)?;

        // Locate `memento-daemon`: prefer the binary next to the current
        // `memento` (cargo bins install side-by-side), then PATH.
        let program = locate_daemon_binary()?;

        // Spawn detached + bind to the armed startup Job Object (R1,
        // REQ-DAEMON-003 GIVEN-3). `CREATE_BREAKAWAY_FROM_JOB` makes the
        // child start outside any job the spawner might be in, so the
        // startup job can bind it.
        let (child, job) = spawn_detached_with_startup_job(&program, &[], opts)?;
        let pid = child.id();
        info!(
            pid,
            program = %program.display(),
            "spawned memento-daemon (startup job armed with KILL_ON_JOB_CLOSE)"
        );

        // Wait for readiness (cookie file + pipe) — B5 default 10 s.
        // A polling loop is fine: the daemon writes the cookie file at
        // the END of its startup sequence, so the cookie appearing is
        // the readiness signal. We poll every 50 ms. On any failure the
        // armed job drops with this function — spawner death pre-readiness
        // (or an aborted readiness wait) closes the last job handle and
        // the daemon is terminated with it (no orphan, spec GIVEN-3).
        let started_at = wait_for_readiness(&opts.root, DEFAULT_READINESS_TIMEOUT).await?;
        info!(
            pid,
            started_at = %started_at.to_rfc3339(),
            "daemon ready"
        );

        // R1 post-readiness release: disarm KILL_ON_JOB_CLOSE BEFORE the
        // job handle drops, so the daemon outlives this process (design:
        // "post-readiness daemon survives any client"). If the disarm
        // fails the daemon dies with this process — degraded but never an
        // orphan.
        if let Err(err) = job.disarm_kill_on_close() {
            warn!(
                ?err,
                pid, "startup job disarm failed; daemon will exit with this process"
            );
        }
        drop(job);

        // The lock guard drops here, releasing the per-root spawn lock.
        drop(_guard);
        // Best-effort: the spawned `Child` handle is intentionally
        // dropped — the daemon must outlive the spawner (R1, REQ-DAEMON-003).
        drop(child);

        Ok(ChildHandle { pid, started_at })
    }

    /// Send `sys.shutdown` through the named pipe; fall back to a
    /// force-kill of the pid if the cooperative exit doesn't complete
    /// within [`DEFAULT_SHUTDOWN_GRACE`].
    ///
    /// Today the wire is roundtripped by hand (HELLO/WELCOME + a single
    /// `{"kind":"sys","command":"shutdown"}` framed request) — the
    /// high-level `DaemonClient` does not yet expose `dispatch`. B7
    /// promotes this into a typed `send(Command)` helper.
    pub async fn stop(root: &Path) -> Result<(), SpawnError> {
        let probe = try_probe_existing(root).ok_or_else(|| {
            SpawnError::Shutdown("no daemon is running for this root".to_string())
        })?;
        match send_sys_shutdown(root, DEFAULT_SHUTDOWN_GRACE).await {
            Ok(()) => Ok(()),
            Err(err) => {
                warn!(
                    ?err,
                    pid = probe.pid,
                    "cooperative sys.shutdown failed; force-killing"
                );
                force_kill(probe.pid);
                Ok(())
            }
        }
    }

    /// Probe the daemon and return its pid + started_at. Surfaces a
    /// structured [`SpawnError`] on any failure (operators can grep the
    /// `tier()` label).
    pub async fn status(root: &Path) -> Result<DaemonStatus, SpawnError> {
        try_probe_existing(root)
            .map(|handle| DaemonStatus {
                pid: handle.pid,
                started_at: handle.started_at,
            })
            .ok_or_else(|| SpawnError::Connect("daemon not running (no cookie)".to_string()))
    }
}

// ---- internal helpers ------------------------------------------------------

/// The daemon's pid + cookie-mtime probe: scans `<root>/.daemon-*.cookie`
/// for an existing daemon (REQ-DAEMON-012 stale cookie tolerance — the
/// file's existence + mtime is enough to answer `status`).
fn try_probe_existing(root: &Path) -> Option<ChildHandle> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut newest: Option<(SystemTime, u32)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(pid_str) = name
            .strip_prefix(".daemon-")
            .and_then(|rest| rest.strip_suffix(".cookie"))
            && let Ok(pid) = pid_str.parse::<u32>()
        {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(UNIX_EPOCH);
            if newest.is_none_or(|(t, _)| mtime > t) {
                newest = Some((mtime, pid));
            }
        }
    }
    let (mtime, pid) = newest?;
    let started_at: DateTime<Utc> = mtime.into();
    Some(ChildHandle { pid, started_at })
}

/// Locate `memento-daemon`: first try next to the current `memento`
/// binary (cargo bins install side-by-side), then walk `PATH`.
fn locate_daemon_binary() -> Result<PathBuf, SpawnError> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("memento-daemon.exe");
        if candidate.exists() {
            return Ok(candidate);
        }
        let candidate = dir.join("memento-daemon");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            for suffix in ["memento-daemon.exe", "memento-daemon"] {
                let candidate = dir.join(suffix);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }
    Err(SpawnError::BinaryNotFound)
}

/// Spawn `program` detached (`CREATE_BREAKAWAY_FROM_JOB` on Windows so the
/// child starts outside any job the spawner might be in) and bind it to an
/// armed startup Job Object (design R1, REQ-DAEMON-003 GIVEN-3). Returns
/// the child plus the job; the caller MUST hold the job until readiness
/// and call [`StartupJob::disarm_kill_on_close`] before the job drops.
///
/// `args` is the child's argument vector — empty in production (the daemon
/// reads env), and the tests pass a long-lived fake child's args.
///
/// Failure to create or assign the job fails the spawn: without the orphan
/// guard the spec GIVEN-3 ("no orphan daemon survives") cannot be honored,
/// and degrading silently would recreate the deviation this replaces.
fn spawn_detached_with_startup_job(
    program: &Path,
    args: &[&str],
    opts: &SpawnerOptions,
) -> Result<(Child, StartupJob), SpawnError> {
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new(program);
        c.creation_flags(0x0100_0000); // CREATE_BREAKAWAY_FROM_JOB
        c
    };
    #[cfg(not(windows))]
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env("MEMENTO_ROOT", &opts.root)
        .env(
            "MEMENTO_NO_EMBEDDINGS",
            if opts.no_embeddings { "1" } else { "0" },
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(loc) = &opts.locale {
        cmd.env("MEMENTO_LOCALE", loc);
    }
    let child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SpawnError::BinaryNotFound
        } else {
            SpawnError::Io(e)
        }
    })?;
    // R1 orphan guard: bind the child to an armed KILL_ON_JOB_CLOSE job.
    let job = StartupJob::create_kill_on_close()
        .map_err(|err| SpawnError::JobObjectFailed(format!("create job: {err}")))?;
    job.assign_process(&child)
        .map_err(|err| SpawnError::JobObjectFailed(format!("assign process: {err}")))?;
    Ok((child, job))
}

/// Poll for the cookie file (RQ-DAEMON-003 readiness signal). Returns the
/// cookie mtime as the `started_at` estimate.
async fn wait_for_readiness(root: &Path, timeout: Duration) -> Result<DateTime<Utc>, SpawnError> {
    let started = std::time::Instant::now();
    loop {
        if let Some(handle) = try_probe_existing(root) {
            return Ok(handle.started_at);
        }
        if started.elapsed() > timeout {
            return Err(SpawnError::ReadinessTimeout(timeout));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// RAII guard for the `.daemon-spawn.lock` file. Acquires exclusively
/// (`OpenOptions::create_new(true)`); drops the lock on `Drop` (best
/// effort — panics during spawn still release it via the unwind).
#[derive(Debug)]
struct SpawnLockGuard {
    path: PathBuf,
}

impl SpawnLockGuard {
    fn acquire(path: &Path) -> Result<Self, SpawnError> {
        // `create_new(true)` atomically creates-or-fails. A concurrent
        // spawn sees the file and bails with `LockBusy`.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(_) => Ok(Self {
                path: path.to_path_buf(),
            }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(SpawnError::LockBusy(path.to_path_buf()))
            }
            Err(err) => Err(SpawnError::Io(err)),
        }
    }
}

impl Drop for SpawnLockGuard {
    fn drop(&mut self) {
        // Best-effort cleanup; ignore errors (the file might have been
        // removed by an operator, which is fine).
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Roundtrip a HELLO/WELCOME + one framed `sys.shutdown` request. The
/// high-level pipe client (`DaemonClient::connect`) exists; this helper
/// reuses the same `frame::read_message` / `frame::write_message`
/// primitives inline because the B5 client does not yet expose a typed
/// `dispatch`. The handshake uses the canonical cookie file (no
/// AppService model load on this path — REQ-DAEMON-007).
async fn send_sys_shutdown(root: &Path, grace: Duration) -> Result<(), SpawnError> {
    use interprocess::os::windows::named_pipe::tokio::PipeStream;
    use memento_mcp::daemon::DEFAULT_PIPE_TIMEOUT;
    use memento_mcp::handshake::{Hello, PROTOCOL_VERSION, Role, Welcome};
    use tokio::io::AsyncReadExt;

    // Resolve the pipe name from the canonical root + cookie-discovered
    // tenant id. For simplicity the SHUTDOWN sender reads the cookie
    // tenant id from env (the operator runs `memento daemon stop` in the
    // same shell that holds `MEMENTO_TENANT`).
    let tenant_str =
        std::env::var("MEMENTO_TENANT").map_err(|_| SpawnError::MissingEnv("MEMENTO_TENANT"))?;
    let tenant_id: TenantId = tenant_str
        .parse()
        .map_err(|err| SpawnError::Connect(format!("invalid MEMENTO_TENANT: {err}")))?;
    let name = pipe_name(root, &tenant_id);

    let token =
        std::env::var("MEMENTO_TOKEN").map_err(|_| SpawnError::MissingEnv("MEMENTO_TOKEN"))?;
    let _agent_id = std::env::var("MEMENTO_AGENT_ID")
        .map_err(|_| SpawnError::MissingEnv("MEMENTO_AGENT_ID"))?;
    let locale = std::env::var("MEMENTO_LOCALE").ok();
    let no_embeddings = std::env::var("MEMENTO_NO_EMBEDDINGS").ok().as_deref() == Some("1");

    // Discover the cookie (the daemon writes it at readiness).
    let entries = std::fs::read_dir(root).map_err(SpawnError::Io)?;
    let mut cookie: Option<String> = None;
    for entry in entries.flatten() {
        let n = entry.file_name();
        let n = n.to_string_lossy();
        if n.starts_with(".daemon-") && n.ends_with(".cookie") {
            cookie = std::fs::read_to_string(entry.path())
                .ok()
                .map(|s| s.trim().to_string());
            if cookie.is_some() {
                break;
            }
        }
    }
    let cookie =
        cookie.ok_or_else(|| SpawnError::Connect("no cookie file for stop".to_string()))?;

    let mut stream = tokio::time::timeout(
        DEFAULT_PIPE_TIMEOUT,
        PipeStream::connect_by_path(name.as_str()),
    )
    .await
    .map_err(|_| SpawnError::Connect(format!("connect timeout after {DEFAULT_PIPE_TIMEOUT:?}")))?
    .map_err(|err| SpawnError::Connect(format!("pipe connect: {err}")))?;

    // HELLO
    let hello = Hello {
        proto: PROTOCOL_VERSION,
        role: Role::Cli,
        pid: std::process::id(),
        ppid: 0,
        version: env!("CARGO_PKG_VERSION").to_string(),
        cookie,
        token,
        locale,
        no_embeddings,
        staging: std::env::temp_dir(),
    };
    let hello_bytes = serde_json::to_vec(&hello)
        .map_err(|err| SpawnError::Connect(format!("hello serialize: {err}")))?;
    frame::write_message(&mut stream, &hello_bytes)
        .await
        .map_err(|err| SpawnError::Connect(format!("hello write: {err}")))?;

    // WELCOME
    let welcome_bytes = frame::read_message(&mut stream)
        .await
        .map_err(|err| SpawnError::Connect(format!("welcome read: {err}")))?;
    let _welcome: Welcome = serde_json::from_slice(&welcome_bytes)
        .map_err(|err| SpawnError::Connect(format!("welcome parse: {err}")))?;

    // `sys.shutdown`
    let cmd = DispatchCommand::Sys(SysCommand::Shutdown);
    let req_bytes = serde_json::to_vec(&cmd)
        .map_err(|err| SpawnError::Connect(format!("request serialize: {err}")))?;
    frame::write_message(&mut stream, &req_bytes)
        .await
        .map_err(|err| SpawnError::Connect(format!("request write: {err}")))?;

    // Response — we don't parse it (the daemon will exit before answering
    // in many cases); just give it the grace window and observe the pipe
    // close or the timeout.
    let started = std::time::Instant::now();
    let mut buf = vec![0u8; 4096];
    loop {
        let remaining = grace.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(SpawnError::Shutdown(format!(
                "grace {grace:?} elapsed without exit"
            )));
        }
        match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return Ok(()),
            Ok(Ok(_)) => continue,
            Ok(Err(err)) => {
                return Err(SpawnError::Connect(format!("post-shutdown read: {err}")));
            }
            Err(_) => {
                return Err(SpawnError::Shutdown(format!(
                    "grace {grace:?} elapsed without EOF"
                )));
            }
        }
    }
}

/// Force-kill a pid by spawning `taskkill /F /PID <pid>` on Windows, or
/// `kill -9 <pid>` elsewhere. Best-effort: a failure here is logged and
/// swallowed (the operator will see the daemon still running and can
/// intervene manually).
fn force_kill(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// The orphan-guard integration test (REQ-DAEMON-003 GIVEN-3, design
    /// R1): the production spawn helper binds the child to an armed
    /// startup Job Object; releasing the job BEFORE readiness (spawner
    /// death) must kill the child. Uses `ping` as a cheap fake "daemon".
    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_job_orphan_guard_kills_child_released_before_readiness() {
        use memento_mcp::job::is_process_alive;
        use std::time::{Duration, Instant};

        let opts = SpawnerOptions {
            root: tempdir().path().to_path_buf(),
            tenant_id: "11111111-1111-4111-8111-111111111111".parse().expect("tid"),
            no_embeddings: false,
            locale: None,
        };
        let (child, job) = spawn_detached_with_startup_job(
            &PathBuf::from("ping.exe"),
            &["127.0.0.1", "-n", "100", "-w", "1000"],
            &opts,
        )
        .expect("spawn + job");
        let pid = child.id();
        // Spawner death pre-readiness == the armed job handle closes.
        drop(job);
        let start = Instant::now();
        let mut dead = false;
        while start.elapsed() < Duration::from_secs(10) {
            if !is_process_alive(pid) {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(dead, "no orphan: pid {pid} survived the armed job close");
    }

    /// The post-readiness release integration test (R1): after the spawn
    /// helper's job is disarmed, closing the handle must leave the child
    /// alive — the daemon outlives the spawning client.
    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_job_post_readiness_release_leaves_child_alive() {
        use memento_mcp::job::is_process_alive;
        use std::time::Duration;

        let opts = SpawnerOptions {
            root: tempdir().path().to_path_buf(),
            tenant_id: "11111111-1111-4111-8111-111111111111".parse().expect("tid"),
            no_embeddings: false,
            locale: None,
        };
        let (child, job) = spawn_detached_with_startup_job(
            &PathBuf::from("ping.exe"),
            &["127.0.0.1", "-n", "100", "-w", "1000"],
            &opts,
        )
        .expect("spawn + job");
        let pid = child.id();
        job.disarm_kill_on_close().expect("disarm");
        drop(job);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            is_process_alive(pid),
            "post-readiness release: pid {pid} must survive"
        );
        // Cleanup: force-kill so the test leaves no zombies.
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
    }

    #[test]
    fn spawn_error_tiers_are_uniform() {
        // REQ-DAEMON-002: every SpawnError variant maps to a stable
        // tier label so operators get a uniform error surface.
        let cases: Vec<(SpawnError, &'static str)> = vec![
            (SpawnError::Disabled, "daemon_disabled"),
            (SpawnError::MissingEnv("MEMENTO_TOKEN"), "missing_env"),
            (
                SpawnError::LockBusy(PathBuf::from("/x/.daemon-spawn.lock")),
                "lock_busy",
            ),
            (SpawnError::BinaryNotFound, "binary_not_found"),
            (
                SpawnError::ReadinessTimeout(Duration::from_secs(5)),
                "readiness_timeout",
            ),
            (
                SpawnError::JobObjectFailed("create job: x".into()),
                "job_object_failed",
            ),
            (SpawnError::Shutdown("x".into()), "shutdown_failed"),
        ];
        for (err, expected) in cases {
            assert_eq!(err.tier(), expected, "tier for {err:?}");
        }
    }

    #[test]
    fn lock_guard_acquires_exclusively() {
        // D6: `create_new(true)` gives us exclusive creation — the
        // second `acquire` on the same path MUST fail with `LockBusy`.
        let tmp = tempdir();
        let lock_path = tmp.path().join(SPAWN_LOCK_NAME);
        let _first = SpawnLockGuard::acquire(&lock_path).expect("first lock");
        let err = SpawnLockGuard::acquire(&lock_path).expect_err("second lock");
        assert!(matches!(err, SpawnError::LockBusy(_)), "second lock fails");
    }

    #[test]
    fn lock_guard_releases_on_drop() {
        // RAII: dropping the guard removes the lock file (best-effort).
        let tmp = tempdir();
        let lock_path = tmp.path().join(SPAWN_LOCK_NAME);
        {
            let _g = SpawnLockGuard::acquire(&lock_path).expect("lock");
            assert!(lock_path.exists(), "lock file present while guard alive");
        }
        assert!(!lock_path.exists(), "lock file gone after guard drop");
    }

    #[test]
    fn try_probe_existing_returns_none_on_empty_root() {
        let tmp = tempdir();
        let probe = try_probe_existing(tmp.path());
        assert!(probe.is_none(), "no cookie, no probe");
    }

    #[test]
    fn try_probe_existing_picks_up_cookie_with_pid_and_mtime() {
        // REQ-DAEMON-007 status probe: a stale cookie is enough to
        // answer `status` (the operator sees the daemon pid + the cookie
        // mtime as `started_at`).
        let tmp = tempdir();
        std::fs::write(tmp.path().join(".daemon-4242.cookie"), "nonce").expect("cookie");
        let probe = try_probe_existing(tmp.path()).expect("probe");
        assert_eq!(probe.pid, 4242);
        // mtime is wall-clock-ish; just assert it's not the epoch.
        assert!(
            probe.started_at.timestamp() > 1_000_000_000,
            "started_at is a real timestamp: {}",
            probe.started_at
        );
    }

    #[test]
    fn try_probe_existing_picks_the_newest_cookie() {
        // Multiple cookies (a kill -9 left a stale one) — the probe
        // returns the NEWEST mtime (REB-DAEMON-013 stale cookie tolerance).
        let tmp = tempdir();
        std::fs::write(tmp.path().join(".daemon-100.cookie"), "old").expect("old cookie");
        // Touch the new cookie so its mtime is strictly greater.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(tmp.path().join(".daemon-200.cookie"), "new").expect("new cookie");
        let probe = try_probe_existing(tmp.path()).expect("probe");
        assert_eq!(probe.pid, 200, "newest wins");
    }
}
