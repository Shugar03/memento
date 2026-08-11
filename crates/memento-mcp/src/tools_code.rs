//! code.* tools (T-073, REQ-MS-002): the 8 read-only code-knowledge tools.
//!
//! Each tool delegates to the application's [`CodeFacade`]
//! ([`AppService::code`], T-067) — the REQ-TA-005 context guard fires
//! there BEFORE any adapter work, and the okf adapter enforces tenant
//! isolation on every query (REQ-CK-011). Indexing itself is CLI-only
//! (design: the MCP surface is read-only); unindexed projects surface the
//! structured bilingual NOT_FOUND of REQ-CK-003.
//!
//! ## Empty-result contract (B1 fix from obs 2663)
//!
//! Every `code.*` tool that can return "no result" surfaces the structured
//! bilingual `NOT_FOUND` error (REQ-MS-004/005) — never soft `null` or
//! `[]`. The five tools that already errored on unindexed projects
//! (`impact`, `dependencies`, `graph_dump`, `project_overview`, and
//! `search` against an unknown project) keep their shape; the four that
//! used to return `null`/`[]` (`symbol_lookup`, `callers_of`,
//! `callees_of`, `search`) now consistently error with `NOT_FOUND` when
//! the query produced no result. `code.search` additionally rejects empty
//! / whitespace-only queries with `INVALID_INPUT` (B2 fix).

use memento_domain::{ArtifactKind, DomainError};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::McpServer;
use crate::errors::ToolError;

/// Default artifact count for `code.search` when the client omits `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 20;

// ---------- output DTOs -----------------------------------------------------

#[derive(Serialize, JsonSchema)]
struct OverviewOutput {
    project_id: String,
    summary: String,
    artifact_count: usize,
}

#[derive(Serialize, JsonSchema)]
struct ArtifactDto {
    project_id: String,
    artifact_id: String,
    kind: String,
    content: Value,
}

#[derive(Serialize, JsonSchema)]
struct SymbolOutput {
    /// `null` for unknown symbols (REQ-CK-004: clean not-found, not error).
    symbol: Option<ArtifactDto>,
}

#[derive(Serialize, JsonSchema)]
struct SymbolsOutput {
    symbols: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
struct CodeSearchOutput {
    artifacts: Vec<ArtifactDto>,
}

#[derive(Serialize, JsonSchema)]
struct GraphDumpOutput {
    /// Canonical `{nodes, edges}` graph (REQ-CK-009, Gephi/Cytoscape/Sigma
    /// compatible).
    graph: Value,
}

// ---------- input params ----------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ProjectParams {
    project_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct SymbolParams {
    project_id: String,
    symbol: String,
}

#[derive(Deserialize, JsonSchema)]
struct CodeSearchParams {
    project_id: String,
    query: String,
    #[serde(default)]
    limit: usize,
}

// ---------- the tools -------------------------------------------------------

/// The 8 read-only `code.*` tools, generated into `McpServer::code_tools()`.
#[tool_router(router = code_tools, vis = "pub(crate)")]
impl McpServer {
    /// code.project_overview — L4 architectural summary (REQ-CK-003).
    #[tool(name = "code.project_overview")]
    async fn code_project_overview(
        &self,
        Parameters(p): Parameters<ProjectParams>,
    ) -> Result<Json<OverviewOutput>, ToolError> {
        let port = self.app.code(&self.ctx).await?;
        let overview = port.project_overview(&self.ctx, &p.project_id).await?;
        Ok(Json(OverviewOutput {
            project_id: overview.project_id,
            summary: overview.summary,
            artifact_count: overview.artifact_count,
        }))
    }

