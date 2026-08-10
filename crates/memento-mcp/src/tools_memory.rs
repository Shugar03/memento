//! memory.* tools (T-072, REQ-MS-002): thin delegation to the application
//! layer. NO business logic here (REQ-MS-006) — each tool validates its
//! parameters, delegates to [`AppService`] and shapes the response. Tool
//! descriptions come from the memento-i18n ES-first tables (REQ-MS-004);
//! errors are structured and bilingual (REQ-MS-005, see [`crate::errors`]).

use std::str::FromStr;

use memento_application::context_fit::ContextFitRequest;
use memento_domain::{ChunkId, DocId, DomainError, SourceKind, WorkspaceId};
use memento_ports::{
    DeleteScope, IngestDocumentRequest, IngestResult, IngestTextRequest, Metadata, SearchHit,
    SearchQuery,
};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::McpServer;
use crate::errors::ToolError;
use crate::source_label;

/// Default candidate count when the client omits `top_k` (REQ-MR-001: the
/// store default is 20).
const DEFAULT_TOP_K: usize = 20;

/// `0` means "not provided" for MCP clients — normalize to the default.
fn top_k_or_default(top_k: usize) -> usize {
    if top_k == 0 { DEFAULT_TOP_K } else { top_k }
}

fn parse_workspace(raw: &str) -> Result<WorkspaceId, ToolError> {
    WorkspaceId::from_str(raw).map_err(|_| {
        ToolError(DomainError::InvalidInput {
            message: format!("workspace_id is not a valid uuid: {raw}"),
        })
    })
}

fn parse_chunk(raw: &str) -> Result<ChunkId, ToolError> {
    ChunkId::from_str(raw).map_err(|_| {
        ToolError(DomainError::InvalidInput {
            message: format!("chunk_id is not a valid uuid: {raw}"),
        })
    })
}

fn parse_doc(raw: &str) -> Result<DocId, ToolError> {
    DocId::from_str(raw).map_err(|_| {
        ToolError(DomainError::InvalidInput {
            message: format!("doc_id is not a valid uuid: {raw}"),
        })
    })
}

/// Parse the `source` parameter ("text" | "markdown" | "document:<ext>").
fn parse_source(raw: &str) -> Result<SourceKind, ToolError> {
    match raw {
        "text" => Ok(SourceKind::Text),
        "markdown" => Ok(SourceKind::Markdown),
        other => other
            .strip_prefix("document:")
            .map(|ext| SourceKind::Document(ext.to_string()))
            .ok_or_else(|| {
                ToolError(DomainError::InvalidInput {
                    message: format!(
                        "source must be 'text', 'markdown' or 'document:<ext>', got: {raw}"
                    ),
                })
            }),
    }
}

// ---------- output DTOs (shaped, schema'd — never leak domain Debug) ------

#[derive(Serialize, JsonSchema)]
struct ProvenanceDto {
    source: String,
    doc_id: String,
    chunk_id: String,
    created_at: String,
    embedding_model_version: String,
    tenant_id: String,
    workspace_id: String,
    agent_id: String,
}

#[derive(Serialize, JsonSchema)]
struct HitDto {
    chunk_id: String,
    score: f32,
    text: String,
    provenance: ProvenanceDto,
}

#[derive(Serialize, JsonSchema)]
struct SearchOutput {
    hits: Vec<HitDto>,
}

#[derive(Serialize, JsonSchema)]
struct IngestOutput {
    chunk_ids: Vec<String>,
    doc_id: String,
    chore_id: Option<String>,
}

#[derive(Serialize, JsonSchema)]
struct ChunkDto {
    id: String,
    doc_id: String,
    text: String,
    created_at: String,
    provenance: ProvenanceDto,
}

#[derive(Serialize, JsonSchema)]
struct GetChunkOutput {
    chunk: Option<ChunkDto>,
}

#[derive(Serialize, JsonSchema)]
struct OkOutput {
    ok: bool,
}

#[derive(Serialize, JsonSchema)]
struct DeleteOutput {
    deleted_count: usize,
    freed_bytes: u64,
    chore_id: String,
}

#[derive(Serialize, JsonSchema)]
struct ContextFitOutput {
    chunks: Vec<HitDto>,
    total_tokens: usize,
    reason: Option<String>,
}

