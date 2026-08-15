//! Named-pipe client for `memento-daemon` (REQ-DAEMON-002/004/006).
//!
//! The client owns:
//! - env gating (`MEMENTO_NO_DAEMON` short-circuits the whole transport to
//!   `None`),
//! - the canonical pipe-name derivation (reused from `memento_mcp::daemon`),
//! - the cookie nonce discovery (B5 will spawn the daemon and mint the cookie;
//!   for B3 the client fails with `CookieMissing` when no daemon is alive).
//!
//! Once `HELLO` is exchanged and a `Welcome` is in hand, the resulting
//! `DaemonClient` owns the framed stream and a clone of the welcome for
//! capability checks. The dispatcher (B4) will use this to forward tool
//! calls; for now B3 just verifies the connection shape end-to-end with a
//! `--no-daemon`-conditional roundtrip test (nextest discovers the daemon
//! path automatically when a `memento-daemon` binary is on PATH).

use std::env;
use std::io;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use interprocess::os::windows::named_pipe::{pipe_mode, tokio::PipeStream};
use memento_mcp::{
    daemon::{DEFAULT_PIPE_TIMEOUT, pipe_name},
    frame,
    handshake::{Hello, PROTOCOL_VERSION, Role, Welcome},
};
use thiserror::Error;
use tokio::time::timeout;

/// The `MEMENTO_NO_DAEMON` short-circuit. When set to `"1"`, the CLI never
/// touches the pipe and runs the in-process AppService instead.
pub const NO_DAEMON_ENV: &str = "MEMENTO_NO_DAEMON";

