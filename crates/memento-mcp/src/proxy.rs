//! MCP stdio → named-pipe proxy (design D1/D4, REQ-DAEMON-008).
//!
//! When a daemon is reachable for the MCP process's (root, tenant, spawn
//! config), the `memento-mcp-server` binary becomes a THIN rmcp client
//! over the daemon's named pipe instead of opening its own AppService +
//! embedder — this is what keeps the model loaded exactly once per tenant
//! (REQ-DAEMON-001 GIVEN-2: "exactly one process holds the model").
//!
//! The proxy presents `Role::McpProxy` in the HELLO handshake; the daemon
//! confines that role to the public 15 tools (REQ-DAEMON-012 role gate).
//! `tools/list` serves the same schemas the direct server exposes
//! ([`crate::router::tool_router`] — schemas only, no AppService
//! needed); `tools/call` maps the tool name to the dispatcher wire
//! envelope, forwards it over the pipe, and renders the response in the
//! same shape the direct tool produces (identical ids/scores across
//! carriers — REQ-MS-006 equivalence).
//!
//! Codec note ("codec-dup flag" per the tasks artifact): the client-side
//! connect/handshake/dispatch glue is a thin re-implementation of the
//! CLI's `memento-cli::transport::pipe_client` — it cannot be shared
//! because memento-cli depends on memento-mcp (a cycle), so the ~60 lines
//! of glue live here over memento-mcp's own `frame`/`handshake`/`daemon`
//! modules (the codec itself is NOT duplicated).
//!
//! The daemon executes `memory.search` today; the other 14 tools are
//! listed (schemas) but return a structured "not delegated yet" error
//! until the remaining dispatcher bodies land (documented follow-up).

use std::path::PathBuf;
use std::time::Duration;

use interprocess::os::windows::named_pipe::{pipe_mode, tokio::PipeStream};
use memento_domain::{DomainError, TenantId};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler};
use serde_json::{Value, json};

use crate::daemon::{DEFAULT_PIPE_TIMEOUT, pipe_name};
use crate::dispatcher::{Command, McpCommand, MemoryTool};
use crate::frame;
use crate::handshake::{Hello, PROTOCOL_VERSION, Role, Welcome};
use crate::router::tool_router;

/// Runtime configuration for the proxy, resolved from the MCP process env
/// (mirrors the daemon's gate + the CLI's `ClientConfig`).
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Storage root (the daemon's bound root; D8 layout).
    pub root: PathBuf,
    /// Raw `MEMENTO_TOKEN` (one-shot HELLO, D3).
    pub token: String,
    /// Agent identity bound to every tool call (REQ-TA-003).
    pub agent_id: String,
    /// The tenant id (pipe-name axis).
    pub tenant_id: String,
    /// Surface locale (`es` | `en`), CONFIG_MISMATCH axis.
    pub locale: Option<String>,
    /// `--no-embeddings` expectation, CONFIG_MISMATCH axis.
    pub no_embeddings: bool,
    /// Per-write/read bound (`MEMENTO_DAEMON_PIPE_TIMEOUT`).
    pub pipe_timeout: Duration,
}

impl ProxyConfig {
    /// Resolve from the process env. Returns `None` when the daemon gate
    /// is disabled (`MEMENTO_NO_DAEMON=1`) or a required var is missing —
    /// the caller then falls back to the direct server.
    pub fn from_env() -> Option<Self> {
        if std::env::var("MEMENTO_NO_DAEMON").ok().as_deref() == Some("1") {
            return None;
        }
        Some(Self {
            root: PathBuf::from(std::env::var("MEMENTO_ROOT").ok()?),
            token: std::env::var("MEMENTO_TOKEN").ok()?,
            agent_id: std::env::var("MEMENTO_AGENT_ID").ok()?,
            tenant_id: std::env::var("MEMENTO_TENANT").ok()?,
            locale: std::env::var("MEMENTO_LOCALE").ok(),
            no_embeddings: std::env::var("MEMENTO_NO_EMBEDDINGS")
                .map(|v| v == "1")
                .unwrap_or(false),
            pipe_timeout: std::env::var("MEMENTO_DAEMON_PIPE_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .map(Duration::from_secs_f64)
                .unwrap_or(DEFAULT_PIPE_TIMEOUT),
        })
    }

    /// The deterministic pipe name for this `(root, tenant)`.
    pub fn pipe_name(&self) -> Result<String, DomainError> {
        let tid: TenantId = self
            .tenant_id
            .parse()
            .map_err(|err| DomainError::InvalidInput {
                message: format!("invalid tenant id: {err}"),
            })?;
        Ok(pipe_name(&self.root, &tid))
    }

    /// Discover the cookie nonce (`<root>/.daemon-<pid>.cookie`).
    fn read_cookie(&self) -> Option<String> {
        let entries = std::fs::read_dir(&self.root).ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix(".daemon-")
                && let Some(stripped) = rest.strip_suffix(".cookie")
                && stripped.chars().all(|c| c.is_ascii_digit())
            {
                let nonce = std::fs::read_to_string(entry.path()).ok()?;
                return Some(nonce.trim().to_string());
            }
        }
        None
    }
}