// ---------- input params (schemars JSON schemas for tools/list) -----------

#[derive(Deserialize, JsonSchema)]
struct SearchParams {
    query: String,
    workspace_id: String,
    #[serde(default)]
    top_k: usize,
    #[serde(default)]
    rrf_enabled: bool,
}

#[derive(Deserialize, JsonSchema)]
struct IngestTextParams {
    text: String,
    #[serde(default)]
    doc_id: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize, JsonSchema)]
struct IngestDocumentParams {
    /// Base64-encoded document blob (JSON has no binary type).
    blob_b64: String,
    /// "text" | "markdown" | "document:<ext>".
    source: String,
    #[serde(default)]
    doc_id: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize, JsonSchema)]
struct GetChunkParams {
    chunk_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct FeedbackParams {
    chunk_id: String,
    useful: bool,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct DeleteParams {
    /// "chunk" | "doc" | "workspace" | "tenant".
    scope: String,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ContextFitParams {
    query: String,
    budget_tokens: usize,
    workspace_id: String,
    #[serde(default)]
    top_k: usize,
    #[serde(default)]
    rrf_enabled: bool,
}

// ---------- the tools ------------------------------------------------------

/// The 7 `memory.*` tools, generated into `McpServer::memory_tools()`.
#[tool_router(router = memory_tools, vis = "pub(crate)")]
impl McpServer {
    /// memory.search — BM25 by default, RRF hybrid behind the toggle
    /// (REQ-MR-001/002/003). Workspace is mandatory (REQ-MR-006).
    #[tool(name = "memory.search")]
    async fn memory_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<Json<SearchOutput>, ToolError> {
        let workspace_id = parse_workspace(&p.workspace_id)?;
        let hits = self
            .app
            .search(
                &self.ctx,
                SearchQuery {
                    query: p.query,
                    top_k: top_k_or_default(p.top_k),
                    workspace_id,
                    rrf_enabled: p.rrf_enabled,
                    filters: None,
                },
            )
            .await?;
        Ok(Json(SearchOutput {
            hits: hits.into_iter().map(hit_dto).collect(),
        }))
    }

    /// memory.ingest_text — chunk → embed → store (REQ-MC-001).
    #[tool(name = "memory.ingest_text")]
    async fn memory_ingest_text(
        &self,
        Parameters(p): Parameters<IngestTextParams>,
    ) -> Result<Json<IngestOutput>, ToolError> {
        let doc_id = p.doc_id.as_deref().map(parse_doc).transpose()?;
        let result = self
            .app
            .ingest_text(
                &self.ctx,
                IngestTextRequest {
                    text: p.text,
                    doc_id,
                    metadata: p.metadata.map(Metadata),
                },
            )
            .await?;
        Ok(Json(ingest_output(result)))
    }

    /// memory.ingest_document — normalize a blob (base64) through the
    /// single normalization boundary (REQ-MC-002).
    #[tool(name = "memory.ingest_document")]
    async fn memory_ingest_document(
        &self,
        Parameters(p): Parameters<IngestDocumentParams>,
    ) -> Result<Json<IngestOutput>, ToolError> {
        use base64::Engine;
        let blob = base64::engine::general_purpose::STANDARD
            .decode(p.blob_b64.as_bytes())
            .map_err(|e| {
                ToolError(DomainError::InvalidInput {
                    message: format!("blob_b64 is not valid base64: {e}"),
                })
            })?;
        let source_hint = parse_source(&p.source)?;
        let doc_id = p.doc_id.as_deref().map(parse_doc).transpose()?;
        let result = self
            .app
            .ingest_document(
                &self.ctx,
                IngestDocumentRequest {
                    blob,
                    source_hint,
                    doc_id,
                    metadata: p.metadata.map(Metadata),
                },
            )
            .await?;
        Ok(Json(ingest_output(result)))
    }

    /// memory.get_chunk — one chunk by id with provenance (REQ-MR-005;
    /// unknown/foreign ids resolve to `null`, never an error).
    #[tool(name = "memory.get_chunk")]
    async fn memory_get_chunk(
        &self,
        Parameters(p): Parameters<GetChunkParams>,
    ) -> Result<Json<GetChunkOutput>, ToolError> {
        let id = parse_chunk(&p.chunk_id)?;
        let chunk = self.app.get_chunk(&self.ctx, &id).await?;
        Ok(Json(GetChunkOutput {
            chunk: chunk.map(chunk_dto),
        }))
    }

    /// memory.feedback — attach a usefulness signal to a chunk
    /// (REQ-ML-001; unknown chunk → structured error).
    #[tool(name = "memory.feedback")]
    async fn memory_feedback(
        &self,
        Parameters(p): Parameters<FeedbackParams>,
    ) -> Result<Json<OkOutput>, ToolError> {
        let chunk_id = parse_chunk(&p.chunk_id)?;
        self.app
            .feedback(&self.ctx, chunk_id, p.useful, p.reason)
            .await?;
        Ok(Json(OkOutput { ok: true }))
    }

    /// memory.delete — hard delete by scope (REQ-ML-002): chunk, doc,
    /// workspace, or the whole tenant.
    #[tool(name = "memory.delete")]
    async fn memory_delete(
        &self,
        Parameters(p): Parameters<DeleteParams>,
    ) -> Result<Json<DeleteOutput>, ToolError> {
        let scope = match p.scope.as_str() {
            "chunk" => {
                let id = p.id.as_deref().ok_or_else(|| {
                    ToolError(DomainError::InvalidInput {
                        message: "delete scope 'chunk' requires an id".into(),
                    })
                })?;
                DeleteScope::Chunk {
                    id: parse_chunk(id)?,
                }
            }
            "doc" => {
                let id = p.id.as_deref().ok_or_else(|| {
                    ToolError(DomainError::InvalidInput {
                        message: "delete scope 'doc' requires an id".into(),
                    })
                })?;
                DeleteScope::Doc { id: parse_doc(id)? }
            }
            "workspace" => {
                let id = p.id.as_deref().ok_or_else(|| {
                    ToolError(DomainError::InvalidInput {
                        message: "delete scope 'workspace' requires an id".into(),
                    })
                })?;
                DeleteScope::Workspace {
                    id: parse_workspace(id)?,
                }
            }
            "tenant" => {
                // Only the bound tenant can be erased from this process
                // (REQ-TA-001/002): absent id → the bound tenant.
                let id = match p.id.as_deref() {
                    Some(raw) => memento_domain::TenantId::from_str(raw).map_err(|_| {
                        ToolError(DomainError::InvalidInput {
                            message: format!("tenant id is not a valid uuid: {raw}"),
                        })
                    })?,
                    None => *self.ctx.tenant_id(),
                };
                DeleteScope::Tenant { id }
            }
            other => {
                return Err(ToolError(DomainError::InvalidInput {
                    message: format!(
                        "scope must be one of 'chunk', 'doc', 'workspace', 'tenant', got: {other}"
                    ),
                }));
            }
        };
        let report = self.app.delete(&self.ctx, scope).await?;
        Ok(Json(DeleteOutput {
            deleted_count: report.deleted_count,
            freed_bytes: report.freed_bytes,
            chore_id: report.chore_id.to_string(),
        }))
    }

    /// memory.context_fit — greedy token-budget context packing
    /// (REQ-MR-004, design D6).
    #[tool(name = "memory.context_fit")]
    async fn memory_context_fit(
        &self,
        Parameters(p): Parameters<ContextFitParams>,
    ) -> Result<Json<ContextFitOutput>, ToolError> {
        let workspace_id = parse_workspace(&p.workspace_id)?;
        let result = self
            .app
            .context_fit(
                &self.ctx,
                ContextFitRequest {
                    query: p.query,
                    budget_tokens: p.budget_tokens,
                    workspace_id,
                    top_k: top_k_or_default(p.top_k),
                    rrf_enabled: p.rrf_enabled,
                },
            )
            .await?;
        Ok(Json(ContextFitOutput {
            chunks: result.chunks.into_iter().map(hit_dto).collect(),
            total_tokens: result.total_tokens,
            reason: result.reason,
        }))
    }
}

// ---------- DTO construction helpers ---------------------------------------

fn hit_dto(hit: SearchHit) -> HitDto {
    HitDto {
        chunk_id: hit.chunk_id.to_string(),
        score: hit.score,
        text: hit.text,
        provenance: provenance_dto(&hit.provenance),
    }
}

fn chunk_dto(chunk: memento_domain::MemoryChunk) -> ChunkDto {
    ChunkDto {
        id: chunk.id.to_string(),
        doc_id: chunk.doc_id.to_string(),
        text: chunk.text,
        created_at: chunk.created_at.to_rfc3339(),
        provenance: provenance_dto(&chunk.provenance),
    }
}

fn provenance_dto(provenance: &memento_domain::Provenance) -> ProvenanceDto {
    ProvenanceDto {
        source: source_label(&provenance.source),
        doc_id: provenance.doc_id.to_string(),
        chunk_id: provenance.chunk_id.to_string(),
        created_at: provenance.created_at.to_rfc3339(),
        embedding_model_version: provenance.embedding_model_version.clone(),
        tenant_id: provenance.tenant_id.to_string(),
        workspace_id: provenance.workspace_id.to_string(),
        agent_id: provenance.agent_id.to_string(),
    }
}

fn ingest_output(result: IngestResult) -> IngestOutput {
    IngestOutput {
        chunk_ids: result.chunk_ids.iter().map(|id| id.to_string()).collect(),
        doc_id: result.doc_id.to_string(),
        chore_id: result.chore_id.map(|id| id.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use crate::McpServer;
    use memento_application::{AppService, SystemClock};
    use memento_parse::ParseService;
    use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
    use memento_testkit::{StubEmbedPort, TempStore};
    use rmcp::model::CallToolRequestParams;
    use rmcp::{ClientHandler, ServiceExt};
    use serde_json::{Value, json};
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

    /// Text content of a tool result (the first text block).
    fn text_of(result: &rmcp::model::CallToolResult) -> Value {
        let text = result
            .content
            .iter()
            .find_map(|block| block.as_text())
            .map(|t| t.text.clone())
            .expect("text block");
        serde_json::from_str(&text).expect("tool output is JSON")
    }

    #[tokio::test]
    async fn memory_tools_are_callable_end_to_end_via_the_mcp_client() {
        // T-072 acceptance: every memory.* tool callable end-to-end through
        // the in-process client, with provenance and chore ids observable.
        let ts = TempStore::new();
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;
        let ws = ts.workspace_id().to_string();

        // ingest_text → ids + chore.
        let ingest = client
            .call_tool(call(
                "memory.ingest_text",
                json!({ "text": "La memoria es un río subterráneo que fluye.", "metadata": {"title": "Nota"} }),
            ))
            .await
            .expect("ingest_text ok");
        assert_ne!(ingest.is_error, Some(true));
        let ingest_v = text_of(&ingest);
        let chunk_ids = ingest_v["chunk_ids"]
            .as_array()
            .expect("chunk_ids array")
            .clone();
        assert_eq!(chunk_ids.len(), 1);
        assert!(!ingest_v["doc_id"].as_str().unwrap().is_empty());
        assert!(ingest_v["chore_id"].as_str().is_some(), "chore observable");

        // search → hit with provenance.
        let search = client
            .call_tool(call(
                "memory.search",
                json!({ "query": "memoria", "workspace_id": ws }),
            ))
            .await
            .expect("search ok");
        assert_ne!(search.is_error, Some(true));
        let search_v = text_of(&search);
        let hits = search_v["hits"].as_array().expect("hits array");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["chunk_id"], chunk_ids[0]);
        assert_eq!(hits[0]["provenance"]["workspace_id"], ws);
        assert_eq!(hits[0]["provenance"]["source"], "text");
        assert_eq!(hits[0]["provenance"]["agent_id"], ts.agent_id().to_string());

        // get_chunk → chunk text + provenance; unknown id → null.
        let chunk = client
            .call_tool(call(
                "memory.get_chunk",
                json!({ "chunk_id": chunk_ids[0].as_str().unwrap() }),
            ))
            .await
            .expect("get_chunk ok");
        assert_ne!(chunk.is_error, Some(true));
        let chunk_v = text_of(&chunk);
        assert!(
            chunk_v["chunk"]["text"]
                .as_str()
                .unwrap()
                .contains("memoria"),
            "chunk text round-trips"
        );
        let missing = client
            .call_tool(call(
                "memory.get_chunk",
                json!({ "chunk_id": memento_domain::ChunkId::new().to_string() }),
            ))
            .await
            .expect("get_chunk unknown ok");
        assert_eq!(text_of(&missing)["chunk"], Value::Null, "None, not error");

        // feedback → ok.
        let feedback = client
            .call_tool(call(
                "memory.feedback",
                json!({ "chunk_id": chunk_ids[0].as_str().unwrap(), "useful": true, "reason": "muy útil" }),
            ))
            .await
            .expect("feedback ok");
        assert_eq!(text_of(&feedback)["ok"], true);

        // context_fit → total_tokens ≤ budget.
        let fit = client
            .call_tool(call(
                "memory.context_fit",
                json!({ "query": "memoria", "budget_tokens": 500, "workspace_id": ws }),
            ))
            .await
            .expect("context_fit ok");
        assert_ne!(fit.is_error, Some(true));
        let fit_v = text_of(&fit);
        assert_eq!(fit_v["chunks"].as_array().unwrap().len(), 1);
        assert!(
            fit_v["total_tokens"].as_u64().unwrap() <= 500,
            "fitted set within budget"
        );

        // delete chunk → report; then search returns nothing.
        let delete = client
            .call_tool(call(
                "memory.delete",
                json!({ "scope": "chunk", "id": chunk_ids[0].as_str().unwrap() }),
            ))
            .await
            .expect("delete ok");
        assert_ne!(delete.is_error, Some(true));
        assert_eq!(text_of(&delete)["deleted_count"], 1);

        let after = client
            .call_tool(call(
                "memory.search",
                json!({ "query": "memoria", "workspace_id": ws }),
            ))
            .await
            .expect("search after delete");
        assert_eq!(
            text_of(&after)["hits"].as_array().unwrap().len(),
            0,
            "hard delete visible to search (REQ-ML-002)"
        );

        task.abort();
    }

    #[tokio::test]
    async fn ingest_document_round_trips_base64_blob() {
        // REQ-MC-002 through the MCP surface: a Markdown blob arrives as
        // base64 and normalizes through the real fallback parser.
        use base64::Engine;
        let ts = TempStore::new();
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;

        let markdown = format!("# Notas\n\n{}", memento_testkit::spanish_corpus().join(" "));
        let blob_b64 = base64::engine::general_purpose::STANDARD.encode(markdown.as_bytes());
        let result = client
            .call_tool(call(
                "memory.ingest_document",
                json!({ "blob_b64": blob_b64, "source": "markdown" }),
            ))
            .await
            .expect("ingest_document ok");
        assert_ne!(result.is_error, Some(true));
        let v = text_of(&result);
        assert_eq!(v["chunk_ids"].as_array().unwrap().len(), 1);

        let hit = client
            .call_tool(call(
                "memory.search",
                json!({ "query": "memoria", "workspace_id": ts.workspace_id().to_string() }),
            ))
            .await
            .expect("search ok");
        let hit_v = text_of(&hit);
        assert_eq!(
            hit_v["hits"][0]["provenance"]["source"], "markdown",
            "document source recorded"
        );

        task.abort();
    }

    #[tokio::test]
    async fn invalid_inputs_are_structured_errors() {
        // REQ-MS-005: business validation failures are structured tool
        // errors with the stable code + bilingual payload.
        let ts = TempStore::new();
        let server = test_server(&ts).await;
        let (client, task) = pair(server).await;

        // Bad workspace uuid.
        let err = client
            .call_tool(call(
                "memory.search",
                json!({ "query": "x", "workspace_id": "not-a-uuid" }),
            ))
            .await
            .expect("structured error result");
        assert_eq!(err.is_error, Some(true));
        let v: Value = serde_json::from_str(&err.content[0].as_text().unwrap().text).expect("json");
        assert_eq!(v["code"], "INVALID_INPUT");

        // Unknown chunk feedback → CHUNK_NOT_FOUND (REQ-ML-001).
        let err = client
            .call_tool(call(
                "memory.feedback",
                json!({ "chunk_id": memento_domain::ChunkId::new().to_string(), "useful": true }),
            ))
            .await
            .expect("structured error result");
        let v: Value = serde_json::from_str(&err.content[0].as_text().unwrap().text).expect("json");
        assert_eq!(v["code"], "CHUNK_NOT_FOUND");
        // Bilingual payload present (REQ-MS-004).
        assert!(!v["message_es"].as_str().unwrap().is_empty());
        assert!(!v["message_en"].as_str().unwrap().is_empty());

        task.abort();
    }
}
