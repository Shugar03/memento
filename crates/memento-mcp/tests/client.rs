//! T-102 — in-process rmcp client harness + full-surface round trip.
//!
//! The MCP surface is exercised exactly the way a real client would drive
//! it: an in-process rmcp client over `tokio::io::duplex`, a real
//! AppService over a tempdir LanceDB store and the stub embedder (no ONNX
//! download — REQ-MC-004 day-1 vectors), and the assembled 15-tool
//! registry (REQ-MS-002).
//!
//! This file is the harness home for the crate: [`TestClient`] + [`pair`] +
//! [`call_ok`] are the reusable primitives (the T-070 protocol RED tests
//! keep their own inline copies for independence). It proves the happy path
//! end-to-end — the ingest → search → get_chunk → feedback → context_fit →
//! delete lifecycle with structured outputs at every step — plus the code.*
//! error path on an unindexed project.

use std::sync::Arc;
use std::time::Duration;

use memento_application::{AppService, SystemClock};
use memento_mcp::McpServer;
use memento_parse::ParseService;
use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
use memento_testkit::{StubEmbedPort, TempStore};
use rmcp::model::CallToolRequestParams;
use rmcp::{ClientHandler, ServiceExt};
use serde_json::{Value, json};

/// Minimal in-process MCP client. The MVP server never calls back, so the
/// default `ClientHandler` behavior suffices.
struct TestClient;

impl ClientHandler for TestClient {}

/// Build the server under test: a real AppService over a temp LanceDB
/// store, stub embedder (deterministic vectors — ranking behaves like a
/// real embedder, D2), never-invoked parse boundary.
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

fn call_params(tool: &str, args: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(tool.to_string())
        .with_arguments(args.as_object().cloned().unwrap_or_default())
}

/// Call a tool, assert a NON-error result, and parse its JSON text block.
async fn call_ok(
    client: &rmcp::service::RunningService<rmcp::RoleClient, TestClient>,
    tool: &str,
    args: Value,
) -> Value {
    let res = client
        .call_tool(call_params(tool, args))
        .await
        .expect("tool call completes");
    assert_ne!(res.is_error, Some(true), "tool {tool} must succeed");
    let text = res
        .content
        .iter()
        .find_map(|block| block.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("tool {tool} output is JSON: {text}"))
}

/// Call a tool and assert it returns an is_error result (structured error
/// path, REQ-MS-005); returns the JSON payload.
async fn call_err(
    client: &rmcp::service::RunningService<rmcp::RoleClient, TestClient>,
    tool: &str,
    args: Value,
) -> Value {
    let res = client
        .call_tool(call_params(tool, args))
        .await
        .expect("tool call completes");
    assert_eq!(res.is_error, Some(true), "tool {tool} must error");
    let text = res
        .content
        .iter()
        .find_map(|block| block.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("tool {tool} error is JSON: {text}"))
}

#[tokio::test]
async fn client_drives_the_full_memory_surface_round_trip() {
    // The complete memory.* lifecycle through the wire: ingest → search →
    // get_chunk → feedback → context_fit → delete(doc) → search empty.
    let ts = TempStore::new();
    let ws = ts.workspace_id().to_string();
    let server = test_server(&ts).await;
    let (client, task) = pair(server).await;

    // ingest_text: chore id + chunk ids + provenance stamped (REQ-MC-006).
    let ingested = call_ok(
        &client,
        "memory.ingest_text",
        json!({ "text": "La memoria es un río subterráneo que fluye entre documentos.", "metadata": { "title": "Nota" } }),
    )
    .await;
    let chunk_ids: Vec<String> = ingested["chunk_ids"]
        .as_array()
        .expect("chunk_ids")
        .iter()
        .map(|v| v.as_str().expect("id").to_string())
        .collect();
    assert_eq!(chunk_ids.len(), 1, "short text → one chunk");
    let doc_id = ingested["doc_id"].as_str().expect("doc_id").to_string();

    // search: hit carries the canonical DTO (REQ-MS-006).
    let hits = call_ok(
        &client,
        "memory.search",
        json!({ "query": "memoria", "workspace_id": ws, "top_k": 10 }),
    )
    .await;
    let hit = &hits["hits"][0];
    assert_eq!(hit["chunk_id"], chunk_ids[0]);
    assert_eq!(hit["provenance"]["tenant_id"], ts.tenant_id().to_string());
    assert_eq!(hit["provenance"]["workspace_id"], ws);
    assert_eq!(hit["provenance"]["agent_id"], "test-agent");
    assert_eq!(hit["provenance"]["source"], "text");

    // get_chunk (REQ-MR-005).
    let chunk = call_ok(
        &client,
        "memory.get_chunk",
        json!({ "chunk_id": chunk_ids[0] }),
    )
    .await;
    assert_eq!(chunk["chunk"]["doc_id"], doc_id);
    assert!(
        chunk["chunk"]["text"]
            .as_str()
            .expect("text")
            .contains("río")
    );

    // feedback (REQ-ML-001).
    let fb = call_ok(
        &client,
        "memory.feedback",
        json!({ "chunk_id": chunk_ids[0], "useful": true, "reason": "muy útil" }),
    )
    .await;
    assert_eq!(fb["ok"], true);

    // context_fit: greedy selection within the token budget (REQ-MR-004/D6).
    let fit = call_ok(
        &client,
        "memory.context_fit",
        json!({ "query": "memoria", "budget_tokens": 1000, "workspace_id": ws }),
    )
    .await;
    assert_eq!(fit["chunks"].as_array().expect("chunks").len(), 1);
    assert!(fit["total_tokens"].as_u64().expect("tokens") > 0);
    assert_eq!(fit["chunks"][0]["chunk_id"], chunk_ids[0]);

    // delete doc → chunks gone from search (REQ-ML-002 hard delete). The
    // report counts every removed row: 1 chunk + 1 docs row + the feedback
    // row recorded above.
    let del = call_ok(
        &client,
        "memory.delete",
        json!({ "scope": "doc", "id": doc_id }),
    )
    .await;
    assert_eq!(del["deleted_count"], 3, "chunk + docs row + feedback row");
    let hits = call_ok(
        &client,
        "memory.search",
        json!({ "query": "memoria", "workspace_id": ws }),
    )
    .await;
    assert!(
        hits["hits"].as_array().expect("hits").is_empty(),
        "hard delete"
    );

    task.abort();
}