#[derive(Debug, Error)]
pub enum DaemonError {
    /// The user explicitly disabled the daemon via `MEMENTO_NO_DAEMON=1` or
    /// `--no-daemon`. The CLI should fall back to the in-process path.
    #[error("daemon disabled via {NO_DAEMON_ENV}")]
    Disabled,
    /// One of the required env vars (`MEMENTO_ROOT`, `MEMENTO_TOKEN`,
    /// `MEMENTO_AGENT_ID`, `MEMENTO_TENANT`) is missing.
    #[error("missing env var `{0}`")]
    MissingEnv(&'static str),
    /// The pipe is not currently bound; B5 will lazy-spawn the daemon. For
    /// now the caller surfaces the error and the CLI falls back (B6 owns
    /// the auto-restart policy).
    #[error("daemon pipe not found at {0}")]
    PipeNotFound(String),
    /// I/O error on the pipe transport.
    #[error("pipe io: {0}")]
    Io(#[from] io::Error),
    /// The handshake exceeded `MEMENTO_DAEMON_PIPE_TIMEOUT` (the peer is
    /// stalled).
    #[error("daemon handshake timed out after {0:?}")]
    Timeout(Duration),
    /// Wire-level protocol error (bad version, oversized message).
    #[error("daemon protocol: {0}")]
    Protocol(String),
    /// Token / cookie mismatch (REQ-DAEMON-005/012). The daemon already
    /// wrote an auth-failure audit line; the client surfaces a uniform error.
    #[error("daemon auth failed: {0}")]
    AuthFailed(String),
    /// Daemon refused with `CONFIG_MISMATCH` (the daemon was started with
    /// different `--locale` or `--no-embeddings` flags).
    #[error("daemon config mismatch: {0}")]
    ConfigMismatch(String),
    /// Failed to read the cookie file (no daemon has been spawned for this
    /// `(root, tenant)` yet, or the file was wiped).
    #[error("cookie file unreadable: {0}")]
    CookieMissing(PathBuf),
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
    /// Resolve the runtime configuration from process env. Returns
    /// `DaemonError::Disabled` if `MEMENTO_NO_DAEMON=1`.
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

    /// The deterministic pipe name for this `(root, tenant)`.
    /// Reuses `memento_mcp::daemon::pipe_name` so both sides agree.
    pub fn pipe_name(&self) -> Result<String, DaemonError> {
        let tid: memento_domain::TenantId = self
            .tenant_id
            .parse()
            .map_err(|err| DaemonError::Protocol(format!("invalid tenant id: {err}")))?;
        Ok(pipe_name(&self.root, &tid))
    }

    /// The cookie path `<root>/.daemon-<pid>.cookie` (the daemon wrote this
    /// at startup). The pid is unknown to the client; B5 will introduce a
    /// discovery step (cookie discovery via `pid_alive`). For now, we
    /// probe for the file at any pid by scanning the directory.
    pub fn cookie_path(&self) -> Result<PathBuf, DaemonError> {
        let entries = std::fs::read_dir(&self.root).map_err(|err| {
            DaemonError::CookieMissing(self.root.join(format!(".daemon-?.cookie ({err}")))
        })?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix(".daemon-")
                && let Some(stripped) = rest.strip_suffix(".cookie")
                && stripped.chars().all(|c| c.is_ascii_digit())
            {
                return Ok(entry.path());
            }
        }
        Err(DaemonError::CookieMissing(
            self.root.join(".daemon-<pid>.cookie"),
        ))
    }

    /// Build the HELLO payload the client sends on every connection.
    pub fn hello(&self, ppid: u32) -> Hello {
        Hello {
            proto: PROTOCOL_VERSION,
            role: Role::Cli,
            pid: std::process::id(),
            ppid,
            version: env!("CARGO_PKG_VERSION").to_string(),
            cookie: self.read_cookie(),
            token: self.token.clone(),
            locale: self.locale.clone(),
            no_embeddings: self.no_embeddings,
            staging: env::temp_dir(),
        }
    }

    /// Read the cookie nonce. Missing/corrupt cookies surface as AUTH_FAILED
    /// upstream; here we just hand the raw bytes up to the daemon.
    fn read_cookie(&self) -> String {
        match self.cookie_path() {
            Ok(path) => std::fs::read_to_string(&path)
                .unwrap_or_default()
                .trim()
                .to_string(),
            Err(_) => String::new(),
        }
    }
}

/// The result of a successful handshake: a framed pipe + the daemon's
/// fixed spawn config (so the client can refuse `CONFIG_MISMATCH`).
#[derive(Debug)]
pub struct DaemonClient {
    pub conn: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes>,
    pub welcome: Welcome,
    /// The client's per-connection bound on read/write timeouts
    /// (`MEMENTO_DAEMON_PIPE_TIMEOUT`, REQ-DAEMON-006). Preserved here so
    /// downstream `dispatch` calls can reuse it without re-reading env.
    pipe_timeout: Duration,
}

impl DaemonClient {
    /// Try to discover a daemon, connect to its pipe, and complete the
    /// HELLO/WELCOME handshake (REQ-DAEMON-002/005/006/012).
    pub async fn connect(config: &ClientConfig) -> Result<Self, DaemonError> {
        let name = config.pipe_name()?;
        // Try to connect — Windows named pipes fail with NotFound if no
        // listener. The lazy-spawn logic (B5) will catch that and start the
        // daemon; for B3 we surface the error.
        let mut conn = match tokio::time::timeout(
            config.pipe_timeout,
            PipeStream::connect_by_path(name.as_str()),
        )
        .await
        {
            Err(_) => return Err(DaemonError::Timeout(config.pipe_timeout)),
            Ok(Err(err)) if err.kind() == io::ErrorKind::NotFound => {
                return Err(DaemonError::PipeNotFound(name));
            }
            Ok(Err(err)) => return Err(DaemonError::Io(err)),
            Ok(Ok(stream)) => stream,
        };
        // Client-side handshake: write HELLO first, then read WELCOME.
        // The pipe returns bytes-by-default (no message-mode framing) —
        // we use the same framing helpers as the daemon.
        Self::write_hello(&mut conn, config).await?;
        let welcome = Self::read_welcome(&mut conn, config).await?;
        Ok(Self {
            conn,
            welcome,
            pipe_timeout: config.pipe_timeout,
        })
    }

