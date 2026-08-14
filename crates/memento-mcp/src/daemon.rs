//! Daemon pipe service (REQ-DAEMON-005/006/012, design D2/D3/D5).
//!
//! One [`DaemonPipe`] per (root, tenant) listens on the hashed pipe name
//! (D5); every connection starts with the framed HELLO/WELCOME handshake
//! ([`crate::handshake`]) and continues as an rmcp session over
//! [`crate::frame::FramedStream`] (wired by the dispatcher, S4).
//!
//! Auth (D3, REQ-DAEMON-005/012): the HELLO presents the raw token and the
//! cookie nonce; [`server_handshake`] validates both against the daemon's
//! startup values and closes with `AUTH_FAILED` + one audit line on
//! mismatch. Writes are bounded by [`pipe_timeout`] (S2.5): a stalled client
//! fails its own request while the daemon keeps serving the next connection
//! (REQ-DAEMON-006 GIVEN).

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use interprocess::os::windows::named_pipe::{
    pipe_mode, tokio::{PipeListener, PipeStream}, PipeListenerOptions,
};
use memento_application::audit::AuditLogger;
use memento_domain::{DomainError, TenantContext, TenantId};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::frame;
use crate::handshake::{Capability, Hello, SpawnConfig, Welcome, PROTOCOL_VERSION};

/// Default bound on daemon writes to a stalled client (S2.5).
pub const DEFAULT_PIPE_TIMEOUT: Duration = Duration::from_secs(5);

/// `MEMENTO_DAEMON_PIPE_TIMEOUT`: seconds a daemon write may block on a
/// non-draining client before the request fails (REQ-DAEMON-006). Overridable
/// per environment; the env is read once per call so tests can inject short
/// values.
pub fn pipe_timeout() -> Duration {
    std::env::var("MEMENTO_DAEMON_PIPE_TIMEOUT")
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .map(Duration::from_secs_f64)
        .unwrap_or(DEFAULT_PIPE_TIMEOUT)
}

/// The deterministic pipe name for a (root, tenant) pair (D5):
/// `\\.\pipe\memento-<sha256(canonical root)[0..16]>-<tenant_id>`. The token
/// never appears in the name (D4). Root is canonicalized when possible so
/// two spellings of the same path resolve to the same daemon.
pub fn pipe_name(root: &Path, tenant_id: &TenantId) -> String {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut hex16 = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex16.push_str(&format!("{byte:02x}"));
    }
    format!(r"\\.\pipe\memento-{hex16}-{tenant_id}")
}

/// Everything the daemon-side handshake needs to authenticate one client.
#[derive(Debug, Clone)]
pub struct DaemonAuth {
    /// The daemon's storage root (audit log location).
    pub root: PathBuf,
    /// The daemon's bound context (audit target identity).
    pub ctx: TenantContext,
    /// The daemon's own raw token (startup env; D3 — validated once per
    /// connection, never re-hashed).
    pub daemon_token: String,
    /// `<root>/.daemon-<pid>.cookie` — the nonce file the daemon wrote at
    /// startup (S5.1; tests create it themselves).
    pub cookie_path: PathBuf,
    /// The daemon's fixed spawn config (echoed in WELCOME; CONFIG_MISMATCH
    /// axis, R3).
    pub no_embeddings: bool,
    pub locale: Option<String>,
}

