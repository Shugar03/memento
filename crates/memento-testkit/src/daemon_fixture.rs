//! In-process daemon fixture for integration tests (daemon-persistent B7,
//! design D10/R4/H4).
//!
//! [`DaemonFixture`] runs the real `memento_mcp::daemon` (pipe bind) +
//! `memento_mcp::dispatcher` (B5 sys.* + B7 mcp.* bodies) inside the test
//! process, bound to a real Windows named pipe on `tempdir`. External CLI
//! children (and external MCP stdio proxies) can connect to that pipe just
//! like a production daemon. Tests get a typed handle with the same
//! helpers production uses (`cookie_path`, `pid`, `tenant_id`,
//! `connect_client`, `dispatch`).
//!
//! ## Design contract
//!
//! * **One fixture per temp_root.** Two `DaemonFixture`s over the same
//!   `root` race on the pipe name (`\\.\pipe\memento-<hash>-<tid>`) and
//!   the cookie path (`.daemon-<pid>.cookie`). Tests that need a fresh
//!   store must use a fresh temp dir.
//! * **In-process ≠ free.** The fixture still loads the real parse
//!   boundary and the real `AppService`; tests that want a lighter
//!   harness should use a `tokio::io::duplex` + hand-rolled handshake.
//! * **Drop kills the daemon.** Closing the fixture aborts the accept
//!   loop and removes the cookie file. The fixture does NOT force-kill by
//!   pid (there is no separate process — this is in-process).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use interprocess::os::windows::named_pipe::tokio::PipeStream;
use interprocess::os::windows::named_pipe::pipe_mode;
use memento_application::{AppService, SystemClock};
use memento_domain::{DomainError, TenantContext, TenantId};
use memento_mcp::daemon::{pipe_name, server_handshake_with_timeout, DaemonAuth, DaemonPipe};
use memento_mcp::dispatcher::{dispatch_command_with_state, Command, DaemonState};
use memento_mcp::frame;
use memento_mcp::handshake::{Hello, Welcome, PROTOCOL_VERSION, Role};
use memento_parse::ParseService;
use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
use memento_ports::{EmbedPort, ParsePort};
use crate::StubEmbedPort;
use rand_core::{OsRng, RngCore};
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// What every daemon fixture needs from the test: the storage root + the
/// tenant context (the daemon binds itself to one tenant, REQ-TA-001).
pub struct DaemonFixtureOptions {
    /// The fixture's `tempfile::TempDir` path (production layout root).
    pub root: PathBuf,
    /// The bound tenant (every `TenantContext::new_for_tests` is fine).
    pub ctx: TenantContext,
    /// The token the test will present in the HELLO (the daemon compares
    /// raw; the fixture mirrors production by validating the byte at
    /// handshake).
    pub token: String,
    /// `--no-embeddings` flag, forwarded to the daemon's `WELCOME.spawn`.
    pub no_embeddings: bool,
    /// Surface locale, forwarded to the daemon's `WELCOME.spawn`.
    pub locale: Option<String>,
    /// Bound on per-handshake reads + writes (S2.5; `MEMENTO_DAEMON_PIPE_TIMEOUT`).
    pub pipe_timeout: Duration,
}

impl DaemonFixtureOptions {
    /// The fixture's deterministic pipe name (mirrors `pipe_name`).
    pub fn pipe_name(&self) -> String {
        pipe_name(&self.root, self.ctx.tenant_id())
    }

    /// The cookie path the daemon writes at startup.
    pub fn cookie_path(&self) -> PathBuf {
        self.root.join(format!(".daemon-{}.cookie", std::process::id()))
    }
}

/// An in-process daemon bound to a real Windows named pipe, with helpers
/// production callers use (`connect_client`, `cookie_path`, `pid`,
/// `tenant_id`). `Drop` aborts the accept loop and removes the cookie.
pub struct DaemonFixture {
    state: Arc<DaemonState>,
    pid: u32,
    options: DaemonFixtureOptions,
    runner: Option<JoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    cookie_path: PathBuf,
}

