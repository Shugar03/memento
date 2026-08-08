//! T-070 RED tests — MCP protocol boundary (threat-matrix row 3).
//!
//! The design threat matrix maps the MCP stdio protocol boundary to three
//! cases: malformed frames, schema-violating parameters, and session
//! survival (REQ-MS-005: "Malformed requests MUST NOT terminate the stdio
//! session or the process").
//!
//! These tests are RED by design: they assert the boundary properties
//! BEFORE the server skeleton exists (T-071) and the implementation must
//! make them pass without weakening them:
//!
//! 1. A malformed (invalid-JSON) frame and a bogus notification must not
//!    kill the session — the handshake and tools/list still succeed.
//! 2. A tool call with schema-violating parameters returns a STRUCTURED
//!    error (is_error result, deserialization message) and the SAME session
//!    executes a valid call afterwards.
//! 3. An unknown tool returns a structured protocol error and the SAME
//!    session survives.
//!
//! Harness: an in-process rmcp client-server pair over
//! `tokio::io::duplex`, a real AppService over a tempdir LanceDB store, and
//! the stub embedder (no ONNX download — REQ-MC-004 day-1 vectors).

use std::sync::Arc;
use std::time::Duration;

use memento_application::{AppService, SystemClock};
use memento_mcp::McpServer;
use memento_parse::ParseService;
use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
use memento_testkit::{StubEmbedPort, TempStore};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use rmcp::service::ServiceError;
use rmcp::{ClientHandler, ServiceExt};
use serde_json::{Value, json};

/// Minimal in-process MCP client. The MVP server never calls back, so the
/// default `ClientHandler` behavior suffices.
struct TestClient;

impl ClientHandler for TestClient {}

/// Build the server under test: a real AppService over a temp LanceDB
/// store, stub embedder, fixed clock (retention is not exercised here).
async fn test_server(ts: &TempStore) -> McpServer {
    let app = AppService::open(
        &ts.ctx(),
        ts.root(),
        Arc::new(ParseService::new(AnydocConfig {
            command: AnydocCommand {
                program: "never-invoked".into(),
                args: vec![],
                env: vec![],
            },
            timeout: Duration::from_secs(1),
            stdout_limit: 1024,
            staging_dir: std::env::temp_dir(),
        })),
        Some(Arc::new(StubEmbedPort::default())),
        Arc::new(SystemClock),
    )
    .await
    .expect("test app opens");
    McpServer::from_app(app, ts.ctx(), memento_i18n::Locale::Es)
}

/// A `tools/call` request for `tool` with JSON `args`.
fn call_params(tool: &str, args: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(tool.to_string())
        .with_arguments(args.as_object().cloned().unwrap_or_default())
}

/// A valid `memory.search` call for the bound workspace (used after every
/// failure to prove the session is still alive).
fn valid_search(ts: &TempStore) -> CallToolRequestParams {
    call_params(
        "memory.search",
        json!({ "query": "memoria", "workspace_id": ts.workspace_id().to_string() }),
    )
}

/// Pair the server with an in-process client over a 1 MiB memory duplex.
async fn pair(
    server: McpServer,
) -> (
    rmcp::service::RunningService<rmcp::RoleClient, TestClient>,
    tokio::task::JoinHandle<()>,
) {
    let (server_half, client_half) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(async move {
        let running = server
            .serve(server_half)
            .await
            .expect("server handshake completes");
        let _ = running.waiting().await;
    });
    let client = TestClient
        .serve(client_half)
        .await
        .expect("client handshake completes");
    (client, task)
}

/// Assert the given call completed as a NON-error tool result (helper used
/// after every boundary violation to prove the session survived).
async fn assert_session_alive(
    client: &rmcp::service::RunningService<rmcp::RoleClient, TestClient>,
    ts: &TempStore,
) {
    let ok = client
        .call_tool(valid_search(ts))
        .await
        .expect("session alive: valid call succeeds");
    assert_ne!(
        ok.is_error,
        Some(true),
        "valid call must not carry is_error"
    );
}

#[tokio::test]
async fn malformed_frames_do_not_kill_the_session() {
    // Threat-matrix row 3 (adapted): raw garbage frames hit the stdio
    // boundary BEFORE the client service takes over. The server must
    // absorb them (the transport ignores unparsable lines) and keep
    // serving — REQ-MS-005 crash isolation.
    let ts = TempStore::new();
    let server = test_server(&ts).await;

    let (server_half, mut client_half) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(async move {
        let running = server
            .serve(server_half)
            .await
            .expect("server handshake completes");
        let _ = running.waiting().await;
    });

    // Invalid-JSON garbage line + a well-formed JSON-RPC notification with
    // a bogus method (no response expected for notifications).
    tokio::io::AsyncWriteExt::write_all(&mut client_half, b"not json {{{{\r\n")
        .await
        .expect("write garbage frame");
    tokio::io::AsyncWriteExt::write_all(
        &mut client_half,
        b"{\"jsonrpc\":\"2.0\",\"method\":\"bogus/method\",\"params\":[1,2,3]}\n",
    )
    .await
    .expect("write bogus notification");

    let client = TestClient
        .serve(client_half)
        .await
        .expect("client handshake still completes after garbage");

    // The registry is intact and calls still execute.
    let tools = client
        .list_tools(None)
        .await
        .expect("tools/list after garbage");
    assert_eq!(tools.tools.len(), 15, "full registry served");
    assert_session_alive(&client, &ts).await;
    task.abort();
}

#[tokio::test]
async fn schema_violating_params_return_structured_error_and_session_survives() {
    // REQ-MS-005 scenario "Malformed params survive": a call with
    // schema-violating parameters (query is an integer, the schema says
    // string) returns a STRUCTURED error — not a transport failure — and
    // the SAME session then executes a valid call.
    let ts = TempStore::new();
    let server = test_server(&ts).await;
    let (client, task) = pair(server).await;

    let err_result = client
        .call_tool(call_params(
            "memory.search",
            json!({ "query": 123, "workspace_id": ts.workspace_id().to_string() }),
        ))
        .await
        .expect("structured error, not a transport failure");
    assert_eq!(err_result.is_error, Some(true), "tool-level error");
    let text = err_result
        .content
        .iter()
        .find_map(|block| block.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(
        text.contains("failed to deserialize parameters"),
        "deserialization error text: {text}"
    );

    assert_session_alive(&client, &ts).await;
    task.abort();
}

#[tokio::test]
async fn unknown_tool_is_a_protocol_error_and_session_survives() {
    // A call to a tool outside the 15-tool registry is a structured
    // protocol error (invalid params: "tool not found") and the session
    // keeps serving.
    let ts = TempStore::new();
    let server = test_server(&ts).await;
    let (client, task) = pair(server).await;

    let err = client
        .call_tool(call_params("memory.does_not_exist", json!({})))
        .await
        .expect_err("unknown tool must not succeed");
    match err {
        ServiceError::McpError(e) => {
            assert_eq!(
                e.code,
                ErrorCode::INVALID_PARAMS,
                "structured protocol error"
            );
            assert_eq!(e.message.as_ref(), "tool not found");
        }
        other => panic!("unexpected error kind for unknown tool: {other:?}"),
    }

    assert_session_alive(&client, &ts).await;
    task.abort();
}