#[tokio::test]
async fn client_lists_the_15_tool_registry() {
    // REQ-MS-002: tools/list serves the full 15-tool registry.
    let ts = TempStore::new();
    let server = test_server(&ts).await;
    let (client, task) = pair(server).await;

    let tools = client.list_tools(None).await.expect("tools/list");
    assert_eq!(tools.tools.len(), 15, "full registry");
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
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
        assert!(
            names.contains(&expected),
            "registry missing {expected}: {names:?}"
        );
    }
    task.abort();
}

#[tokio::test]
async fn client_ingests_documents_and_searches_them() {
    // ingest_document: base64 blob across the JSON boundary (REQ-MC-002),
    // fallback Markdown normalization.
    let ts = TempStore::new();
    let ws = ts.workspace_id().to_string();
    let server = test_server(&ts).await;
    let (client, task) = pair(server).await;

    let markdown = "# Capítulo uno\n\nEl río de la memoria fluye.\n\n## Sección\n\nDocumento de prueba con contenido extenso.";
    use base64::Engine;
    let blob = base64::engine::general_purpose::STANDARD.encode(markdown.as_bytes());
    let ingested = call_ok(
        &client,
        "memory.ingest_document",
        json!({ "blob_b64": blob, "source": "markdown" }),
    )
    .await;
    let ids = ingested["chunk_ids"].as_array().expect("chunk_ids");
    assert!(!ids.is_empty(), "document chunks");
    assert_eq!(
        ingested["doc_id"].as_str().expect("doc_id").len(),
        36,
        "uuid v7 doc id"
    );

    let hits = call_ok(
        &client,
        "memory.search",
        json!({ "query": "memoria", "workspace_id": ws }),
    )
    .await;
    assert_eq!(
        hits["hits"][0]["provenance"]["source"], "markdown",
        "source label preserved"
    );
    task.abort();
}

#[tokio::test]
async fn client_code_tools_error_cleanly_on_unindexed_project() {
    // Read-only code tools on a project that was never indexed: structured
    // bilingual NOT_FOUND (REQ-CK-003), never a transport failure.
    let ts = TempStore::new();
    let server = test_server(&ts).await;
    let (client, task) = pair(server).await;

    let err = call_err(
        &client,
        "code.project_overview",
        json!({ "project_id": "0000000000000000" }),
    )
    .await;
    assert_eq!(err["code"], "NOT_FOUND", "stable code in the payload");
    assert!(
        !err["message_es"].as_str().expect("message_es").is_empty(),
        "ES message present"
    );
    assert!(
        !err["message_en"].as_str().expect("message_en").is_empty(),
        "EN fallback present"
    );

    // The session survives the error (REQ-MS-005): a memory call still works.
    let ok = call_ok(
        &client,
        "memory.search",
        json!({ "query": "memoria", "workspace_id": ts.workspace_id().to_string() }),
    )
    .await;
    assert_eq!(
        ok["hits"].as_array().expect("hits").len(),
        0,
        "session alive"
    );
    task.abort();
}