/// Handshake outcome tiers (mapped onto the REQ-DAEMON-002 error tiers by
/// the client/dispatcher layers).
#[derive(Debug)]
pub enum HandshakeError {
    /// Transport-level failure (broken pipe, refused, EOF mid-frame).
    Io(io::Error),
    /// The handshake exceeded [`pipe_timeout`] (stalled peer).
    Timeout,
    /// Wire/protocol violation: bad proto version, oversized message.
    Protocol(String),
    /// Token or cookie mismatch (REQ-DAEMON-005/012).
    AuthFailed(String),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Io(err) => write!(f, "pipe io: {err}"),
            HandshakeError::Timeout => write!(f, "handshake timed out"),
            HandshakeError::Protocol(msg) => write!(f, "protocol: {msg}"),
            HandshakeError::AuthFailed(reason) => write!(f, "auth failed: {reason}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl From<io::Error> for HandshakeError {
    fn from(err: io::Error) -> Self {
        HandshakeError::Io(err)
    }
}

/// The bound pipe listener for one daemon (D5).
pub struct DaemonPipe {
    listener: PipeListener<pipe_mode::Bytes, pipe_mode::Bytes>,
}

impl DaemonPipe {
    /// Bind the named pipe. The caller owns the name (derived from root +
    /// tenant via [`pipe_name`]); binding twice in the same namespace fails
    /// with a "pipe already exists" io error, which is how the spawner races
    /// for ownership (S5.3).
    pub async fn bind(name: &str) -> io::Result<Self> {
        // S5.2 attaches the owner-only SecurityDescriptor here (design:
        // interprocess SD, fallback windows raw).
        let listener = PipeListenerOptions::new()
            .path(name)
            .create_tokio_duplex::<pipe_mode::Bytes>()?;
        Ok(Self { listener })
    }

    /// Accept one client connection.
    pub async fn accept(&self) -> io::Result<PipeStream<pipe_mode::Bytes, pipe_mode::Bytes>> {
        self.listener.accept().await
    }
}

/// Read the cookie nonce file. Missing/corrupt cookies refuse the connection
/// (REQ-DAEMON-012 GIVEN: corrupt or missing cookie → AUTH_FAILED, no panic).
fn read_cookie(path: &Path) -> Result<String, HandshakeError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let cookie = raw.trim();
            if cookie.is_empty() {
                Err(HandshakeError::AuthFailed(
                    "cookie file is empty".to_string(),
                ))
            } else {
                Ok(cookie.to_string())
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(HandshakeError::AuthFailed(
            "cookie file missing".to_string(),
        )),
        Err(err) => Err(HandshakeError::Io(err)),
    }
}

/// Server side of the HELLO/WELCOME exchange (REQ-DAEMON-005/012, S2.2/S2.4):
/// read one framed HELLO, validate proto + cookie + token, write one framed
/// WELCOME. Every failure closes the connection; auth failures additionally
/// leave one audit line in `<root>/logs/<tid>.jsonl` (best-effort — the
/// audit log must never take the daemon down).
pub async fn server_handshake<S>(stream: &mut S, auth: &DaemonAuth) -> Result<Welcome, HandshakeError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    server_handshake_with_timeout(stream, auth, pipe_timeout()).await
}

/// [`server_handshake`] with an explicit bound (tests inject short timeouts;
/// production goes through [`pipe_timeout`]).
pub async fn server_handshake_with_timeout<S>(
    stream: &mut S,
    auth: &DaemonAuth,
    timeout: Duration,
) -> Result<Welcome, HandshakeError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let raw = tokio::time::timeout(timeout, frame::read_message(stream))
        .await
        .map_err(|_| HandshakeError::Timeout)??;
    let hello: Hello = serde_json::from_slice(&raw).map_err(|err| {
        HandshakeError::Protocol(format!("HELLO is not valid JSON: {err}"))
    })?;

    if hello.proto != PROTOCOL_VERSION {
        return Err(HandshakeError::Protocol(format!(
            "proto mismatch: client {}, daemon {PROTOCOL_VERSION}",
            hello.proto
        )));
    }

    // REQ-DAEMON-012: the cookie nonce must match the daemon's own file.
    let expected_cookie = read_cookie(&auth.cookie_path)?;
    if hello.cookie != expected_cookie {
        audit_auth_failure(auth, "cookie_mismatch");
        return Err(HandshakeError::AuthFailed("cookie mismatch".to_string()));
    }
    // REQ-DAEMON-005: the raw token must match the daemon's startup token
    // (D3: the daemon already verified it against the credential store at
    // startup; per-connection comparison avoids per-invocation Argon2).
    if hello.token != auth.daemon_token {
        audit_auth_failure(auth, "token_mismatch");
        return Err(HandshakeError::AuthFailed("token mismatch".to_string()));
    }

    let mut capabilities = vec![];
    if !auth.no_embeddings {
        capabilities.push(Capability::Embedding);
    }
    capabilities.push(Capability::Rerank);
    capabilities.push(Capability::Quiesce);

    let welcome = Welcome {
        proto: PROTOCOL_VERSION,
        daemon_pid: std::process::id(),
        tenant_id: auth.ctx.tenant_id().to_string(),
        capabilities,
        spawn: SpawnConfig {
            no_embeddings: auth.no_embeddings,
            locale: auth.locale.clone(),
        },
    };
    let payload = serde_json::to_vec(&welcome).map_err(|err| {
        HandshakeError::Protocol(format!("WELCOME serialization failed: {err}"))
    })?;
    tokio::time::timeout(timeout, frame::write_message(stream, &payload))
        .await
        .map_err(|_| HandshakeError::Timeout)??;
    Ok(welcome)
}