/// Error connecting to the daemon (the caller falls back to the direct
/// server on any failure — the proxy is an optimization, not a gate).
#[derive(Debug)]
pub struct ProxyConnectError(String);

impl std::fmt::Display for ProxyConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ProxyConnectError {}

/// The stdio→pipe proxy: an rmcp `ServerHandler` whose tool calls are
/// forwarded over the daemon's named pipe. `conn` is behind a
/// `tokio::sync::Mutex` because `ServerHandler::call_tool` takes `&self`
/// while the pipe roundtrip needs `&mut`.
pub struct StdioProxy {
    conn: tokio::sync::Mutex<PipeStream<pipe_mode::Bytes, pipe_mode::Bytes>>,
    welcome: Welcome,
    pipe_timeout: Duration,
}

impl StdioProxy {
    /// Connect to the daemon's pipe and complete the HELLO/WELCOME
    /// handshake as `Role::McpProxy` (REQ-DAEMON-005/012). Fails when no
    /// daemon is reachable, the cookie is missing, or the daemon's fixed
    /// spawn config diverges from the client's (CONFIG_MISMATCH — R3).
    pub async fn connect(config: &ProxyConfig) -> Result<Self, ProxyConnectError> {
        let name = config
            .pipe_name()
            .map_err(|err| ProxyConnectError(err.to_string()))?;
        let conn = tokio::time::timeout(
            config.pipe_timeout,
            PipeStream::connect_by_path(name.as_str()),
        )
        .await
        .map_err(|_| {
            ProxyConnectError(format!(
                "pipe connect timed out after {:?}",
                config.pipe_timeout
            ))
        })?
        .map_err(|err| ProxyConnectError(format!("pipe connect: {err}")))?;
        let cookie = config.read_cookie().ok_or_else(|| {
            ProxyConnectError("cookie file missing; no daemon for this root".into())
        })?;
        let hello = Hello {
            proto: PROTOCOL_VERSION,
            role: Role::McpProxy,
            pid: std::process::id(),
            ppid: 0,
            version: env!("CARGO_PKG_VERSION").to_string(),
            cookie,
            token: config.token.clone(),
            locale: config.locale.clone(),
            no_embeddings: config.no_embeddings,
            staging: std::env::temp_dir(),
        };
        let mut conn = conn;
        let payload = serde_json::to_vec(&hello)
            .map_err(|err| ProxyConnectError(format!("HELLO serialize: {err}")))?;
        tokio::time::timeout(
            config.pipe_timeout,
            frame::write_message(&mut conn, &payload),
        )
        .await
        .map_err(|_| ProxyConnectError("HELLO write timed out".into()))?
        .map_err(|err| ProxyConnectError(format!("HELLO write: {err}")))?;
        let raw = tokio::time::timeout(config.pipe_timeout, frame::read_message(&mut conn))
            .await
            .map_err(|_| ProxyConnectError("WELCOME read timed out".into()))?
            .map_err(|err| ProxyConnectError(format!("WELCOME read: {err}")))?;
        let welcome: Welcome = serde_json::from_slice(&raw)
            .map_err(|err| ProxyConnectError(format!("WELCOME parse: {err}")))?;
        // R3 / REQ-DAEMON-003: refuse a diverging daemon config (never
        // silently run with different semantics).
        if welcome.spawn.locale != config.locale
            || welcome.spawn.no_embeddings != config.no_embeddings
        {
            return Err(ProxyConnectError(format!(
                "CONFIG_MISMATCH: daemon spawn {:?} vs client {:?}",
                welcome.spawn, config
            )));
        }
        Ok(Self {
            conn: tokio::sync::Mutex::new(conn),
            welcome,
            pipe_timeout: config.pipe_timeout,
        })
    }

