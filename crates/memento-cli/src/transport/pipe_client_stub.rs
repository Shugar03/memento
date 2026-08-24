//! Non-Windows stub for the named-pipe daemon client (REQ-DAEMON-001).
//!
//! Daemon mode is Windows-first; on other targets the CLI always falls back
//! to the in-process AppService path.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use memento_mcp::{
    handshake::{Hello, PROTOCOL_VERSION, Role},
    pipe_naming::{DEFAULT_PIPE_TIMEOUT, pipe_name},
};
use thiserror::Error;

/// The `MEMENTO_NO_DAEMON` short-circuit. When set to `"1"`, the CLI never
/// touches the pipe and runs the in-process AppService instead.
pub const NO_DAEMON_ENV: &str = "MEMENTO_NO_DAEMON";

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("daemon disabled via {NO_DAEMON_ENV}")]
    Disabled,
    #[error("missing env var `{0}`")]
    MissingEnv(&'static str),
    #[error("daemon pipe not found at {0}")]
    PipeNotFound(String),
    #[error("pipe io: {0}")]
    Io(#[from] std::io::Error),
    #[error("daemon handshake timed out after {0:?}")]
    Timeout(Duration),
    #[error("daemon protocol: {0}")]
    Protocol(String),
    #[error("daemon auth failed: {0}")]
    AuthFailed(String),
    #[error("daemon config mismatch: {0}")]
    ConfigMismatch(String),
    #[error("cookie file unreadable: {0}")]
    CookieMissing(PathBuf),
    #[error("daemon pipe broken: {0}")]
    PipeBroken(String),
}

/// Runtime configuration resolved from env (mirrors the daemon's gate).
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub root: PathBuf,
    pub token: String,
    pub agent_id: String,
    pub tenant_id: String,
    pub locale: Option<String>,
    pub no_embeddings: bool,
    pub pipe_timeout: Duration,
}

impl ClientConfig {
    pub fn from_env() -> Result<Self, DaemonError> {
        if env::var(NO_DAEMON_ENV).ok().as_deref() == Some("1") {
            return Err(DaemonError::Disabled);
        }
        let root = env::var("MEMENTO_ROOT").map_err(|_| DaemonError::MissingEnv("MEMENTO_ROOT"))?;
        let token =
            env::var("MEMENTO_TOKEN").map_err(|_| DaemonError::MissingEnv("MEMENTO_TOKEN"))?;
        let agent_id = env::var("MEMENTO_AGENT_ID")
            .map_err(|_| DaemonError::MissingEnv("MEMENTO_AGENT_ID"))?;
        let tenant_id =
            env::var("MEMENTO_TENANT").map_err(|_| DaemonError::MissingEnv("MEMENTO_TENANT"))?;
        let no_embeddings = env::var("MEMENTO_NO_EMBEDDINGS")
            .map(|v| v == "1")
            .unwrap_or(false);
        let locale = env::var("MEMENTO_LOCALE").ok();
        let pipe_timeout_secs: f64 = env::var("MEMENTO_DAEMON_PIPE_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_PIPE_TIMEOUT.as_secs_f64());
        Ok(ClientConfig {
            root: PathBuf::from(root),
            token,
            agent_id,
            tenant_id,
            locale,
            no_embeddings,
            pipe_timeout: Duration::from_secs_f64(pipe_timeout_secs),
        })
    }

    pub fn pipe_name(&self) -> Result<String, DaemonError> {
        let tid: memento_domain::TenantId = self
            .tenant_id
            .parse()
            .map_err(|err| DaemonError::Protocol(format!("invalid tenant id: {err}")))?;
        Ok(pipe_name(&self.root, &tid))
    }

    pub fn cookie_path(&self) -> Result<PathBuf, DaemonError> {
        Err(DaemonError::CookieMissing(
            self.root.join(".daemon-<pid>.cookie"),
        ))
    }

    pub fn hello(&self, ppid: u32) -> Hello {
        Hello {
            proto: PROTOCOL_VERSION,
            role: Role::Cli,
            pid: std::process::id(),
            ppid,
            version: env!("CARGO_PKG_VERSION").to_string(),
            cookie: String::new(),
            token: self.token.clone(),
            locale: self.locale.clone(),
            no_embeddings: self.no_embeddings,
            staging: env::temp_dir(),
        }
    }
}

/// Stub client — never successfully constructed on non-Windows targets.
#[derive(Debug)]
pub struct DaemonClient {
    pub welcome: memento_mcp::handshake::Welcome,
}

impl DaemonClient {
    pub async fn connect(config: &ClientConfig) -> Result<Self, DaemonError> {
        let name = config.pipe_name()?;
        Err(DaemonError::PipeNotFound(name))
    }
}

pub fn parent_pid() -> u32 {
    0
}