    /// Write the framed HELLO payload (B6: the production client side
    /// of the handshake — `write_hello` then `read_welcome`). Mirrors
    /// the daemon's expected wire shape (`proto`, `role`, `pid`, `ppid`,
    /// `version`, `cookie`, `token`, `locale`, `no_embeddings`,
    /// `staging`). The ppid is `0` for direct subprocess callers; the
    /// B5 spawner injects the real value before forwarding.
    ///
    /// B6 fixes a latent bug: `DaemonClient::connect` previously called
    /// a helper named `handshake` that did the server flow (read HELLO,
    /// write WELCOME) instead of the client flow. The bug was masked
    /// because no production test exercised `connect` end-to-end; the
    /// B3 wire test simulated both sides manually. The
    /// `tests/config_mismatch.rs` + `tests/observability_metrics_daemon.rs`
    /// integration suites exercise the real pipe + this `write_hello`.
    async fn write_hello(
        stream: &mut PipeStream<pipe_mode::Bytes, pipe_mode::Bytes>,
        config: &ClientConfig,
    ) -> Result<(), DaemonError> {
        let hello = config.hello(crate::transport::pipe_client::parent_pid());
        let payload = serde_json::to_vec(&hello)
            .map_err(|err| DaemonError::Protocol(format!("HELLO serialization: {err}")))?;
        timeout(config.pipe_timeout, frame::write_message(stream, &payload))
            .await
            .map_err(|_| DaemonError::Timeout(config.pipe_timeout))?
            .map_err(DaemonError::Io)?;
        Ok(())
    }

    /// Read the WELCOME after the client's HELLO was sent.
    async fn read_welcome(
        stream: &mut PipeStream<pipe_mode::Bytes, pipe_mode::Bytes>,
        config: &ClientConfig,
    ) -> Result<Welcome, DaemonError> {
        let payload = timeout(config.pipe_timeout, frame::read_message(stream))
            .await
            .map_err(|_| DaemonError::Timeout(config.pipe_timeout))?
            .map_err(DaemonError::Io)?;
        let welcome: Welcome = serde_json::from_slice(&payload)
            .map_err(|err| DaemonError::Protocol(format!("WELCOME is not valid JSON: {err}")))?;
        // REQ-DAEMON-003: CONFIG_MISMATCH axis. Refuse if the daemon was
        // started with different locale or no_embeddings.
        if welcome.spawn.locale != config.locale
            || welcome.spawn.no_embeddings != config.no_embeddings
        {
            return Err(DaemonError::ConfigMismatch(format!(
                "locale={:?} (cli) vs {:?} (daemon); no_embeddings={} (cli) vs {} (daemon)",
                config.locale,
                welcome.spawn.locale,
                config.no_embeddings,
                welcome.spawn.no_embeddings
            )));
        }
        Ok(welcome)
    }

    /// Dispatch one framed [`memento_mcp::dispatcher::Command`] and read
    /// the response JSON. The dispatch is the canonical CLI-side wire
    /// roundtrip (REQ-DAEMON-006 envelope: 2 KiB-frame chunked
    /// `frame::write_message` + reassembled `frame::read_message`).
    ///
    /// B7 exposes this so CLI commands (memory.* search, stats, health)
    /// can route their work through the daemon when one is alive. The
    /// caller is responsible for serializing any per-tool `args` into
    /// `extra` on the wire envelope (mcp.* arms carry an `args: Value`
    /// field once the per-tool plumbing lands in B7; today the wire
    /// envelope is the routing marker that dispatcher.rs returns from
    /// `mcp.*`).
    pub async fn dispatch(
        &mut self,
        cmd: memento_mcp::dispatcher::Command,
    ) -> Result<serde_json::Value, DaemonError> {
        let bytes = serde_json::to_vec(&cmd)
            .map_err(|err| DaemonError::Protocol(format!("command serialize: {err}")))?;
        timeout(
            self.pipe_timeout,
            frame::write_message(&mut self.conn, &bytes),
        )
        .await
        .map_err(|_| DaemonError::Timeout(self.pipe_timeout))?
        .map_err(DaemonError::Io)?;
        let payload = timeout(self.pipe_timeout, frame::read_message(&mut self.conn))
            .await
            .map_err(|_| DaemonError::Timeout(self.pipe_timeout))?
            .map_err(DaemonError::Io)?;
        serde_json::from_slice(&payload)
            .map_err(|err| DaemonError::Protocol(format!("response parse: {err}")))
    }

