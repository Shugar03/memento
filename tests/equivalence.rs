//! T-102 — integration harness + MCP↔CLI equivalence (REQ-MS-006).
//!
//! The two user surfaces must be interchangeable: the same input through
//! either surface must produce the same ids, the same provenance and the
//! same ranking — "modulo serialization" (the MCP tool result JSON and the
//! CLI `--json` output carry the same canonical fields, REQ-MS-006).
//!
//! The harness runs BOTH surfaces against the SAME on-disk store:
//!
//! * CLI side: the real `memento` binary via `assert_cmd` (each command is
//!   its own process, exactly like production).
//! * MCP side: `McpServer::from_app` over the real store the CLI wrote,
//!   with the tenant resolved from the environment through the REAL
//!   `TenantResolverImpl` (the same credential path as production startup,
//!   REQ-MS-003), driven by an in-process rmcp client over
//!   `tokio::io::duplex`.
//!
//! Windows note: LanceDB holds file locks while a store is open, so the two
//! surfaces are never open at the same instant — the MCP server is dropped
//! before a CLI command touches the store again.
//!
//! Covered scenarios:
//! * Ingest via CLI, then ingest the same text via MCP → identical chunk
//!   ids + doc id (tenant-scoped content-hash dedup, REQ-MC-005, must be
//!   surface-independent).
//! * Search via both surfaces → identical ordered hit ids, identical BM25
//!   scores, identical full provenance (REQ-MS-006 ranking equivalence).
//! * get_chunk via both surfaces → identical text + provenance.
//! * Fresh MCP writes are visible to a later CLI process (one shared store,
//!   not just dedup).
//! * Mixed-source corpus (CLI + MCP docs) → identical ranking on both
//!   surfaces.
//! * `code index` is CLI-only (REQ-CK-002); the read-only MCP code tools
//!   serve the same index and the CLI's own `code status` agrees.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use assert_cmd::Command;
use memento_application::{AppService, SystemClock};
use memento_mcp::McpServer;
use memento_parse::ParseService;
use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
use memento_tenant::{BearerToken, TenantResolverImpl, default_workspace_id};
use rmcp::model::CallToolRequestParams;
use rmcp::{ClientHandler, ServiceExt};
use serde_json::{Value, json};

/// Process-env guard: the MCP server resolves credentials from the real
/// environment (the same variables the CLI reads, REQ-TA-002/003), and
/// `std::env::set_var` is thread-unsafe (unsafe in edition 2024). The
/// guard is held ONLY around the synchronous mutation+resolution inside
/// [`mcp_server_for`] — never across an await (clippy
/// `await_holding_lock`). CLI children are immune to stray process env:
/// [`authed`] sets the vars explicitly and [`provisioned`] removes them.
static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---- CLI helpers (assert_cmd, real binary) --------------------------------

fn bin() -> Command {
    Command::cargo_bin("memento").expect("binary")
}

