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

fn options(store: &memento_testkit::TempStore, dir: &tempfile::TempDir) -> DaemonFixtureOptions {
    DaemonFixtureOptions {
        root: dir.path().to_path_buf(),
        ctx: store.ctx(),
        token: format!("memo_it_{}", store.tenant_id()),
        no_embeddings: true,
        locale: Some("es".into()),
        pipe_timeout: Duration::from_secs(2),
    }
}

async fn connect(fixture: &DaemonFixture, token: &str) -> DaemonClient {
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
    let (va, vb) = tokio::join!(a.dispatch(resp_a), b.dispatch(resp_b),);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_and_mcp_search_agree_over_one_daemon() {
    // REQ-DAEMON-008 GIVEN: one daemon serving a CLI-side dispatch
    // (memento-cli's pipe transport) AND the MCP stdio proxy returns
    // identical ids + scores for the same query — no lock errors, one
    // store owner. The CLI's delegable commands route through this same
    // `DaemonClient` transport once the command layer wires Remote.
    use std::sync::Arc;

    use memento_application::{AppService, SystemClock};
    use memento_mcp::dispatcher::McpCommand;
    use memento_mcp::dispatcher::MemoryTool;
    use memento_mcp::proxy::ProxyConfig;
    use memento_mcp::proxy::StdioProxy;
    use memento_ports::IngestTextRequest;
    use memento_ports::SearchQuery;
    use rmcp::ClientHandler;
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use serde_json::{Value, json};

    struct TestClient;
    impl ClientHandler for TestClient {}

    let dir = tempfile::tempdir().expect("tempdir");
    let store = memento_testkit::TempStore::new();

    // Seed BEFORE the daemon opens the same root (design D10 rule: no two
    // concurrent store holders).
    let direct_hits: Vec<(String, f32)> = {
        let parse: Arc<dyn memento_ports::ParsePort> = Arc::new(memento_parse::ParseService::new(
            memento_parse::anydoc::AnydocConfig {
                command: memento_parse::anydoc::AnydocCommand {
                    program: "never-invoked".into(),
                    args: vec![],
                    env: vec![],
                },
                timeout: std::time::Duration::from_secs(1),
                stdout_limit: 1024,
                staging_dir: std::env::temp_dir(),
            },
        ));
        let embedder: Option<Arc<dyn memento_ports::EmbedPort>> =
            Some(Arc::new(memento_testkit::StubEmbedPort::default()));
        let app = AppService::open(
            &store.ctx(),
            dir.path(),
            parse,
            embedder,
            Arc::new(SystemClock),
        )
        .await
        .expect("seed app opens");
        app.ingest_text(
            &store.ctx(),
            IngestTextRequest {
                text: "dual carrier search over one shared daemon".into(),
                doc_id: None,
                metadata: None,
            },
        )
        .await
        .expect("seed ingest");
        let hits = app
            .search(
                &store.ctx(),
                SearchQuery {
                    query: "shared daemon".into(),
                    top_k: 10,
                    workspace_id: *store.ctx().workspace_id(),
                    rrf_enabled: false,
                    rrf_k: 60.0,
                    rerank: false,
                    filters: None,
                },
            )
            .await
            .expect("direct search");
        let pairs: Vec<(String, f32)> = hits
            .iter()
            .map(|h| (h.chunk_id.to_string(), h.score))
            .collect();
        assert!(!pairs.is_empty(), "seeded search must produce hits");
        pairs
    };

    let fixture = DaemonFixture::start(options(&store, &dir)).await;
    let token = format!("memo_it_{}", store.tenant_id());

    // Carrier 1 — CLI transport: memento-cli's DaemonClient dispatches
    // memory.search with wire args.
    let mut cli = connect(&fixture, &token).await;
    let ws = store.ctx().workspace_id().to_string();
    let cli_resp = cli
        .dispatch(DispatchCommand::Mcp(McpCommand::Memory {
            tool: MemoryTool::Search,
            args: json!({ "query": "shared daemon", "workspace_id": ws, "top_k": 10 }),
        }))
        .await
        .expect("CLI-side search over the pipe");
    let cli_hits: Vec<(String, f32)> = cli_resp["hits"]
        .as_array()
        .expect("hits array")
        .iter()
        .map(|h| {
            (
                h["chunk_id"].as_str().expect("chunk_id").to_string(),
                h["score"].as_f64().expect("score") as f32,
            )
        })
        .collect();
    assert_eq!(cli_hits, direct_hits, "CLI carrier matches direct");

    // Carrier 2 — MCP stdio proxy over the SAME daemon.
    let proxy = StdioProxy::connect(&ProxyConfig {
        root: fixture.root().to_path_buf(),
        token: token.clone(),
        agent_id: "agent-it".into(),
        tenant_id: fixture.tenant_id().to_string(),
        locale: Some("es".into()),
        no_embeddings: true,
        pipe_timeout: std::time::Duration::from_secs(5),
    })
    .await
    .expect("proxy connects");
    let (server_half, client_half) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(async move {
        let running = proxy.serve(server_half).await.expect("proxy serve");
        let _ = running.waiting().await;
    });
    let client = TestClient.serve(client_half).await.expect("stdio client");
    let args: serde_json::Map<String, Value> = serde_json::from_value(json!({
        "query": "shared daemon",
        "workspace_id": store.ctx().workspace_id().to_string(),
        "top_k": 10,
    }))
    .expect("args object");
    let result = client
        .call_tool(CallToolRequestParams::new("memory.search").with_arguments(args))
        .await
        .expect("proxy search");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .expect("text block")
        .text
        .clone();
    let value: Value = serde_json::from_str(&text).expect("json");
    let proxy_hits: Vec<(String, f32)> = value["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| {
            (
                h["chunk_id"].as_str().expect("chunk_id").to_string(),
                h["score"].as_f64().expect("score") as f32,
            )
        })
        .collect();
    assert_eq!(
        proxy_hits, direct_hits,
        "MCP carrier matches direct — CLI + MCP agree over one daemon"
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_shaped_offline_move_survives_quiesce_resume() {
    // REQ-DAEMON-009: `tenant restore` runs quiesce → offline move →
    // resume WITHOUT killing the daemon. This test drives the daemon-side
    // contract with a restore-shaped offline move: quiesce the daemon,
    // move the tenant store directory (the offline move), resume, and
    // assert the daemon is alive and serving again (store intact).
    let (_dir, store) = fixture();
    let fixture = DaemonFixture::start(options(&store, &_dir)).await;

    let tenant_dir = fixture
        .root()
        .join("db")
        .join("tenants")
        .join(fixture.tenant_id().to_string());
    assert!(tenant_dir.exists(), "tenant store exists pre-restore");

    // 1. Quiesce (drains + releases the store handle, R2).
    let q = fixture
        .dispatch(DispatchCommand::Sys(SysCommand::Quiesce))
        .await
        .expect("quiesce");
    assert_eq!(q["status"], "ok");
    assert!(!fixture.state().app_is_open(), "store handle released");

    // 2. Offline move: rename the tenant dir out and back.
    let moved = tenant_dir.with_extension("restored");
    std::fs::rename(&tenant_dir, &moved).expect("offline move out");
    std::fs::rename(&moved, &tenant_dir).expect("offline move back");
    assert!(tenant_dir.exists(), "store intact after the move");

    // 3. Resume (reopens the store with the preserved adapter Arcs).
    let r = fixture
        .dispatch(DispatchCommand::Sys(SysCommand::Resume))
        .await
        .expect("resume");
    assert_eq!(r["status"], "ok");
    assert_eq!(r["phase"], "resumed");
    assert!(fixture.state().app_is_open(), "daemon serves again");

    // The daemon is alive and accepts a fresh connection (REQ-DAEMON-009
    // GIVEN: restore succeeds without killing the daemon).
    let token = format!("memo_it_{}", store.tenant_id());
    let mut client = connect(&fixture, &token).await;
    let v = client
        .dispatch(DispatchCommand::Sys(SysCommand::Metrics))
        .await
        .expect("post-restore metrics");
    assert_eq!(v["status"], "ok", "daemon alive post-restore: {v}");
}