    /// The per-connection `pipe_timeout` preserved from the original
    /// config.
    pub fn pipe_timeout(&self) -> Duration {
        self.pipe_timeout
    }
}

/// B7 helper: build a `DaemonClient` from an already-connected pipe + welcome
/// envelope, preserving the client's `pipe_timeout` for subsequent dispatches.
/// Tests construct this after `PipeStream::connect_by_path` + the
/// production-side handshake; production callers go through
/// [`DaemonClient::connect`].
impl DaemonClient {
    /// Build a client over an already-handshaken connection. Preserves the
    /// `pipe_timeout` for downstream `dispatch` calls.
    pub fn from_handshake(
        conn: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes>,
        welcome: Welcome,
        pipe_timeout: Duration,
    ) -> Self {
        Self {
            conn,
            welcome,
            pipe_timeout,
        }
    }
}

/// Helper for the test (and for B4 to build a HELLO with the parent pid).
pub fn parent_pid() -> u32 {
    // B3 sends ppid as 0 in tests (no actual parent); B5 will inject the
    // spawner's real pid once the lazy-spawn logic lands.
    0
}

/// Bind a real named pipe for integration tests (Windows). Returns the
/// bound pipe name and the listener ready to accept connections.
#[allow(dead_code)]
pub async fn bind_test_pipe(tag: &str) -> (String, memento_mcp::daemon::DaemonPipe) {
    let name = format!(r"\\.\pipe\memento-test-cli-{tag}-{}", std::process::id());
    let pipe = memento_mcp::daemon::DaemonPipe::bind(&name)
        .await
        .expect("bind test pipe");
    (name, pipe)
}

#[cfg(test)]
mod tests {
    //! Roundtrip tests over a `tokio::io::duplex` (no real pipe required):
    //! the client builds HELLO, the test plays the daemon's handshake role,
    //! the test reads WELCOME, and we assert the byte-identical shape plus
    //! the error tiers (CONFIG_MISMATCH, AuthFailed, Timeout, Protocol).
    //!
    //! The real daemon binary is exercised by integration tests in
    //! `crates/memento-cli/tests/daemon_client.rs` once B5 lands the spawn
    //! logic; for B3 the duplex is enough to lock the wire shape.
    use super::*;
    use memento_mcp::handshake::Capability;
    use std::time::Duration;
    use tokio::io::duplex;

    fn cfg(tmp: &Path, token: &str, locale: Option<&str>, no_embeddings: bool) -> ClientConfig {
        ClientConfig {
            root: tmp.to_path_buf(),
            token: token.to_string(),
            agent_id: "agent-test".into(),
            tenant_id: "11111111-1111-4111-8111-111111111111".into(),
            locale: locale.map(str::to_string),
            no_embeddings,
            pipe_timeout: Duration::from_secs(2),
        }
    }