/// JSON stdout of a successful run.
fn json_of(out: &std::process::Output) -> Value {
    assert!(
        out.status.success(),
        "expected success, got {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

/// Provision a tenant on a temp root; returns (root, token). Credential
/// env vars are explicitly removed so a stray process env from a parallel
/// test can never affect the bootstrap.
fn provisioned() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = bin()
        .env_remove("MEMENTO_TOKEN")
        .env_remove("MEMENTO_AGENT_ID")
        .args(["--json", "--root"])
        .arg(dir.path())
        .args(["tenant", "create", "--name", "dev"])
        .output()
        .expect("run create");
    let v = json_of(&out);
    (dir, v["token"].as_str().expect("token").to_string())
}

/// A command pre-loaded with credentials + no-embeddings for `root`.
fn authed(root: &Path, token: &str) -> Command {
    let mut cmd = bin();
    cmd.env("MEMENTO_ROOT", root)
        .env("MEMENTO_TOKEN", token)
        .env("MEMENTO_AGENT_ID", "test-agent")
        .arg("--no-embeddings");
    cmd
}

fn cli_ingest_text(root: &Path, token: &str, text: &str) -> Value {
    let out = authed(root, token)
        .args(["--json", "ingest", "text", text])
        .output()
        .expect("cli ingest");
    json_of(&out)
}

fn cli_search(root: &Path, token: &str, query: &str) -> Value {
    let out = authed(root, token)
        .args(["--json", "search", query])
        .output()
        .expect("cli search");
    json_of(&out)
}

fn cli_get_chunk(root: &Path, token: &str, id: &str) -> Value {
    let out = authed(root, token)
        .args(["--json", "get-chunk", id])
        .output()
        .expect("cli get-chunk");
    json_of(&out)
}

// ---- MCP side (in-process rmcp client over the shared store) ---------------

struct TestClient;

impl ClientHandler for TestClient {}

/// A parse boundary that is never invoked (equivalence tests ingest text
/// only; the document path is covered by the crate suites).
fn never_parse() -> Arc<dyn memento_ports::ParsePort> {
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

/// Build the MCP server for the CLI-provisioned tenant at `root`: the
/// tenant is resolved from the process environment (the production channel,
/// REQ-MS-003) and the application service opens the REAL store the CLI
/// wrote. `--no-embeddings` state is mirrored with no embedder — FTS is
/// fully functional without vectors (REQ-MR-001).
///
/// # Panics
///
/// Panics if the tenant cannot be resolved or the store cannot be opened —
/// the test cannot proceed without both.
async fn mcp_server_for(root: &Path, token: &str) -> McpServer {
    // Env mutation + resolution are synchronous; the guard never spans an
    // await (edition 2024 marks set_var unsafe — thread-unsafe with
    // concurrent get_var; the lock makes it sound).
    let ctx = {
        let _g = env_guard();
        unsafe {
            std::env::set_var("MEMENTO_TOKEN", token);
            std::env::set_var("MEMENTO_AGENT_ID", "test-agent");
        }
        TenantResolverImpl::open(root)
            .resolve_from_env()
            .expect("resolver binds the CLI-provisioned tenant")
    };
    let app = AppService::open(&ctx, root, never_parse(), None, Arc::new(SystemClock))
        .await
        .expect("app opens on the CLI-provisioned store");
    McpServer::from_app(app, ctx, memento_i18n::Locale::Es)
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

/// Call a tool, asserting a NON-error result, and parse its JSON text.
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

/// Tear down the MCP side and wait for the server task to finish, so the
/// store's file locks are released before a CLI process opens it again.
async fn stop_mcp(
    client: rmcp::service::RunningService<rmcp::RoleClient, TestClient>,
    task: tokio::task::JoinHandle<()>,
) {
    drop(client);
    task.abort();
    let _ = task.await;
}

// ---- fixtures ---------------------------------------------------------------

/// A Spanish text long enough to produce several chunks (~1300+ tokens):
/// ~5 chunks of 256-300 tokens. The topic words repeat so FTS ranking has
/// structure to compare across surfaces.
fn multi_chunk_doc() -> String {
    const SENTENCES: [&str; 6] = [
        "La memoria es un río subterráneo que fluye entre documentos antiguos y nuevos.",
        "Cada archivo de la historia guarda conocimiento valioso para el futuro.",
        "El río de la memoria arrastra recuerdos, fechas y nombres de personas.",
        "Los documentos de la biblioteca contienen la historia completa de la región.",
        "El conocimiento fluye como el agua cuando los archivos se organizan bien.",
        "Memoria y documento se unen para preservar la historia de cada generación.",
    ];
    let mut doc = String::new();
    for i in 0..100 {
        doc.push_str(SENTENCES[i % SENTENCES.len()]);
        doc.push('\n');
    }
    doc
}

const DOC_RIVER: &str = "El río de la memoria fluye entre documentos de la historia.\n\
    Los archivos del río guardan el conocimiento de la región.\n\
    La memoria del pueblo se conserva en documentos antiguos.\n";

const DOC_MOUNTAIN: &str = "La montaña guarda la nieve del invierno en sus laderas.\n\
    Los excursionistas suben la montaña cada fin de semana.\n\
    La cumbre de la montaña se ve desde la ciudad.\n";

const DOC_LAKE: &str = "El lago refleja la memoria de los bosques que lo rodean.\n\
    Los documentos del lago cuentan la historia de sus orillas.\n\
    La memoria del lago es profunda y silenciosa.\n";

/// Rust fixture: an entry → mid → leaf chain in `src/a.rs` PLUS a
/// cross-module call into `src/b.rs` (same shape as the okf adapter and
/// MCP code-tool fixtures; the L3 module edge needs ≥2 modules).
fn write_chain_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src")).expect("fixture src dir");
    std::fs::write(
        root.join("src/a.rs"),
        "fn entry() { mid(); helper(); }\nfn mid() { leaf(); }\nfn leaf() {}\n",
    )
    .expect("fixture a.rs");
    std::fs::write(root.join("src/b.rs"), "fn helper() {}\n").expect("fixture b.rs");
}

// ---- the equivalence suite ---------------------------------------------------

/// Provenance equivalence across surfaces. Every field must be identical
/// EXCEPT `created_at`, which crosses the surfaces in two RFC3339 renderings
/// of the same instant (MCP: `...Z`, CLI: `...+00:00`) — the serialization
/// difference REQ-MS-006 explicitly allows.
fn assert_same_provenance(a: &Value, b: &Value) {
    for key in [
        "source",
        "doc_id",
        "chunk_id",
        "embedding_model_version",
        "tenant_id",
        "workspace_id",
        "agent_id",
    ] {
        assert_eq!(a[key], b[key], "provenance.{key} identical across surfaces");
    }
    let t_a = chrono::DateTime::parse_from_rfc3339(a["created_at"].as_str().expect("ts"))
        .expect("MCP timestamp parses");
    let t_b = chrono::DateTime::parse_from_rfc3339(b["created_at"].as_str().expect("ts"))
        .expect("CLI timestamp parses");
    assert_eq!(t_a, t_b, "created_at is the same instant");
}

#[tokio::test]
async fn mcp_and_cli_return_identical_ids_provenance_and_ranking() {
    // T-102 acceptance: ingest+search via MCP and CLI → identical ids,
    // provenance and ranking (REQ-MS-006).
    let (dir, token) = provisioned();
    let root = dir.path().to_path_buf();
    let doc = multi_chunk_doc();

    // 1. CLI surface first: ingest → search → get_chunk.
    let cli_ingest = cli_ingest_text(&root, &token, &doc);
    let cli_ids: Vec<String> = cli_ingest["chunk_ids"]
        .as_array()
        .expect("chunk_ids")
        .iter()
        .map(|v| v.as_str().expect("id").to_string())
        .collect();
    assert!(
        cli_ids.len() >= 4,
        "multi-chunk doc produced {} chunks",
        cli_ids.len()
    );
    let cli_hits = cli_search(&root, &token, "memoria");
    let hits = cli_hits["hits"].as_array().expect("hits");
    assert!(!hits.is_empty(), "CLI search must find the ingested doc");
    let ws = hits[0]["provenance"]["workspace_id"]
        .as_str()
        .expect("workspace_id")
        .to_string();
    let cli_first = cli_get_chunk(&root, &token, &cli_ids[0]);

    // 2. MCP server over the SAME on-disk store, tenant from the real env
    //    resolution path.
    let server = mcp_server_for(&root, &token).await;
    let (client, task) = pair(server).await;

    // Ingesting the SAME text through MCP hits the tenant-scoped content
    // hash (REQ-MC-005): the ids must be exactly the CLI's ids — the two
    // ingest pipelines are equivalent.
    let mcp_ingest = call_ok(&client, "memory.ingest_text", json!({ "text": doc })).await;
    let mcp_ids: Vec<String> = mcp_ingest["chunk_ids"]
        .as_array()
        .expect("chunk_ids")
        .iter()
        .map(|v| v.as_str().expect("id").to_string())
        .collect();
    assert_eq!(mcp_ids, cli_ids, "identical chunk ids across surfaces");
    assert_eq!(
        mcp_ingest["doc_id"], cli_ingest["doc_id"],
        "identical doc id across surfaces"
    );

    // Same query through MCP → identical ordered hits: ids, BM25 scores,
    // full provenance (ranking equivalence).
    let mcp_search = call_ok(
        &client,
        "memory.search",
        json!({ "query": "memoria", "workspace_id": ws, "top_k": 20 }),
    )
    .await;
    let mcp_hits = mcp_search["hits"].as_array().expect("hits");
    assert_eq!(mcp_hits.len(), hits.len(), "same result count");
    for (cli, mcp) in hits.iter().zip(mcp_hits.iter()) {
        assert_eq!(cli["chunk_id"], mcp["chunk_id"], "identical rank order");
        let (s_cli, s_mcp) = (
            cli["score"].as_f64().expect("score"),
            mcp["score"].as_f64().expect("score"),
        );
        assert!(
            (s_cli - s_mcp).abs() < 1e-6,
            "identical BM25 score: {s_cli} vs {s_mcp}"
        );
        assert_same_provenance(&cli["provenance"], &mcp["provenance"]);
    }

    // get_chunk equivalence: same id → same text + provenance.
    let mcp_chunk = call_ok(
        &client,
        "memory.get_chunk",
        json!({ "chunk_id": cli_ids[0] }),
    )
    .await;
    assert_eq!(
        mcp_chunk["chunk"]["text"], cli_first["chunk"]["text"],
        "identical chunk text"
    );
    assert_same_provenance(
        &mcp_chunk["chunk"]["provenance"],
        &cli_first["chunk"]["provenance"],
    );

    // 3. MCP writes are visible to a NEW CLI process — the equivalence is
    //    one shared store, not just dedup.
    let fresh = "El río nuevo de documentos fluye con fuerza.";
    let fresh_ids = call_ok(&client, "memory.ingest_text", json!({ "text": fresh })).await;
    let fresh_id = fresh_ids["chunk_ids"][0]
        .as_str()
        .expect("fresh id")
        .to_string();
    stop_mcp(client, task).await;

    let cli_after = cli_search(&root, &token, "río nuevo");
    let found: Vec<&str> = cli_after["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| h["chunk_id"].as_str().expect("id"))
        .collect();
    assert!(
        found.contains(&fresh_id.as_str()),
        "MCP write visible to a later CLI process: {found:?}"
    );
}

#[tokio::test]
async fn ranking_is_identical_with_mixed_source_docs() {
    // Two docs via CLI, one via MCP, then both surfaces rank the full set
    // identically.
    let (dir, token) = provisioned();
    let root = dir.path().to_path_buf();
    let ws = default_workspace_id(
        BearerToken::parse(&token)
            .expect("token parses")
            .tenant_id(),
    )
    .to_string();

    cli_ingest_text(&root, &token, DOC_RIVER);
    cli_ingest_text(&root, &token, DOC_MOUNTAIN);

    let server = mcp_server_for(&root, &token).await;
    let (client, task) = pair(server).await;
    let mcp_ids = call_ok(&client, "memory.ingest_text", json!({ "text": DOC_LAKE })).await;
    assert_eq!(
        mcp_ids["chunk_ids"].as_array().expect("chunk_ids").len(),
        1,
        "3-sentence doc → one chunk (REQ-MC-003 short-doc rule)"
    );

    let mcp_search = call_ok(
        &client,
        "memory.search",
        json!({ "query": "memoria", "workspace_id": ws, "top_k": 20 }),
    )
    .await;
    let mcp_rank: Vec<String> = mcp_search["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| h["chunk_id"].as_str().expect("id").to_string())
        .collect();
    // "memoria" only occurs in DOC_RIVER and DOC_LAKE — DOC_MOUNTAIN is the
    // non-matching control. Both surfaces must agree on the 2 hits.
    assert_eq!(mcp_rank.len(), 2, "river + lake match, mountain does not");
    stop_mcp(client, task).await;

    let cli_after = cli_search(&root, &token, "memoria");
    let cli_rank: Vec<String> = cli_after["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| h["chunk_id"].as_str().expect("id").to_string())
        .collect();
    assert_eq!(
        cli_rank, mcp_rank,
        "identical ranking across surfaces on the mixed corpus"
    );
}

#[tokio::test]
async fn cli_code_index_is_visible_to_mcp_code_tools_and_cli_status() {
    // Indexing is CLI-only (REQ-CK-002); the read-only MCP code tools
    // (T-073) and the CLI's own `code status` (T-084) must agree on the
    // same index.
    let (dir, token) = provisioned();
    let root = dir.path().to_path_buf();
    let repo = tempfile::tempdir().expect("fixture repo");
    write_chain_fixture(repo.path());

    let out = authed(&root, &token)
        .args(["--json", "code", "index"])
        .arg(repo.path())
        .output()
        .expect("code index");
    let v = json_of(&out);
    let project_id = v["project_id"].as_str().expect("project_id").to_string();
    assert!(
        v["graph_edge_count"].as_u64().expect("edges") >= 1,
        "cross-module edge indexed"
    );

    let server = mcp_server_for(&root, &token).await;
    let (client, task) = pair(server).await;

    let overview = call_ok(
        &client,
        "code.project_overview",
        json!({ "project_id": project_id }),
    )
    .await;
    assert_eq!(overview["project_id"], project_id, "MCP sees the CLI index");

    let graph = call_ok(
        &client,
        "code.graph_dump",
        json!({ "project_id": project_id }),
    )
    .await;
    let nodes = graph["graph"]["nodes"].as_array().expect("nodes array");
    assert!(nodes.len() >= 3, "fixture symbols indexed: {nodes:?}");
    let edges = graph["graph"]["edges"].as_array().expect("edges array");
    // Node ids are kind-qualified (`functions/src/a/entry`); edge fields
    // are `source`/`target`. The cross-module call edge src/a.rs → src/b.rs
    // must be present (REQ-CK-007).
    assert!(
        edges.iter().any(|e| {
            e["source"]
                .as_str()
                .is_some_and(|s| s.ends_with("/a/entry"))
                && e["target"]
                    .as_str()
                    .is_some_and(|t| t.ends_with("/b/helper"))
        }),
        "cross-module edge src/a.rs → src/b.rs present in {edges:?}"
    );
    stop_mcp(client, task).await;

    let out = authed(&root, &token)
        .args(["--json", "code", "status"])
        .output()
        .expect("code status");
    let v = json_of(&out);
    assert_eq!(
        v["project_id"].as_str().expect("project_id"),
        project_id,
        "CLI status agrees with MCP"
    );
    assert_eq!(v["layers"]["l1_bundles"], true, "L1 present");
    assert_eq!(v["layers"]["l4_summary"], true, "L4 present");
}