    /// code.symbol_lookup — L2 symbol resolution (REQ-CK-004). Unknown
    /// symbols return the structured `NOT_FOUND` error (B1 fix).
    #[tool(name = "code.symbol_lookup")]
    async fn code_symbol_lookup(
        &self,
        Parameters(p): Parameters<SymbolParams>,
    ) -> Result<Json<SymbolOutput>, ToolError> {
        let port = self.app.code(&self.ctx).await?;
        let symbol = port
            .symbol_lookup(&self.ctx, &p.project_id, &p.symbol)
            .await?;
        match symbol {
            Some(artifact) => Ok(Json(SymbolOutput {
                symbol: Some(artifact_dto(artifact)),
            })),
            None => Err(ToolError(DomainError::NotFound {
                what: format!("symbol '{}' in project '{}'", p.symbol, p.project_id),
            })),
        }
    }

    /// code.callers_of — who calls a symbol, depth 2 (REQ-CK-005). Empty
    /// result (unknown symbol or no callers) returns `NOT_FOUND` (B1 fix).
    #[tool(name = "code.callers_of")]
    async fn code_callers_of(
        &self,
        Parameters(p): Parameters<SymbolParams>,
    ) -> Result<Json<SymbolsOutput>, ToolError> {
        let port = self.app.code(&self.ctx).await?;
        let symbols = port.callers_of(&self.ctx, &p.project_id, &p.symbol).await?;
        if symbols.is_empty() {
            return Err(ToolError(DomainError::NotFound {
                what: format!(
                    "callers of '{}' in project '{}'",
                    p.symbol, p.project_id
                ),
            }));
        }
        Ok(Json(SymbolsOutput { symbols }))
    }

    /// code.callees_of — what a symbol calls, depth 2 (REQ-CK-005). Empty
    /// result (unknown symbol or no callees) returns `NOT_FOUND` (B1 fix).
    #[tool(name = "code.callees_of")]
    async fn code_callees_of(
        &self,
        Parameters(p): Parameters<SymbolParams>,
    ) -> Result<Json<SymbolsOutput>, ToolError> {
        let port = self.app.code(&self.ctx).await?;
        let symbols = port.callees_of(&self.ctx, &p.project_id, &p.symbol).await?;
        if symbols.is_empty() {
            return Err(ToolError(DomainError::NotFound {
                what: format!(
                    "callees of '{}' in project '{}'",
                    p.symbol, p.project_id
                ),
            }));
        }
        Ok(Json(SymbolsOutput { symbols }))
    }

    /// code.impact — reverse reachability (REQ-CK-006).
    #[tool(name = "code.impact")]
    async fn code_impact(
        &self,
        Parameters(p): Parameters<SymbolParams>,
    ) -> Result<Json<SymbolsOutput>, ToolError> {
        let port = self.app.code(&self.ctx).await?;
        let symbols = port.impact(&self.ctx, &p.project_id, &p.symbol).await?;
        Ok(Json(SymbolsOutput { symbols }))
    }

    /// code.dependencies — module dependency graph with cycle detection
    /// (REQ-CK-007).
    #[tool(name = "code.dependencies")]
    async fn code_dependencies(
        &self,
        Parameters(p): Parameters<ProjectParams>,
    ) -> Result<Json<SymbolsOutput>, ToolError> {
        let port = self.app.code(&self.ctx).await?;
        let symbols = port.dependencies(&self.ctx, &p.project_id).await?;
        Ok(Json(SymbolsOutput { symbols }))
    }