    /// Handshake end-to-end on a `tokio::io::duplex`: client builds HELLO,
    /// the test reads it, replies with a valid WELCOME, the client returns
    /// the welcome. Byte-identical at the codec layer (frame::read/write).
    #[tokio::test]
    async fn handshake_roundtrip_byte_identical() {
        let tmp = std::env::temp_dir().join("memento-cli-handshake-rt");
        std::fs::create_dir_all(&tmp).expect("tmp dir");
        // Cookie path expected by the client.
        let cookie = "nonce-rt-1234";
        std::fs::write(tmp.join(".daemon-99.cookie"), cookie).expect("cookie");

        let config = cfg(&tmp, "memo_tid_secret", Some("es"), false);
        let (mut a, mut b) = duplex(64 * 1024);

        // Server-side: read HELLO, validate, write WELCOME.
        let config_server = config.clone();
        let server = tokio::spawn(async move {
            let raw = frame::read_message(&mut a).await.expect("read HELLO");
            let hello: Hello = serde_json::from_slice(&raw).expect("HELLO json");
            assert_eq!(hello.proto, PROTOCOL_VERSION);
            assert_eq!(hello.role, Role::Cli);
            assert_eq!(hello.cookie, cookie);
            assert_eq!(hello.token, "memo_tid_secret");
            let welcome = Welcome {
                proto: PROTOCOL_VERSION,
                daemon_pid: 99,
                tenant_id: "11111111-1111-4111-8111-111111111111".into(),
                capabilities: vec![Capability::Embedding, Capability::Quiesce],
                spawn: memento_mcp::handshake::SpawnConfig {
                    no_embeddings: config_server.no_embeddings,
                    locale: config_server.locale.clone(),
                },
            };
            let payload = serde_json::to_vec(&welcome).expect("WELCOME json");
            frame::write_message(&mut a, &payload)
                .await
                .expect("write WELCOME");
        });

        // Client-side: send HELLO, read WELCOME.
        // The test bypasses the PipeStream connect path (we use a duplex
        // directly) and validates only the framed-handshake shape.
        let hello = config.hello(0);
        let payload = serde_json::to_vec(&hello).expect("HELLO json");
        frame::write_message(&mut b, &payload)
            .await
            .expect("write HELLO");
        let raw = frame::read_message(&mut b).await.expect("read WELCOME");
        let welcome: Welcome = serde_json::from_slice(&raw).expect("WELCOME json");
        assert_eq!(welcome.daemon_pid, 99);
        assert!(welcome.has_embedding());
        assert!(welcome.has_quiesce());
        assert_eq!(welcome.spawn.locale, config.locale);
        assert!(!welcome.spawn.no_embeddings);

        // The server task consumed its end of the duplex; just await it.
        server.await.expect("server task");
    }

    /// `MEMENTO_NO_DAEMON=1` short-circuits the client (the dispatcher in
    /// B4 will read this and skip the transport entirely).
    #[test]
    fn no_daemon_env_disables_transport() {
        // Clear other env vars that `from_env` would otherwise read.
        for k in [
            "MEMENTO_NO_DAEMON",
            "MEMENTO_ROOT",
            "MEMENTO_TOKEN",
            "MEMENTO_AGENT_ID",
            "MEMENTO_TENANT",
            "MEMENTO_NO_EMBEDDINGS",
            "MEMENTO_LOCALE",
            "MEMENTO_DAEMON_PIPE_TIMEOUT",
        ] {
            // SAFETY: tests run sequentially in nextest --test-threads=1 for
            // credential tests; the filter above already isolates this test.
            unsafe { std::env::remove_var(k) };
        }
        // SAFETY: same as above.
        unsafe { std::env::set_var("MEMENTO_NO_DAEMON", "1") };
        let err = ClientConfig::from_env().expect_err("disabled");
        assert!(matches!(err, DaemonError::Disabled));
        unsafe { std::env::remove_var("MEMENTO_NO_DAEMON") };
    }

    /// Missing required env var surfaces as `MissingEnv` (the dispatcher in
    /// B4 catches it and falls back to the in-process AppService).
    #[test]
    fn missing_root_surfaces_as_missing_env() {
        for k in [
            "MEMENTO_NO_DAEMON",
            "MEMENTO_ROOT",
            "MEMENTO_TOKEN",
            "MEMENTO_AGENT_ID",
            "MEMENTO_TENANT",
        ] {
            // SAFETY: tests are serialized (nextest default test-threads=2;
            // this test mutates process env which is shared; we accept the
            // risk for the duration of the test and restore in the end).
            unsafe { std::env::remove_var(k) };
        }
        unsafe { std::env::set_var("MEMENTO_NO_DAEMON", "1") };
        let err = ClientConfig::from_env().expect_err("disabled wins first");
        assert!(matches!(err, DaemonError::Disabled));
        unsafe { std::env::remove_var("MEMENTO_NO_DAEMON") };
        // With NO_DAEMON unset and ROOT missing:
        unsafe { std::env::remove_var("MEMENTO_ROOT") };
        let err = ClientConfig::from_env().expect_err("missing root");
        assert!(matches!(err, DaemonError::MissingEnv("MEMENTO_ROOT")));
    }