impl DaemonFixture {
    /// Spawn the in-process daemon. Returns once the cookie is written
    /// (REQ-DAEMON-003 readiness signal #1) and the pipe is bound
    /// (readiness signal #2).
    pub async fn start(opts: DaemonFixtureOptions) -> Self {
        // 1. Parse boundary + embedder (preserved across quiesce/resume, R2).
        let parse: Arc<dyn ParsePort> = Arc::new(ParseService::new(AnydocConfig {
            command: AnydocCommand {
                program: "never-invoked".into(),
                args: vec![],
                env: vec![],
            },
            timeout: Duration::from_secs(1),
            stdout_limit: 1024,
            staging_dir: std::env::temp_dir(),
        }));
        let embedder: Option<Arc<dyn EmbedPort>> = if opts.no_embeddings {
            None
        } else {
            Some(Arc::new(StubEmbedPort::default()))
        };
        // 2. Open the AppService (REQ-DAEMON-002 — daemon owns the store).
        let clock: Arc<dyn memento_application::Clock> = Arc::new(SystemClock);
        let app = AppService::open(
            &opts.ctx,
            &opts.root,
            parse.clone(),
            embedder.clone(),
            clock.clone(),
        )
        .await
        .expect("fixture: AppService opens");
        let state = Arc::new(DaemonState::new(
            opts.root.clone(),
            opts.ctx.clone(),
            parse,
            embedder,
            None,
            clock,
            app,
        ));

        // 3. Cookie nonce (REQ-DAEMON-012).
        let nonce = generate_nonce();
        let cookie_path = opts.cookie_path();
        std::fs::write(&cookie_path, &nonce).expect("write cookie");

        // 4. Bind the named pipe (REQ-DAEMON-003 readiness #2).
        let name = opts.pipe_name();
        let pipe = DaemonPipe::bind(&name).await.expect("bind pipe");
        info!(%name, "DaemonFixture: pipe bound");

        // 5. Accept loop with shutdown signal.
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let runner = tokio::spawn(accept_loop(
            pipe,
            state.clone(),
            DaemonAuth {
                root: opts.root.clone(),
                ctx: opts.ctx.clone(),
                daemon_token: opts.token.clone(),
                cookie_path: cookie_path.clone(),
                no_embeddings: opts.no_embeddings,
                locale: opts.locale.clone(),
            },
            opts.pipe_timeout,
            shutdown_rx,
        ));

        Self {
            state,
            pid: std::process::id(),
            options: opts,
            runner: Some(runner),
            shutdown_tx: Some(shutdown_tx),
            cookie_path,
        }
    }

    /// The daemon's bound `DaemonState` (the dispatcher's source of truth).
    /// Tests inspect `app_is_open` / `shutdown_requested` after exercising
    /// `sys.*` to assert lifecycle semantics.
    pub fn state(&self) -> &Arc<DaemonState> {
        &self.state
    }

    /// The bound tenant id (mirrors `Welcome::tenant_id`).
    pub fn tenant_id(&self) -> &TenantId {
        self.options.ctx.tenant_id()
    }

    /// The daemon's pid (the test process id — in-process fixture).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The fixture's storage root (production layout root).
    pub fn root(&self) -> &Path {
        &self.options.root
    }

    /// The cookie nonce path (`.daemon-<pid>.cookie`).
    pub fn cookie_path(&self) -> &Path {
        &self.cookie_path
    }

    /// The deterministic pipe name.
    pub fn pipe_name(&self) -> String {
        self.options.pipe_name()
    }

    /// Connect a fresh client over the fixture's pipe. Returns a
    /// [`DaemonFixtureClient`] that owns the framed connection + a clone
    /// of the welcome envelope.
    pub async fn connect_client(&self) -> DaemonFixtureClient {
        connect_handshake(&self.options, &self.cookie_path).await
    }

    /// Dispatch one command directly through the in-process dispatcher
    /// (bypasses the wire). Mirrors what a wire roundtrip would yield —
    /// used by quiesce/resume tests that want the dispatcher's source of
    /// truth without a connection roundtrip.
    pub async fn dispatch(
        &self,
        cmd: Command,
    ) -> Result<serde_json::Value, DomainError> {
        dispatch_command_with_state(&self.state, cmd).await
    }
}

impl Drop for DaemonFixture {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.runner.take() {
            handle.abort();
        }
        let _ = std::fs::remove_file(&self.cookie_path);
    }
}

/// A connected client owned by the test (NOT a `DaemonClient` — those are
/// CLI-side and use the memento-cli transport; the fixture-side client is
/// raw pipe + frame codec + dispatcher). Tests use this to drive
/// `sys.metrics` / `sys.shutdown` roundtrips or to dispatch one mcp
/// command per connection.
pub struct DaemonFixtureClient {
    conn: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes>,
    welcome: Welcome,
}