/// One audit line for a failed handshake (REQ-DAEMON-005 GIVEN: "one auth
/// event logged"). Best-effort: audit failures never fail the connection
/// path beyond the auth error itself.
fn audit_auth_failure(auth: &DaemonAuth, reason: &str) {
    let outcome = (|| -> Result<(), DomainError> {
        let logger = AuditLogger::new(&auth.root, auth.ctx.tenant_id())?;
        logger.error(
            &auth.ctx,
            "daemon_handshake",
            json!({ "reason": reason }),
            "AUTH_FAILED",
            None,
        );
        Ok(())
    })();
    if let Err(err) = outcome {
        tracing::warn!(%err, "daemon handshake audit line failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bound tenant context bound to a temp root (REQ-TA-001 shape).
    fn test_auth(ts: &memento_testkit::TempStore, cookie: &str) -> DaemonAuth {
        let mut ctx_path = ts.root().to_path_buf();
        ctx_path.push("ctx");
        std::fs::create_dir_all(&ctx_path).expect("ctx dir");
        DaemonAuth {
            root: ts.root().to_path_buf(),
            ctx: ts.ctx(),
            daemon_token: "memo_ok-secret".into(),
            cookie_path: {
                let p = ts.root().join(".daemon-test.cookie");
                std::fs::write(&p, cookie).expect("write cookie");
                p
            },
            no_embeddings: false,
            locale: Some("es".into()),
        }
    }

    /// Unique pipe name per test process.
    fn test_pipe(tag: &str) -> String {
        format!(r"\\.\pipe\memento-test-{tag}-{}", std::process::id())
    }

    fn sample_hello(token: &str, cookie: &str) -> Hello {
        Hello {
            proto: PROTOCOL_VERSION,
            role: crate::handshake::Role::Cli,
            pid: 4242,
            ppid: 1,
            version: env!("CARGO_PKG_VERSION").into(),
            cookie: cookie.into(),
            token: token.into(),
            locale: Some("es".into()),
            no_embeddings: false,
            staging: std::env::temp_dir(),
        }
    }

    #[test]
    fn pipe_name_is_deterministic_and_hashed() {
        // D5: the name derives from the canonical root + tenant; two
        // spellings of the same root map to the same pipe; the token never
        // appears in the name (D4).
        let root = std::env::temp_dir().join("memento-pipe-name-test");
        let tid: TenantId = "11111111-1111-4111-8111-111111111111"
            .parse()
            .expect("valid uuid");
        let name1 = pipe_name(&root, &tid);
        let name2 = pipe_name(&root, &tid);
        assert_eq!(name1, name2, "deterministic");
        assert!(name1.starts_with(r"\\.\pipe\memento-"), "prefix: {name1}");
        assert!(
            name1.ends_with("-11111111-1111-4111-8111-111111111111"),
            "tenant suffix: {name1}"
        );
        assert!(
            !name1.contains("memo_"),
            "token material never in the name: {name1}"
        );
        assert_eq!(
            name1.len(),
            r"\\.\pipe\memento-".len() + 16 + 1 + "11111111-1111-4111-8111-111111111111".len()
        );
    }

    #[tokio::test]
    async fn bind_accept_one_orderly_close() {
        // S2.1: tmp pipe, accept exactly one connection, orderly close.
        let name = test_pipe("accept1");
        let pipe = DaemonPipe::bind(&name).await.expect("bind");
        let client_task = tokio::spawn(async move {
            let client: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> = PipeStream::connect_by_path(name.as_str()).await.expect("connect");
            drop(client);
        });
        let conn = pipe.accept().await.expect("accept one");
        drop(conn);
        client_task.await.expect("client");
    }

    #[tokio::test]
    async fn handshake_succeeds_with_valid_token_and_cookie() {
        // Happy path: valid token + cookie → WELCOME echoes the daemon
        // config and capabilities (REQ-DAEMON-005/012).
        let ts = memento_testkit::TempStore::new();
        let auth = test_auth(&ts, "nonce-123");
        let name = test_pipe("hs-ok");
        let pipe = DaemonPipe::bind(&name).await.expect("bind");

        let client_task = tokio::spawn(async move {
            let mut client: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> = PipeStream::connect_by_path(name.as_str()).await.expect("connect");
            let hello = sample_hello("memo_ok-secret", "nonce-123");
            frame::write_message(&mut client, &serde_json::to_vec(&hello).expect("hello json"))
                .await
        });
        let mut conn = pipe.accept().await.expect("accept");
        let welcome = server_handshake(&mut conn, &auth).await.expect("handshake");
        assert_eq!(welcome.daemon_pid, std::process::id());
        assert_eq!(welcome.tenant_id, ts.ctx().tenant_id().to_string());
        assert!(welcome.has_embedding());
        assert!(welcome.has_quiesce());
        assert!(!welcome.spawn.no_embeddings);
        client_task.await.expect("client task");
    }

    #[tokio::test]
    async fn wrong_token_is_rejected_with_auth_failed_and_audit_line() {
        // REQ-DAEMON-005 GIVEN: wrong token → AUTH_FAILED + one audit line,
        // no panic, daemon keeps serving.
        let ts = memento_testkit::TempStore::new();
        let auth = test_auth(&ts, "nonce-456");
        let name = test_pipe("hs-badtoken");
        let pipe = DaemonPipe::bind(&name).await.expect("bind");

        let client_task = tokio::spawn(async move {
            let mut client: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> = PipeStream::connect_by_path(name.as_str()).await.expect("connect");
            let hello = sample_hello("memo_WRONG-secret", "nonce-456");
            frame::write_message(
                &mut client,
                &serde_json::to_vec(&hello).expect("hello json"),
            )
            .await
        });
        let mut conn = pipe.accept().await.expect("accept");
        let err = server_handshake(&mut conn, &auth).await.expect_err("must refuse");
        assert!(
            matches!(err, HandshakeError::AuthFailed(ref reason) if reason.contains("token")),
            "AUTH_FAILED tier: {err}"
        );
        client_task.await.expect("client task");

        // The audit line exists (REQ-DAEMON-005 GIVEN: one auth event).
        let audit_path = ts.root().join("logs").join(format!("{}.jsonl", ts.ctx().tenant_id()));
        let content = std::fs::read_to_string(&audit_path).expect("audit file");
        assert!(content.contains("daemon_handshake"), "audit line: {content}");
        assert!(content.contains("AUTH_FAILED"), "code: {content}");
    }

    #[tokio::test]
    async fn corrupt_cookie_is_refused() {
        // REQ-DAEMON-012 GIVEN: corrupt/missing cookie → AUTH_FAILED.
        let ts = memento_testkit::TempStore::new();
        let auth = test_auth(&ts, "nonce-correct");
        let name = test_pipe("hs-badcookie");
        let pipe = DaemonPipe::bind(&name).await.expect("bind");

        let client_task = tokio::spawn(async move {
            let mut client: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> = PipeStream::connect_by_path(name.as_str()).await.expect("connect");
            let hello = sample_hello("memo_ok-secret", "nonce-CORRUPTED");
            frame::write_message(
                &mut client,
                &serde_json::to_vec(&hello).expect("hello json"),
            )
            .await
        });
        let mut conn = pipe.accept().await.expect("accept");
        let err = server_handshake(&mut conn, &auth).await.expect_err("must refuse");
        assert!(
            matches!(err, HandshakeError::AuthFailed(ref reason) if reason.contains("cookie")),
            "AUTH_FAILED tier: {err}"
        );
        client_task.await.expect("client task");
    }

    #[tokio::test]
    async fn missing_cookie_file_is_refused() {
        let ts = memento_testkit::TempStore::new();
        let mut auth = test_auth(&ts, "nonce-789");
        // Point the auth at a cookie file that does not exist.
        auth.cookie_path = ts.root().join(".daemon-missing.cookie");
        let name = test_pipe("hs-nocookie");
        let pipe = DaemonPipe::bind(&name).await.expect("bind");

        let client_task = tokio::spawn(async move {
            let mut client: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> = PipeStream::connect_by_path(name.as_str()).await.expect("connect");
            let hello = sample_hello("memo_ok-secret", "nonce-789");
            frame::write_message(
                &mut client,
                &serde_json::to_vec(&hello).expect("hello json"),
            )
            .await
        });
        let mut conn = pipe.accept().await.expect("accept");
        let err = server_handshake(&mut conn, &auth).await.expect_err("must refuse");
        assert!(
            matches!(err, HandshakeError::AuthFailed(ref reason) if reason.contains("missing")),
            "AUTH_FAILED tier: {err}"
        );
        client_task.await.expect("client task");
    }

    #[tokio::test]
    async fn protocol_version_mismatch_is_refused() {
        let ts = memento_testkit::TempStore::new();
        let auth = test_auth(&ts, "nonce-abc");
        let name = test_pipe("hs-proto");
        let pipe = DaemonPipe::bind(&name).await.expect("bind");

        let client_task = tokio::spawn(async move {
            let mut client: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> = PipeStream::connect_by_path(name.as_str()).await.expect("connect");
            let mut hello = sample_hello("memo_ok-secret", "nonce-abc");
            hello.proto = PROTOCOL_VERSION + 1;
            frame::write_message(
                &mut client,
                &serde_json::to_vec(&hello).expect("hello json"),
            )
            .await
        });
        let mut conn = pipe.accept().await.expect("accept");
        let err = server_handshake(&mut conn, &auth).await.expect_err("must refuse");
        assert!(
            matches!(err, HandshakeError::Protocol(_)),
            "PROTOCOL tier: {err}"
        );
        client_task.await.expect("client task");
    }
    #[tokio::test]
    async fn stalled_client_read_times_out_and_daemon_serves_next() {
        // S2.5 / REQ-DAEMON-006 GIVEN: a client that stops talking fails its
        // own handshake after the bounded timeout; the daemon keeps serving
        // the next connection. (The write-stall bound is proven separately in
        // frame::tests::stalled_write_fails_after_timeout — the WELCOME is
        // small enough to fit the pipe buffer, so a non-draining client only
        // stalls large responses, which ride the same timeout.)
        let ts = memento_testkit::TempStore::new();
        let auth = test_auth(&ts, "nonce-stall");
        let name = test_pipe("hs-stall");
        let pipe = DaemonPipe::bind(&name).await.expect("bind");

        // Stalled client: connects, then never sends the HELLO.
        let name1 = name.clone();
        let stalled = tokio::spawn(async move {
            let _client: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> =
                PipeStream::connect_by_path(name1.as_str())
                    .await
                    .expect("connect");
            std::future::pending::<()>().await;
        });

        let mut conn = pipe.accept().await.expect("accept stalled conn");
        let started = std::time::Instant::now();
        let err = server_handshake_with_timeout(&mut conn, &auth, Duration::from_millis(200))
            .await
            .expect_err("must time out");
        assert!(
            matches!(err, HandshakeError::Timeout),
            "TIMEOUT tier: {err}"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(5),
            "bounded fail: {elapsed:?}"
        );
        stalled.abort();

        // The daemon still serves the NEXT connection (REQ-DAEMON-006).
        let name2 = name.clone();
        let client2 = tokio::spawn(async move {
            let mut client: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> =
                PipeStream::connect_by_path(name2.as_str())
                    .await
                    .expect("connect");
            let hello = sample_hello("memo_ok-secret", "nonce-stall");
            frame::write_message(
                &mut client,
                &serde_json::to_vec(&hello).expect("hello json"),
            )
            .await
            .expect("second client hello");
        });
        let mut conn2 = pipe.accept().await.expect("accept next conn");
        let welcome =
            server_handshake_with_timeout(&mut conn2, &auth, Duration::from_secs(5))
                .await
                .expect("second handshake");
        assert_eq!(welcome.daemon_pid, std::process::id());
        client2.await.expect("client2");
    }
}