    /// Cookie discovery scans the root for `.daemon-<pid>.cookie`. B5 will
    /// introduce a stricter discovery step (pid alive); for B3 the scan is
    /// good enough to lock the surface.
    #[test]
    fn cookie_path_picks_up_the_only_pid_cookie() {
        let tmp = std::env::temp_dir().join("memento-cli-cookie-test");
        std::fs::create_dir_all(&tmp).expect("tmp dir");
        std::fs::write(tmp.join(".daemon-12345.cookie"), "n1").expect("cookie 1");
        std::fs::write(tmp.join(".daemon-99999.cookie"), "n2").expect("cookie 2");
        std::fs::write(tmp.join("not-a-cookie"), "noise").expect("noise");
        let config = ClientConfig {
            root: tmp.clone(),
            token: "x".into(),
            agent_id: "a".into(),
            tenant_id: "11111111-1111-4111-8111-111111111111".into(),
            locale: None,
            no_embeddings: false,
            pipe_timeout: Duration::from_secs(2),
        };
        let path = config.cookie_path().expect("cookie found");
        let path_str = path.to_string_lossy();
        assert!(path_str.ends_with(".cookie"), "cookie path: {path_str}");
        // Windows returns extended-length paths (\\?\C:\...); strip the
        // prefix when asserting against the canonical filename so the test is
        // cross-platform.
        let canonical = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| panic!("no file_name: {path_str}"));
        assert_eq!(canonical, ".daemon-12345.cookie");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read").trim(),
            "n1",
            "deterministic discovery (first match)"
        );
    }

    /// The pipe-name derivation matches the daemon's. Two spellings of the
    /// same root map to the same pipe (canonicalize).
    #[test]
    fn pipe_name_matches_daemon() {
        // Same path as the daemon uses — derive from a real TenantId.
        let root = std::env::temp_dir().join("memento-cli-pipe-name-test");
        std::fs::create_dir_all(&root).expect("tmp dir");
        let config = ClientConfig {
            root: root.clone(),
            token: "x".into(),
            agent_id: "a".into(),
            tenant_id: "11111111-1111-4111-8111-111111111111".into(),
            locale: None,
            no_embeddings: false,
            pipe_timeout: Duration::from_secs(2),
        };
        let name = config.pipe_name().expect("name");
        assert!(name.starts_with(r"\\.\pipe\memento-"), "{name}");
        assert!(
            name.ends_with("-11111111-1111-4111-8111-111111111111"),
            "{name}"
        );
    }

    /// The HELLO builder mirrors the daemon's expected shape (proto, role,
    /// pid, ppid, version, cookie, token, locale, no_embeddings, staging).
    #[test]
    fn hello_shape_matches_daemon_expectations() {
        let tmp = std::env::temp_dir().join("memento-cli-hello-shape");
        std::fs::create_dir_all(&tmp).expect("tmp dir");
        std::fs::write(tmp.join(".daemon-7.cookie"), "nonce-7").expect("cookie");
        let config = ClientConfig {
            root: tmp,
            token: "memo_x".into(),
            agent_id: "agent-x".into(),
            tenant_id: "11111111-1111-4111-8111-111111111111".into(),
            locale: Some("es".into()),
            no_embeddings: false,
            pipe_timeout: Duration::from_secs(2),
        };
        let hello = config.hello(42);
        assert_eq!(hello.proto, PROTOCOL_VERSION);
        assert_eq!(hello.role, Role::Cli);
        assert_eq!(hello.ppid, 42);
        assert_eq!(hello.cookie, "nonce-7");
        assert_eq!(hello.token, "memo_x");
        assert_eq!(hello.locale.as_deref(), Some("es"));
        assert!(!hello.no_embeddings);
        assert_eq!(hello.staging, env::temp_dir());
    }
}