    /// code.search — literal (always) + semantic (when embeddings are
    /// configured) search over the index (REQ-CK-008). Empty / whitespace
    /// queries are rejected with `INVALID_INPUT` (B2 fix); empty result
    /// surfaces `NOT_FOUND` (B1 fix). The embedder is pre-warmed lazily on
    /// first call (B3 fix) so the cold-start ONNX cost (~5 s) is not paid
    /// on the user-facing critical path.
    #[tool(name = "code.search")]
    async fn code_search(
        &self,
        Parameters(p): Parameters<CodeSearchParams>,
    ) -> Result<Json<CodeSearchOutput>, ToolError> {
        // B2 fix (obs 2663): reject empty / whitespace-only queries with
        // INVALID_INPUT — mirroring memory.search (REQ-MR-003). An empty
        // query used to silently return the top-N highest-scoring symbols,
        // which leaks data on a tenant with sensitive code.
        if p.query.trim().is_empty() {
            return Err(ToolError(DomainError::InvalidInput {
                message: "code.search requires a non-empty query".into(),
            }));
        }
        let port = self.app.code(&self.ctx).await?;
        let limit = if p.limit == 0 {
            DEFAULT_SEARCH_LIMIT
        } else {
            p.limit
        };
        let artifacts = port
            .search(&self.ctx, &p.project_id, &p.query, limit)
            .await?;
        if artifacts.is_empty() {
            return Err(ToolError(DomainError::NotFound {
                what: format!(
                    "code matching '{}' in project '{}'",
                    p.query, p.project_id
                ),
            }));
        }
        Ok(Json(CodeSearchOutput {
            artifacts: artifacts.into_iter().map(artifact_dto).collect(),
        }))
    }

    /// code.graph_dump — canonical `{nodes, edges}` JSON (REQ-CK-009).
    #[tool(name = "code.graph_dump")]
    async fn code_graph_dump(
        &self,
        Parameters(p): Parameters<ProjectParams>,
    ) -> Result<Json<GraphDumpOutput>, ToolError> {
        let port = self.app.code(&self.ctx).await?;
        let graph = port.graph_dump(&self.ctx, &p.project_id).await?;
        Ok(Json(GraphDumpOutput { graph }))
    }
}

// ---------- DTO construction helpers ---------------------------------------

/// Stable layer label for an artifact kind (REQ-CK-* layers L1..L4).
fn kind_label(kind: &ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Bundle => "bundle",
        ArtifactKind::Symbol => "symbol",
        ArtifactKind::Graph => "graph",
        ArtifactKind::Summary => "summary",
    }
}

fn artifact_dto(artifact: memento_domain::KnowledgeArtifact) -> ArtifactDto {
    ArtifactDto {
        project_id: artifact.project_id,
        artifact_id: artifact.artifact_id.to_string(),
        kind: kind_label(&artifact.kind).to_string(),
        content: artifact.content,
    }
}

#[cfg(test)]
mod tests {
    use crate::McpServer;
    use memento_application::{AppService, SystemClock};
    use memento_okf::OkfIndex;
    use memento_parse::ParseService;
    use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
    use memento_testkit::{StubEmbedPort, TempStore};
    use rmcp::model::CallToolRequestParams;
    use rmcp::{ClientHandler, ServiceExt};
    use serde_json::{Value, json};
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    struct TestClient;
    impl ClientHandler for TestClient {}

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

    async fn pair(
        server: McpServer,
    ) -> (
        rmcp::service::RunningService<rmcp::RoleClient, TestClient>,
        tokio::task::JoinHandle<()>,
    ) {
        let (server_half, client_half) = tokio::io::duplex(1 << 20);
        let task = tokio::spawn(async move {
            let running = server.serve(server_half).await.expect("server handshake");
            let _ = running.waiting().await;
        });
        let client = TestClient
            .serve(client_half)
            .await
            .expect("client handshake");
        (client, task)
    }

    fn call(tool: &str, args: Value) -> CallToolRequestParams {
        CallToolRequestParams::new(tool.to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default())
    }

    fn text_of(result: &rmcp::model::CallToolResult) -> Value {
        let text = result
            .content
            .iter()
            .find_map(|block| block.as_text())
            .map(|t| t.text.clone())
            .expect("text block");
        serde_json::from_str(&text).expect("tool output is JSON")
    }

