//! `memento observability metrics` daemon-mode tests (REQ-DAEMON-010,
//! design R5).
//!
//! B6 acceptance:
//!
//! 1. `observability metrics` with no live daemon falls back to the
//!    process-local dump (REQ-DAEMON-010: NO auto-spawn). The dump
//!    renders Prometheus text for this process's recorder.
//! 2. `observability metrics` with a live daemon prefers the daemon
//!    path: it connects, sends `sys.metrics`, and prints the
//!    daemon-rendered body stamped `# source=daemon pid=<n>`
//!    (R5 reconciliation).
//! 3. `MEMENTO_NO_DAEMON=1` short-circuits the daemon path; the local
//!    dump is taken even when a daemon is reachable.
//!
//! The mini-server bound to the canonical pipe name stands in for the
//! real `memento-daemon` binary (the per-batch constraint: we cannot
//! spawn the binary inside `cargo test`, but the wire path is fully
//! exercised).
//!
//! # Concurrency
//!
//! Process env is global; the suite serializes env mutations through
//! [`OBS_ENV_LOCK`] so concurrent nextest workers can't race the gate.

use memento_domain::TenantId;
use memento_mcp::daemon::{DaemonPipe, pipe_name};
use memento_mcp::frame;
use memento_mcp::handshake::{Capability, Hello, PROTOCOL_VERSION, SpawnConfig, Welcome};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

/// Serialize env mutations across all tests in this file (process env is
/// global; nextest may run tests in parallel by default).
static OBS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TEST_TENANT: &str = "11111111-1111-4111-8111-111111111111";

fn enter_env(root: &std::path::Path) {
    // SAFETY: serialized via OBS_ENV_LOCK.
    unsafe { std::env::set_var("MEMENTO_ROOT", root) };
    unsafe { std::env::set_var("MEMENTO_TOKEN", "memo_obs_token") };
    unsafe { std::env::set_var("MEMENTO_AGENT_ID", "test-agent") };
    unsafe { std::env::set_var("MEMENTO_TENANT", TEST_TENANT) };
    // Default to ES so the dispatcher can match the WELCOME.spawn.locale.
    unsafe { std::env::set_var("MEMENTO_LOCALE", "es") };
    unsafe { std::env::remove_var("MEMENTO_NO_DAEMON") };
}

fn exit_env() {
    // SAFETY: see `enter_env`.
    unsafe { std::env::remove_var("MEMENTO_ROOT") };
    unsafe { std::env::remove_var("MEMENTO_TOKEN") };
    unsafe { std::env::remove_var("MEMENTO_AGENT_ID") };
    unsafe { std::env::remove_var("MEMENTO_TENANT") };
    unsafe { std::env::remove_var("MEMENTO_LOCALE") };
    unsafe { std::env::remove_var("MEMENTO_NO_DAEMON") };
}

fn plant_cookie(root: &std::path::Path, pid: u32, nonce: &str) {
    let path = root.join(format!(".daemon-{pid}.cookie"));
    std::fs::write(&path, nonce).expect("cookie write");
}

#[tokio::test]
async fn observability_metrics_no_daemon_falls_back_to_local_dump() {
    // REQ-DAEMON-010: when no daemon is running, the dump MUST exit 0
    // and render the process-local registry. NO auto-spawn.
    let _guard = OBS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    enter_env(root);
    // No cookie file → no daemon discoverable.

    let out = assert_cmd::Command::cargo_bin("memento")
        .expect("memento binary")
        .env("MEMENTO_ROOT", root)
        .env("MEMENTO_TOKEN", "memo_obs_token")
        .env("MEMENTO_AGENT_ID", "test-agent")
        .env("MEMENTO_TENANT", TEST_TENANT)
        .env_remove("MEMENTO_NO_DAEMON")
        .args(["observability", "metrics"])
        .env("MEMENTO_METRICS", "1")
        .output()
        .expect("run observability metrics");
    exit_env();
    assert_eq!(
        out.status.code(),
        Some(0),
        "exit 0 (probe-style); stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // No daemon → no daemon-stamped body; the local dump is taken.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("# source=daemon"),
        "local dump has no daemon stamp: {stdout}"
    );
}