    /// The daemon's WELCOME envelope (pid + capabilities + spawn config).
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// One framed dispatch over the pipe (client-side of the
    /// REQ-DAEMON-006 envelope). Mirrors the CLI's `DaemonClient::dispatch`
    /// — see the module docs for why this glue is duplicated.
    pub async fn dispatch(&self, cmd: Command) -> Result<Value, ProxyConnectError> {
        let bytes = serde_json::to_vec(&cmd)
            .map_err(|err| ProxyConnectError(format!("command serialize: {err}")))?;
        let mut conn = self.conn.lock().await;
        tokio::time::timeout(self.pipe_timeout, frame::write_message(&mut *conn, &bytes))
            .await
            .map_err(|_| ProxyConnectError("request write timed out".into()))?
            .map_err(|err| ProxyConnectError(format!("request write: {err}")))?;
        let raw = tokio::time::timeout(self.pipe_timeout, frame::read_message(&mut *conn))
            .await
            .map_err(|_| ProxyConnectError("response read timed out".into()))?
            .map_err(|err| ProxyConnectError(format!("response read: {err}")))?;
        serde_json::from_slice(&raw)
            .map_err(|err| ProxyConnectError(format!("response parse: {err}")))
    }
}

/// Map a `tools/call` request to the dispatcher wire envelope. Today only
/// `memory.search` has a real daemon body; anything else returns `None`
/// and the proxy answers with a structured "not delegated yet" error.
fn command_for(name: &str, arguments: Option<rmcp::model::JsonObject>) -> Option<Command> {
    let args: Value = match arguments {
        Some(map) => Value::Object(map),
        None => json!({}),
    };
    match name {
        "memory.search" => Some(Command::Mcp(McpCommand::Memory {
            tool: MemoryTool::Search,
            args,
        })),
        _ => None,
    }
}

impl ServerHandler for StdioProxy {
    /// Server identity: tools only (same as the direct server).
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    /// Serve the SAME 15-tool registry the direct server exposes
    /// (schemas only — no AppService open on this process, REQ-DAEMON-001).
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        let tools = tool_router().list_all();
        async move {
            Ok(ListToolsResult {
                tools,
                ..Default::default()
            })
        }
    }

    /// Single-tool lookup (SDK pre-call validation).
    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_router().get(name).cloned()
    }

    /// Forward a `tools/call` over the daemon pipe (REQ-DAEMON-008).
    /// The daemon's response JSON is rendered into the same content-block
    /// shape the direct tool produces, so both carriers return identical
    /// ids/scores (REQ-MS-006 equivalence).
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + '_ {
        let tool_name = request.name.to_string();
        let cmd = command_for(&request.name, request.arguments);
        let proxy = self;
        async move {
            let Some(cmd) = cmd else {
                // Listed but not delegated yet — a structured tool-level
                // error (same shape the direct path uses for failures).
                return Ok(CallToolResult::error(vec![ContentBlock::text(serde_json::to_string(
                    &json!({
                        "code": "NOT_DELEGATED",
                        "exit_code": 2,
                        "message": format!(
                            "tool {tool_name} is not delegated to the daemon yet (REQ-DAEMON-008 follow-up)"
                        ),
                    }),
                )
                .expect("payload serializes"))])
                .into());
            };
            match proxy.dispatch(cmd).await {
                Ok(value) if value["status"] == "error" => {
                    // The daemon refused (role gate, quiesced, invalid
                    // args) — surface as a tool-level structured error.
                    let payload = serde_json::to_string(&json!({
                        "code": value["code"],
                        "exit_code": value["exit_code"],
                        "message": value["message"],
                    }))
                    .expect("payload serializes");
                    Ok(CallToolResult::error(vec![ContentBlock::text(payload)]).into())
                }
                Ok(value) => {
                    // Success: the daemon rendered the same JSON the direct
                    // tool produces; wrap it as a text block (the `Json`
                    // wrapper's shape) so the client sees identical content.
                    let text = serde_json::to_string(&value).expect("result serializes");
                    Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
                }
                Err(err) => {
                    // Pipe died mid-call — the daemon is gone; the caller's
                    // retry policy (REQ-DAEMON-013) handles the respawn.
                    let payload = serde_json::to_string(&json!({
                        "code": "DAEMON_UNAVAILABLE",
                        "exit_code": 19,
                        "message": format!("daemon unavailable: {err}"),
                    }))
                    .expect("payload serializes");
                    Ok(CallToolResult::error(vec![ContentBlock::text(payload)]).into())
                }
            }
        }
    }
}