    /// Code of a structured tool error (REQ-MS-005). Panics if the result
    /// is not an error or the code is missing — call this only when the
    /// test is asserting a structured failure.
    fn error_code(result: &rmcp::model::CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true), "expected structured error");
        let v = text_of(result);
        v["code"]
            .as_str()
            .unwrap_or_else(|| panic!("missing code in error payload: {v}"))
            .to_string()
    }

    /// Rust fixture: an entry → mid → leaf chain in `src/a.rs` PLUS a
    /// cross-module call into `src/b.rs` (the module dependency view needs
    /// at least two modules; same shape as the okf adapter's fixtures).
    fn write_chain_fixture(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/a.rs"),
            "fn entry() { mid(); helper(); }\nfn mid() { leaf(); }\nfn leaf() {}\n",
        )
        .unwrap();
        fs::write(root.join("src/b.rs"), "fn helper() {}\n").unwrap();
    }

    /// Index a fixture repo through the okf adapter on the SAME store the
    /// server will serve, returning its project id.
    async fn index_fixture(ts: &TempStore) -> String {
        let repo = tempfile::tempdir().expect("fixture repo");
        write_chain_fixture(repo.path());
        let index = OkfIndex::open(&ts.ctx(), ts.root(), None)
            .await
            .expect("okf index opens");
        let report = index
            .index_project(&ts.ctx(), repo.path())
            .await
            .expect("fixture indexes");
        report.project_id
    }

    #[tokio::test]
    async fn code_tools_serve_every_port_method_via_the_client() {
        // T-073 acceptance: the 8 code.* tools are listed and callable
        // end-to-end via the in-process client; graph_dump returns valid
        // JSON with referential integrity (REQ-CK-009).
        let ts = TempStore::new();
        let project_id = index_fixture(&ts).await;
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;

        // project_overview (REQ-CK-003).
        let overview = client
            .call_tool(call(
                "code.project_overview",
                json!({ "project_id": project_id }),
            ))
            .await
            .expect("overview ok");
        assert_ne!(overview.is_error, Some(true));
        let v = text_of(&overview);
        assert_eq!(v["project_id"], project_id);
        assert!(v["artifact_count"].as_u64().unwrap() >= 3);

        // symbol_lookup: known → artifact; unknown → NOT_FOUND (B1 fix).
        let symbol = client
            .call_tool(call(
                "code.symbol_lookup",
                json!({ "project_id": project_id, "symbol": "leaf" }),
            ))
            .await
            .expect("lookup ok");
        let v = text_of(&symbol);
        assert_eq!(v["symbol"]["kind"], "symbol");
        assert_eq!(v["symbol"]["artifact_id"], "leaf");
        let missing = client
            .call_tool(call(
                "code.symbol_lookup",
                json!({ "project_id": project_id, "symbol": "nope" }),
            ))
            .await
            .expect("missing ok");
        assert_eq!(error_code(&missing), "NOT_FOUND", "unknown symbol → NOT_FOUND (B1)");

        // callers_of/callees_of depth 2 (REQ-CK-005).
        let callers = client
            .call_tool(call(
                "code.callers_of",
                json!({ "project_id": project_id, "symbol": "leaf" }),
            ))
            .await
            .expect("callers ok");
        let v = text_of(&callers);
        let names: Vec<&str> = v["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert!(
            names.iter().any(|s| s.starts_with("entry (")),
            "depth-2 caller: {names:?}"
        );

        // impact (REQ-CK-006).
        let impact = client
            .call_tool(call(
                "code.impact",
                json!({ "project_id": project_id, "symbol": "leaf" }),
            ))
            .await
            .expect("impact ok");
        assert_eq!(
            text_of(&impact)["symbols"].as_array().unwrap().len(),
            2,
            "entry + mid affected"
        );

        // dependencies + cycle detection (REQ-CK-007): the cross-module
        // call aggregates to a module edge a → b.
        let deps = client
            .call_tool(call(
                "code.dependencies",
                json!({ "project_id": project_id }),
            ))
            .await
            .expect("deps ok");
        let v = text_of(&deps);
        let edges: Vec<&str> = v["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert!(
            edges
                .iter()
                .any(|e| e.starts_with("modules/src/a -> modules/src/b")),
            "module edge reported: {edges:?}"
        );

        // code.search (REQ-CK-008).
        let search = client
            .call_tool(call(
                "code.search",
                json!({ "project_id": project_id, "query": "leaf" }),
            ))
            .await
            .expect("search ok");
        let v = text_of(&search);
        assert!(!v["artifacts"].as_array().unwrap().is_empty());

        // graph_dump (REQ-CK-009): valid JSON, referential integrity.
        let dump = client
            .call_tool(call("code.graph_dump", json!({ "project_id": project_id })))
            .await
            .expect("dump ok");
        assert_ne!(dump.is_error, Some(true));
        let v = text_of(&dump);
        let nodes = v["graph"]["nodes"].as_array().expect("nodes array");
        let edges = v["graph"]["edges"].as_array().expect("edges array");
        assert!(!nodes.is_empty());
        let ids: std::collections::HashSet<&str> =
            nodes.iter().filter_map(|n| n["id"].as_str()).collect();
        for edge in edges {
            assert!(
                ids.contains(edge["source"].as_str().unwrap()),
                "edge source in nodes"
            );
            assert!(
                ids.contains(edge["target"].as_str().unwrap()),
                "edge target in nodes"
            );
        }

        task.abort();
    }

    #[tokio::test]
    async fn unindexed_project_is_a_structured_bilingual_error() {
        // REQ-CK-003 "Unindexed project": structured bilingual error
        // guiding toward the indexing step — never a crash.
        let ts = TempStore::new();
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;

        let err = client
            .call_tool(call(
                "code.project_overview",
                json!({ "project_id": "0000000000000000" }),
            ))
            .await
            .expect("structured error result");
        assert_eq!(err.is_error, Some(true));
        let v: Value = serde_json::from_str(&err.content[0].as_text().unwrap().text).expect("json");
        assert_eq!(v["code"], "NOT_FOUND");
        assert!(
            v["detail"].as_str().unwrap().contains("code index"),
            "detail carries the indexing guidance (REQ-CK-003)"
        );
        assert!(!v["message_en"].as_str().unwrap().is_empty(), "EN fallback");

        task.abort();
    }

    // ---------- B1 fix: empty-result shape is NOT_FOUND across all 4 tools

    #[tokio::test]
    async fn b1_callers_of_unknown_symbol_returns_not_found() {
        // B1 fix (obs 2663): callers_of for an unknown name returns the
        // structured NOT_FOUND bilingual error — NOT the soft
        // `{symbols: []}` it used to. Consistent with impact /
        // dependencies / graph_dump / project_overview.
        let ts = TempStore::new();
        let project_id = index_fixture(&ts).await;
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;

        let err = client
            .call_tool(call(
                "code.callers_of",
                json!({ "project_id": project_id, "symbol": "noSuchSymbol_xyz" }),
            ))
            .await
            .expect("structured error result");
        assert_eq!(error_code(&err), "NOT_FOUND", "callers_of unknown → NOT_FOUND");
        let v = text_of(&err);
        assert!(
            v["detail"].as_str().unwrap().contains("noSuchSymbol_xyz"),
            "detail carries the symbol name"
        );
        assert!(
            !v["message_en"].as_str().unwrap().is_empty(),
            "bilingual EN fallback present"
        );

        task.abort();
    }

    #[tokio::test]
    async fn b1_callees_of_unknown_symbol_returns_not_found() {
        // B1 fix: callees_of for an unknown name returns NOT_FOUND.
        let ts = TempStore::new();
        let project_id = index_fixture(&ts).await;
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;

        let err = client
            .call_tool(call(
                "code.callees_of",
                json!({ "project_id": project_id, "symbol": "noSuchSymbol_xyz" }),
            ))
            .await
            .expect("structured error result");
        assert_eq!(error_code(&err), "NOT_FOUND", "callees_of unknown → NOT_FOUND");

        task.abort();
    }

    #[tokio::test]
    async fn b1_search_with_no_matches_returns_not_found() {
        // B1 fix: code.search with a non-empty query that matches nothing
        // returns NOT_FOUND, consistent with the other code.* tools.
        // (The empty-query case is covered separately by b2_search_*.)
        let ts = TempStore::new();
        let project_id = index_fixture(&ts).await;
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;

        let err = client
            .call_tool(call(
                "code.search",
                json!({ "project_id": project_id, "query": "absolutelyNothingMatches_zzqq" }),
            ))
            .await
            .expect("structured error result");
        assert_eq!(error_code(&err), "NOT_FOUND", "search with no matches → NOT_FOUND");

        task.abort();
    }

    // ---------- B2 fix: empty / whitespace query is INVALID_INPUT ------

    #[tokio::test]
    async fn b2_search_empty_query_is_invalid_input() {
        // B2 fix (obs 2663): empty query used to leak the top-N
        // highest-scoring symbols. Reject with INVALID_INPUT — mirroring
        // memory.search (REQ-MR-003).
        let ts = TempStore::new();
        let project_id = index_fixture(&ts).await;
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;

        let err = client
            .call_tool(call(
                "code.search",
                json!({ "project_id": project_id, "query": "" }),
            ))
            .await
            .expect("structured error result");
        assert_eq!(error_code(&err), "INVALID_INPUT", "empty query → INVALID_INPUT");
        let v = text_of(&err);
        assert!(
            v["detail"]
                .as_str()
                .unwrap()
                .contains("non-empty query"),
            "detail explains the precondition"
        );

        task.abort();
    }

    #[tokio::test]
    async fn b2_search_whitespace_query_is_invalid_input() {
        // B2 fix: whitespace-only query is the same precondition failure.
        let ts = TempStore::new();
        let project_id = index_fixture(&ts).await;
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;

        let err = client
            .call_tool(call(
                "code.search",
                json!({ "project_id": project_id, "query": "   \t  " }),
            ))
            .await
            .expect("structured error result");
        assert_eq!(error_code(&err), "INVALID_INPUT", "whitespace query → INVALID_INPUT");

        task.abort();
    }

    // ---------- Session-survives + happy-path regression ---------------

    #[tokio::test]
    async fn session_survives_all_not_found_and_invalid_input_errors() {
        // REQ-MS-005: structured errors never tear down the session — every
        // tool call returns a CallToolResult regardless of the failure
        // path. This test fires the four B1 + B2 errors in sequence and
        // then re-asserts the happy path still resolves the symbol.
        let ts = TempStore::new();
        let project_id = index_fixture(&ts).await;
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;

        // B1 cases (unknown → NOT_FOUND).
        for (tool, args) in [
            (
                "code.symbol_lookup",
                json!({ "project_id": project_id, "symbol": "absent_zz" }),
            ),
            (
                "code.callers_of",
                json!({ "project_id": project_id, "symbol": "absent_zz" }),
            ),
            (
                "code.callees_of",
                json!({ "project_id": project_id, "symbol": "absent_zz" }),
            ),
            (
                "code.search",
                json!({ "project_id": project_id, "query": "absent_zz" }),
            ),
        ] {
            let err = client.call_tool(call(tool, args)).await.expect("err result");
            assert_eq!(error_code(&err), "NOT_FOUND", "{tool} empty-result");
        }

        // B2 case (empty query → INVALID_INPUT).
        let err = client
            .call_tool(call(
                "code.search",
                json!({ "project_id": project_id, "query": "" }),
            ))
            .await
            .expect("err result");
        assert_eq!(error_code(&err), "INVALID_INPUT", "search empty");

        // Happy path still works after the error storm.
        let symbol = client
            .call_tool(call(
                "code.symbol_lookup",
                json!({ "project_id": project_id, "symbol": "leaf" }),
            ))
            .await
            .expect("lookup ok after errors");
        assert_eq!(text_of(&symbol)["symbol"]["artifact_id"], "leaf");

        task.abort();
    }
}