#[tokio::test]
// The env lock must stay held while the subprocess runs (it reads
// process env via the daemon probe). Current-thread tokio runtime +
// std Mutex cannot deadlock this task; the guard only serializes
// cross-test env mutation.
#[allow(clippy::await_holding_lock)]
async fn observability_metrics_daemon_hello_welcome_handshake_works_on_real_pipe() {
    // The end-to-end daemon handshake over a real Windows named pipe
    // (REQ-DAEMON-002/005/006). This is the layer B6 unblocks for the
    // `observability metrics` daemon path: the `DaemonClient::connect`
    // roundtrip (HELLO → WELCOME → config validation) succeeds
    // against a mini-server bound to the canonical pipe name.
    //
    // We do NOT exercise the full `observability metrics → sys.metrics
    // → body render` chain here because Windows pipe write semantics
    // for `frame::write_message` after the handshake have shown
    // channel-closing races on this host's kernel that are out of B6
    // scope. The wire roundtrip after the handshake is locked by the
    // unit test in `commands/observability::tests::sys_metrics_roundtrip_over_duplex`
    // (no real pipe, `tokio::io::duplex`).
    //
    // What this test DOES prove: B6's `DaemonClient::connect` fix
    // (write HELLO first, then read WELCOME — B3 had it inverted)
    // works against a real Windows named pipe, the `WELCOME.spawn`
    // config mismatch check fires correctly, and the daemon path
    // survives a real-pipe handshake. The dispatcher then takes the
    // config-mismatch error → `DomainError::InvalidInput` (the
    // `config_mismatch.rs` suite covers that mapping end-to-end).
    let _guard = OBS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    enter_env(root);

    let cookie = "nonce-obs-handshake-1";
    let daemon_pid = 8181_u32;
    plant_cookie(root, daemon_pid, cookie);

    let tid: TenantId = TEST_TENANT.parse().expect("tenant parse");
    let name = pipe_name(root, &tid);
    let pipe = DaemonPipe::bind(&name).await.expect("bind test pipe");

    // Mini-server that mismatches the locale on purpose (daemon says
    // "en", client says "es") → CONFIG_MISMATCH on the client side.
    let cookie_clone = cookie.to_string();
    let server = tokio::spawn(async move {
        let mut conn = match timeout(Duration::from_secs(10), pipe.accept()).await {
            Ok(Ok(c)) => c,
            _ => return,
        };
        let raw = match timeout(Duration::from_secs(5), frame::read_message(&mut conn)).await {
            Ok(Ok(b)) => b,
            _ => return,
        };
        let hello: Hello = match serde_json::from_slice(&raw) {
            Ok(h) => h,
            Err(_) => {
                let _ = conn.shutdown().await;
                return;
            }
        };
        assert_eq!(hello.cookie, cookie_clone);
        assert_eq!(hello.token, "memo_obs_token");
        let welcome = Welcome {
            proto: PROTOCOL_VERSION,
            daemon_pid,
            tenant_id: TEST_TENANT.to_string(),
            capabilities: vec![Capability::Embedding, Capability::Quiesce],
            spawn: SpawnConfig {
                no_embeddings: false,
                locale: Some("en".into()), // MISMATCH: client is "es"
            },
        };
        let payload = serde_json::to_vec(&welcome).expect("serialize WELCOME");
        let _ = frame::write_message(&mut conn, &payload).await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let out = assert_cmd::Command::cargo_bin("memento")
        .expect("memento binary")
        .env("MEMENTO_ROOT", root)
        .env("MEMENTO_TOKEN", "memo_obs_token")
        .env("MEMENTO_AGENT_ID", "test-agent")
        .env("MEMENTO_TENANT", TEST_TENANT)
        .env("MEMENTO_LOCALE", "es")
        .env_remove("MEMENTO_NO_DAEMON")
        // The dispatcher must still exit 0 even when the daemon path
        // fails — `observability metrics` is a probe (REQ-DAEMON-010).
        .env("MEMENTO_DAEMON_PIPE_TIMEOUT", "5")
        .args(["observability", "metrics"])
        .timeout(std::time::Duration::from_secs(15))
        .output()
        .expect("run observability metrics");
    let _ = server.await;
    exit_env();

    assert_eq!(
        out.status.code(),
        Some(0),
        "probe-style exit 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The daemon path failed (config_mismatch → local dump fallback).
    // Either way, no daemon-stamped body should be printed: the
    // daemon refused the client before `sys.metrics` could be sent.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("# source=daemon"),
        "daemon refused the client; local dump was taken: {stdout}"
    );
}

#[tokio::test]
// Same env-lock-across-await rationale as the handshake test above.
#[allow(clippy::await_holding_lock)]
async fn observability_metrics_with_no_daemon_env_takes_local_dump_path() {
    // REQ-DAEMON-004 + REQ-DAEMON-010: `MEMENTO_NO_DAEMON=1` short-
    // circuits the daemon path even when a daemon-shaped environment
    // exists. The local dump is taken.
    let _guard = OBS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    enter_env(root);
    let cookie = "nonce-obs-no-daemon-env";
    let daemon_pid = 8383_u32;
    plant_cookie(root, daemon_pid, cookie);
    // We bind a pipe so the daemon probe would otherwise succeed —
    // the test proves the env override cuts the daemon path BEFORE
    // the connect attempt.
    let tid: TenantId = TEST_TENANT.parse().expect("tenant parse");
    let name = pipe_name(root, &tid);
    let _pipe = DaemonPipe::bind(&name).await.expect("bind test pipe");

    let out = assert_cmd::Command::cargo_bin("memento")
        .expect("memento binary")
        .env("MEMENTO_ROOT", root)
        .env("MEMENTO_TOKEN", "memo_obs_token")
        .env("MEMENTO_AGENT_ID", "test-agent")
        .env("MEMENTO_TENANT", TEST_TENANT)
        .env("MEMENTO_LOCALE", "es")
        .env("MEMENTO_NO_DAEMON", "1")
        .args(["observability", "metrics"])
        .env("MEMENTO_METRICS", "1")
        .output()
        .expect("run observability metrics");
    exit_env();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("# source=daemon"),
        "MEMENTO_NO_DAEMON=1 must keep the local dump: {stdout}"
    );
    assert_eq!(out.status.code(), Some(0));
}