impl DaemonFixtureClient {
    /// The WELCOME envelope (capabilities + spawn config).
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Dispatch one framed [`Command`] and read the response JSON.
    /// Convenience wrapper over [`frame::write_message`] +
    /// [`frame::read_message`] + serde_json.
    pub async fn dispatch(
        &mut self,
        cmd: Command,
    ) -> Result<serde_json::Value, String> {
        let bytes = serde_json::to_vec(&cmd).expect("command serializes");
        frame::write_message(&mut self.conn, &bytes)
            .await
            .map_err(|err| format!("write: {err}"))?;
        let raw = frame::read_message(&mut self.conn)
            .await
            .map_err(|err| format!("read: {err}"))?;
        serde_json::from_slice(&raw).map_err(|err| format!("parse: {err}"))
    }

    /// Orderly close — drop the stream; the daemon's accept loop sees EOF
    /// and keeps listening (REQ-DAEMON-006: client disconnect ≠ daemon exit).
    pub async fn shutdown(mut self) {
        let _ = self.conn.shutdown().await;
    }
}

// ---- helpers ----------------------------------------------------------------

fn generate_nonce() -> String {
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    let mut hex = String::with_capacity(64);
    for byte in buf.iter() {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Run the daemon's accept loop: accept one connection, do the handshake,
/// dispatch one framed request, write one framed response, return to
/// accept. Polls the shutdown signal between accepts.
async fn accept_loop(
    pipe: DaemonPipe,
    state: Arc<DaemonState>,
    auth: DaemonAuth,
    timeout: Duration,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                info!("DaemonFixture: shutdown signal received");
                return;
            }
            accepted = pipe.accept() => {
                let conn = match accepted {
                    Ok(c) => c,
                    Err(err) => {
                        warn!(?err, "DaemonFixture: accept failed");
                        continue;
                    }
                };
                let auth = auth.clone();
                let state = state.clone();
                tokio::spawn(async move {
                    let mut conn = conn;
                    let welcome = match server_handshake_with_timeout(&mut conn, &auth, timeout).await {
                        Ok(w) => w,
                        Err(err) => {
                            warn!(?err, "DaemonFixture: handshake failed");
                            return;
                        }
                    };
                    if let Err(err) = serve_request(&mut conn, state.clone(), timeout).await {
                        warn!(?err, "DaemonFixture: serve_request failed");
                    }
                    let _ = welcome;
                });
            }
        }
    }
}

async fn serve_request<S>(
    conn: &mut S,
    state: Arc<DaemonState>,
    timeout: Duration,
) -> Result<(), std::io::Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let raw = match tokio::time::timeout(timeout, frame::read_message(conn)).await {
        Ok(Ok(b)) => b,
        Ok(Err(err)) => return Err(err),
        Err(_) => return Ok(()),
    };
    let cmd: Command = match serde_json::from_slice(&raw) {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "DaemonFixture: request is not a valid Command");
            return Ok(());
        }
    };
    let value = match dispatch_command_with_state(&state, cmd).await {
        Ok(v) => v,
        Err(err) => {
            warn!(?err, "DaemonFixture: dispatch failed");
            return Ok(());
        }
    };
    let payload = serde_json::to_vec(&value).expect("dispatcher response serializes");
    tokio::time::timeout(timeout, frame::write_message(conn, &payload))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "response write timeout"))??;
    Ok(())
}

async fn connect_handshake(
    opts: &DaemonFixtureOptions,
    cookie_path: &Path,
) -> DaemonFixtureClient {
    let name = opts.pipe_name();
    let mut conn: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> = tokio::time::timeout(
        opts.pipe_timeout,
        PipeStream::connect_by_path(name.as_str()),
    )
    .await
    .expect("connect timeout")
    .expect("pipe connect");
    let cookie = std::fs::read_to_string(cookie_path).expect("read cookie");
    let hello = Hello {
        proto: PROTOCOL_VERSION,
        role: Role::Cli,
        pid: std::process::id(),
        ppid: 0,
        version: env!("CARGO_PKG_VERSION").to_string(),
        cookie: cookie.trim().to_string(),
        token: opts.token.clone(),
        locale: opts.locale.clone(),
        no_embeddings: opts.no_embeddings,
        staging: std::env::temp_dir(),
    };
    let payload = serde_json::to_vec(&hello).expect("HELLO serializes");
    frame::write_message(&mut conn, &payload)
        .await
        .expect("write HELLO");
    let raw = frame::read_message(&mut conn).await.expect("read WELCOME");
    let welcome: Welcome = serde_json::from_slice(&raw).expect("WELCOME parses");
    DaemonFixtureClient { conn, welcome }
}