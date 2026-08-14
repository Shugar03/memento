//! `daemon-persistent` integration: 2 concurrent clients over one
//! shared daemon (REQ-DAEMON-008, design D2/D5/R1).
//!
//! The acceptance scenario for `mcp.*` + `cli.*` over the same daemon:
//!
//! 1. Spawn one daemon via [`DaemonFixture`] (in-process, real Windows
//!    named pipe bound to the test's `tempdir`).
//! 2. Connect two `DaemonClient`s back-to-back. Each sends one
//!    `Command::Sys(SysCommand::Metrics)` over its own connection.
//! 3. Assert both responses carry the daemon's process id + tenant id
//!    and stamp `prometheus_text` content consistent across calls
//!    (REB-DAEMON-010 / R5 reconciliation).
//! 4. Close both clients. Assert the daemon is still alive
//!    (REQ-DAEMON-006 GIVEN: "the daemon keeps serving the next
//!    connection" after a client disconnect).
//! 5. Open a THIRD connection post-close to prove the accept loop is
//!    intact (REQ-DAEMON-006 GIVEN: client disconnect ≠ daemon exit).
//!
//! Strict TDD: every assertion below is RED-first — without the
//! dispatcher wiring + the fixture the test cannot run.

use std::time::Duration;

use memento_cli::transport::pipe_client::{ClientConfig, DaemonClient};
use memento_mcp::dispatcher::{Command as DispatchCommand, SysCommand};
use memento_testkit::{DaemonFixture, DaemonFixtureOptions};

fn fixture() -> (tempfile::TempDir, memento_testkit::TempStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = memento_testkit::TempStore::new();
    (dir, store)
}

fn options(
    store: &memento_testkit::TempStore,
    dir: &tempfile::TempDir,
) -> DaemonFixtureOptions {
    DaemonFixtureOptions {
        root: dir.path().to_path_buf(),
        ctx: store.ctx(),
        token: format!("memo_it_{}", store.tenant_id()),
        no_embeddings: true,
        locale: Some("es".into()),
        pipe_timeout: Duration::from_secs(2),
    }
}

async fn connect(
    fixture: &DaemonFixture,
    token: &str,
) -> DaemonClient {
    let tenant_id_str = fixture.tenant_id().to_string();
    let config = ClientConfig {
        root: fixture.root().to_path_buf(),
        token: token.to_string(),
        agent_id: "agent-it".into(),
        tenant_id: tenant_id_str,
        locale: Some("es".into()),
        no_embeddings: true,
        pipe_timeout: Duration::from_secs(2),
    };
    DaemonClient::connect(&config)
        .await
        .expect("client connects")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_clients_both_get_consistent_metrics() {
    // S7.2 (B7): one daemon, two CLI-side clients, both dispatch
    // `sys.metrics` concurrently and observe consistent responses.
    // REQ-DAEMON-008 + REQ-DAEMON-010.
    let (_dir, store) = fixture();
    let fixture = DaemonFixture::start(options(&store, &_dir)).await;

    let token = format!("memo_it_{}", store.tenant_id());
    let mut a = connect(&fixture, &token).await;
    let mut b = connect(&fixture, &token).await;

    // Concurrent dispatch: the dispatcher is `&DaemonState` so the two
    // requests serialize on the `Mutex<Option<AppService>>`; that's the
    // S7.2 invariant — neither request ever sees a `STORE_BUSY` failure
    // because the dispatcher never returns one for `sys.*`.
    let resp_a = DispatchCommand::Sys(SysCommand::Metrics);
    let resp_b = DispatchCommand::Sys(SysCommand::Metrics);
    let (va, vb) = tokio::join!(
        a.dispatch(resp_a),
        b.dispatch(resp_b),
    );
    let va = va.expect("client A metrics");
    let vb = vb.expect("client B metrics");

    assert_eq!(va["status"], "ok", "client A: {va}");
    assert_eq!(vb["status"], "ok", "client B: {vb}");
    assert_eq!(va["format"], "prometheus_text", "client A format");
    assert_eq!(vb["format"], "prometheus_text", "client B format");

    let body_a = va["body"].as_str().expect("A body string");
    let body_b = vb["body"].as_str().expect("B body string");
    let pid_line = format!("# source=daemon pid={}", std::process::id());
    assert!(body_a.starts_with(&pid_line), "A stamp: {body_a}");
    assert!(body_b.starts_with(&pid_line), "B stamp: {body_b}");
    let tenant_line = format!("tenant={}", store.tenant_id());
    assert!(body_a.contains(&tenant_line), "A tenant: {body_a}");
    assert!(body_b.contains(&tenant_line), "B tenant: {body_b}");
    // Both bodies must agree on the daemon-stamp lines (the only thing
    // the metrics path stamps per-call is the timestamp footer).
    let lines_a: Vec<&str> = body_a.lines().filter(|l| l.starts_with('#')).collect();
    let lines_b: Vec<&str> = body_b.lines().filter(|l| l.starts_with('#')).collect();
    assert_eq!(lines_a, lines_b, "comment lines identical");

    // The daemon still serves (REQ-DAEMON-006: client disconnect ≠ exit).
    // The fixture's state stays open; the AppService was not dropped.
    assert!(fixture.state().app_is_open(), "daemon alive");

    // Drop both clients; the daemon's accept loop keeps running.
    drop(a);
    drop(b);

    // Open a THIRD connection post-close to prove the accept loop is
    // intact. The new client must handshake and dispatch successfully.
    let mut c = connect(&fixture, &token).await;
    let v_c = c
        .dispatch(DispatchCommand::Sys(SysCommand::Metrics))
        .await
        .expect("post-close metrics");
    assert_eq!(v_c["status"], "ok", "post-close: {v_c}");
    assert!(fixture.state().app_is_open(), "daemon still alive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_routes_sys_via_daemon_fixture() {
    // S7.2 unit test: every `sys.*` verb dispatched through
    // `DaemonFixture::dispatch` reaches the B5 body. This is the
    // out-of-process counterpart to `dispatcher::tests::with_state_tests`.
    let (_dir, store) = fixture();
    let fixture = DaemonFixture::start(options(&store, &_dir)).await;

    let q = fixture
        .dispatch(DispatchCommand::Sys(SysCommand::Quiesce))
        .await
        .expect("quiesce");
    assert_eq!(q["status"], "ok");
    assert_eq!(q["phase"], "quiesced");
    assert!(
        !fixture.state().app_is_open(),
        "AppService dropped on quiesce (R2)"
    );

    let r = fixture
        .dispatch(DispatchCommand::Sys(SysCommand::Resume))
        .await
        .expect("resume");
    assert_eq!(r["phase"], "resumed");
    assert!(fixture.state().app_is_open(), "AppService reopened");

    let s = fixture
        .dispatch(DispatchCommand::Sys(SysCommand::Shutdown))
        .await
        .expect("shutdown");
    assert_eq!(s["phase"], "shutting_down");
    assert!(fixture.state().shutdown_requested(), "flag raised");
}