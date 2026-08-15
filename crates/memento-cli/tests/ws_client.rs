//! REQ-DAEMON-001 acceptance: the CLI client working set stays ≤ 150 MB
//! while the daemon serves it (the client is a thin pipe client — it never
//! opens `AppService` and never loads the embedder; the daemon is the sole
//! model/store owner).
//!
//! The test drives the REAL `memento` binary against a live daemon-shaped
//! pipe server (the `DaemonFixture` is in-process, so a hand-rolled
//! mini-server bound to the canonical pipe name stands in — same wire
//! shape the fixture uses) and measures the client process's resident
//! working set with `GetProcessMemoryInfo` (via `memento_mcp::job`).
//!
//! The exercised command is `memento observability metrics` — the CLI's
//! daemon-mode path that really roundtrips over the pipe today
//! (REQ-DAEMON-010). The GIVEN's intent is the client-side budget: with a
//! warm daemon, the CLI process must stay thin. The daemon-stamped body in
//! the stdout proves the result came from the daemon, not a local dump.

use std::time::Duration;

use memento_mcp::daemon::{DaemonPipe, pipe_name};
use memento_mcp::frame;
use memento_mcp::handshake::{Capability, Hello, PROTOCOL_VERSION, Role, SpawnConfig, Welcome};
use memento_mcp::job::working_set_bytes;
use memento_tenant::CredentialStore;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

/// REQ-DAEMON-001 GIVEN: client working set ≤ 150 MB (int8 default).
const MAX_CLIENT_WS_BYTES: u64 = 150 * 1024 * 1024;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_client_working_set_stays_below_150mb_with_daemon_warm() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // Provision a tenant the CLI client can authenticate against.
    let (tid, token) = CredentialStore::new(root)
        .create_tenant("ws-tenant")
        .expect("provision tenant");

    // The daemon's readiness signals: cookie nonce (REQ-DAEMON-012) +
    // bound pipe.
    let nonce = "ws-client-nonce";
    let daemon_pid = std::process::id();
    let cookie_path = root.join(format!(".daemon-{daemon_pid}.cookie"));
    std::fs::write(&cookie_path, nonce).expect("cookie write");

    let name = pipe_name(root, &tid);
    let pipe = DaemonPipe::bind(&name).await.expect("bind pipe");

    // Mini-daemon: handshake (HELLO → WELCOME) + one `sys.metrics`
    // roundtrip per connection, mirroring `memento-mcp::daemon` +
    // `dispatcher::sys_metrics`.
    let expected_token = token.clone();
    let expected_cookie = nonce.to_string();
    let tid_str = tid.to_string();
    let server = tokio::spawn(async move {
        loop {
            let conn = match timeout(Duration::from_secs(15), pipe.accept()).await {
                Ok(Ok(c)) => c,
                _ => return,
            };
            let expected_token = expected_token.clone();
            let expected_cookie = expected_cookie.clone();
            let tid_str = tid_str.clone();
            tokio::spawn(async move {
                let mut conn = conn;
                // HELLO.
                let raw =
                    match timeout(Duration::from_secs(5), frame::read_message(&mut conn)).await {
                        Ok(Ok(b)) => b,
                        _ => return,
                    };
                let hello: Hello = match serde_json::from_slice(&raw) {
                    Ok(h) => h,
                    Err(_) => return,
                };
                assert_eq!(hello.cookie, expected_cookie, "cookie presented");
                assert_eq!(
                    hello.token.as_str(),
                    expected_token.as_str(),
                    "token presented"
                );
                // WELCOME (spawn config must match the client's env:
                // locale unset, embeddings on).
                let welcome = Welcome {
                    proto: PROTOCOL_VERSION,
                    daemon_pid,
                    tenant_id: tid_str,
                    capabilities: vec![Capability::Embedding, Capability::Quiesce],
                    spawn: SpawnConfig {
                        no_embeddings: false,
                        locale: None,
                    },
                    role: Role::Cli,
                };
                let payload = serde_json::to_vec(&welcome).expect("WELCOME serializes");
                let _ = frame::write_message(&mut conn, &payload).await;
                // One request → dispatcher-shaped sys.metrics envelope.
                let raw =
                    match timeout(Duration::from_secs(5), frame::read_message(&mut conn)).await {
                        Ok(Ok(b)) => b,
                        _ => return,
                    };
                let request: serde_json::Value =
                    serde_json::from_slice(&raw).expect("request JSON");
                assert_eq!(request["command"], "metrics", "sys.metrics dispatched");
                let body = format!(
                    "# source=daemon pid={daemon_pid} tenant=ws\n# HELP ws_test 1\nws_test 1\n"
                );
                let response = json!({
                    "status": "ok",
                    "format": "prometheus_text",
                    "body": body,
                    "ts": "2026-08-15T00:00:00Z",
                });
                let payload = serde_json::to_vec(&response).expect("response serializes");
                let _ = frame::write_message(&mut conn, &payload).await;
                let _ = conn.shutdown().await;
            });
        }
    });

    // The real CLI client against the daemon. All env the client needs
    // (MEMENTO_NO_DAEMON explicitly removed so the daemon path is live).
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_memento"))
        .env("MEMENTO_ROOT", root)
        .env("MEMENTO_TOKEN", token.as_str())
        .env("MEMENTO_AGENT_ID", "ws-agent")
        .env("MEMENTO_TENANT", tid.to_string())
        .env("MEMENTO_DAEMON_PIPE_TIMEOUT", "5")
        .env_remove("MEMENTO_NO_DAEMON")
        .args(["observability", "metrics"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("memento binary spawns");
    let pid = child.id();

    // Sample the client's resident working set while it runs.
    let mut peak: u64 = 0;
    let mut samples: u64 = 0;
    loop {
        if let Some(bytes) = working_set_bytes(pid) {
            peak = peak.max(bytes);
            samples += 1;
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(samples > 0, "at least one working-set sample was taken");

    let out = child.wait_with_output().expect("collect output");
    server.abort();

    // The daemon served the result (REQ-DAEMON-001 GIVEN: "the daemon
    // serves the result") — the daemon stamp proves the pipe path, not a
    // local dump.
    assert_eq!(
        out.status.code(),
        Some(0),
        "client exit 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("# source=daemon"),
        "result came from the daemon: {stdout}"
    );

    // REQ-DAEMON-001: client working set ≤ 150 MB.
    assert!(
        peak <= MAX_CLIENT_WS_BYTES,
        "client working set {peak} bytes ({} MiB) exceeds the 150 MB budget",
        peak / (1024 * 1024)
    );
}
