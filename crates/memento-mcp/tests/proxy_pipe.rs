//! MCP stdio → pipe proxy integration (design D1/D4/S4.5, REQ-DAEMON-008).
//!
//! The proxy is the piece that kills the double model load (REQ-DAEMON-001
//! GIVEN-2): when a daemon is reachable, the MCP stdio server becomes a
//! thin rmcp client over the named pipe instead of opening its own
//! AppService + embedder. These tests prove, over one real daemon
//! (in-process `DaemonFixture` on a real Windows named pipe):
//!
//! 1. `tools/list` over stdio exposes exactly the public 15 tools —
//!    no `sys.*`, no `cli.*` (REQ-DAEMON-012 GIVEN-2).
//! 2. `memory.search` through the proxy returns the SAME ids + scores as
//!    a direct search on the same store (REQ-DAEMON-008 GIVEN, REQ-MS-006
//!    equivalence across carriers).
//! 3. The daemon-side role gate refuses `sys.*` from an `mcp_proxy`
//!    connection (REQ-DAEMON-012 role gate, D4).
//!
//! Strict TDD: these tests are RED-first — `memento_mcp::proxy::StdioProxy`
//! and the dispatcher's search body do not exist before this file.

use std::sync::Arc;
use std::time::Duration;

use interprocess::os::windows::named_pipe::{pipe_mode, tokio::PipeStream};
use memento_application::{AppService, SystemClock};
use memento_mcp::daemon::pipe_name;
use memento_mcp::dispatcher::{Command as DispatchCommand, SysCommand};
use memento_mcp::frame;
use memento_mcp::handshake::{Hello, PROTOCOL_VERSION, Role, Welcome};
use memento_mcp::proxy::{ProxyConfig, StdioProxy};
use memento_parse::ParseService;
use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
use memento_ports::{IngestTextRequest, SearchQuery};
use memento_testkit::{DaemonFixture, DaemonFixtureOptions, StubEmbedPort, TempStore};
use rmcp::model::{CallToolRequestParams, ListToolsResult};
use rmcp::{ClientHandler, ServiceExt};
use serde_json::{Value, json};

/// The stdio-side MCP client (no custom behavior — the SDK drives it).
struct TestClient;
impl ClientHandler for TestClient {}

fn fixture() -> (tempfile::TempDir, TempStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = TempStore::new();
    (dir, store)
}

fn options(store: &TempStore, dir: &tempfile::TempDir) -> DaemonFixtureOptions {
    DaemonFixtureOptions {
        root: dir.path().to_path_buf(),
        ctx: store.ctx(),
        token: format!("memo_px_{}", store.tenant_id()),
        no_embeddings: true,
        locale: Some("es".into()),
        pipe_timeout: Duration::from_secs(5),
    }
}

fn proxy_config(fixture: &DaemonFixture, store: &TempStore) -> ProxyConfig {
    ProxyConfig {
        root: fixture.root().to_path_buf(),
        token: format!("memo_px_{}", store.tenant_id()),
        agent_id: "agent-proxy".into(),
        tenant_id: fixture.tenant_id().to_string(),
        locale: Some("es".into()),
        no_embeddings: true,
        pipe_timeout: Duration::from_secs(5),
    }
}

/// A stub parse boundary (never invoked — search does not parse).
fn parse_stub() -> Arc<dyn memento_ports::ParsePort> {
    Arc::new(ParseService::new(AnydocConfig {
        command: AnydocCommand {
            program: "never-invoked".into(),
            args: vec![],
            env: vec![],
        },
        timeout: Duration::from_secs(1),
        stdout_limit: 1024,
        staging_dir: std::env::temp_dir(),
    }))
}

/// Serve the proxy over a duplex and return the connected stdio client.
async fn stdio_client(
    proxy: StdioProxy,
) -> (
    tokio::task::JoinHandle<()>,
    rmcp::service::RunningService<rmcp::service::RoleClient, TestClient>,
) {
    let (server_half, client_half) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(async move {
        let running = proxy.serve(server_half).await.expect("proxy stdio serve");
        let _ = running.waiting().await;
    });
    let client = TestClient
        .serve(client_half)
        .await
        .expect("stdio client handshake");
    (task, client)
}

