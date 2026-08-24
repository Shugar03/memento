//! Non-Windows stub for the daemon spawner (REQ-DAEMON-001).
//!
//! Process spawn and named-pipe shutdown are Windows-only; cookie probing
//! for `status` still works so operators can inspect leftover state.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use memento_domain::TenantId;
use thiserror::Error;

pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
pub const DEFAULT_READINESS_TIMEOUT: Duration = Duration::from_secs(10);
pub const SPAWN_LOCK_NAME: &str = ".daemon-spawn.lock";

#[derive(Debug, Error)]
pub enum SpawnError {
    #[error("MEMENTO_NO_DAEMON=1; daemon mode disabled")]
    Disabled,
    #[error("missing env var `{0}`")]
    MissingEnv(&'static str),
    #[error("another spawn is in progress (lock file busy at {0})")]
    LockBusy(PathBuf),
    #[error("`memento-daemon` binary not found on PATH or next to memento")]
    BinaryNotFound,
    #[error("daemon exited before becoming ready (status: {0:?})")]
    SpawnFailedExit(std::process::ExitStatus),
    #[error("startup job object failed: {0}")]
    JobObjectFailed(String),
    #[error("daemon did not become ready within {0:?}")]
    ReadinessTimeout(Duration),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("daemon connect: {0}")]
    Connect(String),
    #[error("daemon refused shutdown: {0}")]
    Shutdown(String),
}

impl SpawnError {
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

#[derive(Debug, Clone)]
pub struct SpawnerOptions {
    pub root: PathBuf,
    pub tenant_id: TenantId,
    pub no_embeddings: bool,
    pub locale: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildHandle {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

pub struct DaemonSpawner;

impl DaemonSpawner {
    pub async fn start(_opts: &SpawnerOptions) -> Result<ChildHandle, SpawnError> {
        Err(SpawnError::Disabled)
    }

    pub async fn stop(_root: &Path) -> Result<(), SpawnError> {
        Err(SpawnError::Disabled)
    }

    pub async fn status(root: &Path) -> Result<DaemonStatus, SpawnError> {
        try_probe_existing(root)
            .map(|handle| DaemonStatus {
                pid: handle.pid,
                started_at: handle.started_at,
            })
            .ok_or_else(|| SpawnError::Connect("daemon not running (no cookie)".to_string()))
    }
}

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