fn tool_names(result: &ListToolsResult) -> Vec<String> {
    result.tools.iter().map(|t| t.name.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_proxy_lists_exactly_15_tools_and_no_sys() {
    // REQ-DAEMON-012 GIVEN-2: tools/list over the stdio proxy shows the
    // public 15 tools only; sys.* / cli.* are unreachable.
    let (_dir, store) = fixture();
    let fixture = DaemonFixture::start(options(&store, &_dir)).await;
    let proxy = StdioProxy::connect(&proxy_config(&fixture, &store))
        .await
        .expect("proxy connects to the daemon pipe");
    let (task, client) = stdio_client(proxy).await;

    let tools = client.list_tools(None).await.expect("tools/list");
    let names = tool_names(&tools);
    assert_eq!(names.len(), 15, "exactly 15 tools: {names:?}");
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("sys.") || n.starts_with("cli.")),
        "no sys.* or cli.* on the stdio carrier: {names:?}"
    );
    for expected in [
        "memory.search",
        "memory.ingest_text",
        "memory.ingest_document",
        "memory.get_chunk",
        "memory.feedback",
        "memory.delete",
        "memory.context_fit",
        "code.project_overview",
        "code.symbol_lookup",
        "code.callers_of",
        "code.callees_of",
        "code.impact",
        "code.dependencies",
        "code.search",
        "code.graph_dump",
    ] {
        assert!(names.iter().any(|n| n == expected), "missing {expected}");
    }
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_proxy_search_returns_identical_ids_and_scores_as_direct() {
    // REQ-DAEMON-008 GIVEN: a search through the stdio proxy over one
    // daemon returns the same ids + scores as a direct search on the same
    // store (REQ-MS-006 equivalence across carriers).
    let dir = tempfile::tempdir().expect("tempdir");
    let store = TempStore::new();

    // Seed the store + capture the DIRECT result BEFORE the daemon opens
    // the same root (no two concurrent store holders — design D10 rule).
    let direct_hits: Vec<(String, f32)> = {
        let embedder: Option<Arc<dyn memento_ports::EmbedPort>> =
            Some(Arc::new(StubEmbedPort::default()));
        let app = AppService::open(
            &store.ctx(),
            dir.path(),
            parse_stub(),
            embedder,
            Arc::new(SystemClock),
        )
        .await
        .expect("seed app opens");
        app.ingest_text(
            &store.ctx(),
            IngestTextRequest {
                text: "the quick brown fox jumps over the lazy dog daemon proxy".into(),
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
                    query: "daemon".into(),
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
    let proxy = StdioProxy::connect(&proxy_config(&fixture, &store))
        .await
        .expect("proxy connects");
    let (task, client) = stdio_client(proxy).await;

    let ws = store.ctx().workspace_id().to_string();
    let args: serde_json::Map<String, Value> = serde_json::from_value(json!({
        "query": "daemon",
        "workspace_id": ws,
        "top_k": 10,
    }))
    .expect("args object");
    let result = client
        .call_tool(CallToolRequestParams::new("memory.search").with_arguments(args))
        .await
        .expect("proxy search roundtrip");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .expect("text content block")
        .text
        .clone();
    let value: Value = serde_json::from_str(&text).expect("search result is JSON");
    let proxy_hits: Vec<(String, f32)> = value["hits"]
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
    assert_eq!(
        proxy_hits, direct_hits,
        "identical ids + scores across carriers (REQ-DAEMON-008)"
    );
    task.abort();
}

/// Raw pipe client speaking HELLO with `Role::McpProxy` — the same wire
/// shape the production proxy uses, without the rmcp layer.
async fn connect_proxy_role(
    fixture: &DaemonFixture,
    store: &TempStore,
) -> PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> {
    let name = pipe_name(fixture.root(), fixture.tenant_id());
    let mut conn: PipeStream<pipe_mode::Bytes, pipe_mode::Bytes> =
        PipeStream::connect_by_path(name.as_str())
            .await
            .expect("pipe connect");
    let cookie = std::fs::read_to_string(fixture.cookie_path())
        .expect("cookie")
        .trim()
        .to_string();
    let hello = Hello {
        proto: PROTOCOL_VERSION,
        role: Role::McpProxy,
        pid: std::process::id(),
        ppid: 0,
        version: env!("CARGO_PKG_VERSION").to_string(),
        cookie,
        token: format!("memo_px_{}", store.tenant_id()),
        locale: Some("es".into()),
        no_embeddings: true,
        staging: std::env::temp_dir(),
    };
    let payload = serde_json::to_vec(&hello).expect("HELLO json");
    frame::write_message(&mut conn, &payload)
        .await
        .expect("write HELLO");
    let raw = frame::read_message(&mut conn).await.expect("read WELCOME");
    let _welcome: Welcome = serde_json::from_slice(&raw).expect("WELCOME parses");
    conn
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_proxy_role_cannot_reach_sys_commands() {
    // REQ-DAEMON-012 role gate (D4): a connection that presented
    // `Role::McpProxy` MUST NOT reach sys.* — the daemon refuses the
    // dispatch with a structured FORBIDDEN envelope.
    let (_dir, store) = fixture();
    let fixture = DaemonFixture::start(options(&store, &_dir)).await;

    let mut conn = connect_proxy_role(&fixture, &store).await;
    let cmd = DispatchCommand::Sys(SysCommand::Metrics);
    let payload = serde_json::to_vec(&cmd).expect("command json");
    frame::write_message(&mut conn, &payload)
        .await
        .expect("write sys.metrics");
    let raw = frame::read_message(&mut conn)
        .await
        .expect("daemon replies");
    let resp: Value = serde_json::from_slice(&raw).expect("response json");
    assert_eq!(
        resp["status"], "error",
        "sys.* refused for mcp_proxy: {resp}"
    );
    assert_eq!(resp["code"], "FORBIDDEN", "role-gate code: {resp}");
    // The daemon survived the refusal (REQ-DAEMON-006: bad clients don't
    // take the daemon down).
    assert!(fixture.state().app_is_open(), "daemon alive after refusal");
}
