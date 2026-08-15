//! Daemon command dispatcher (REQ-DAEMON-002, design D4).
//!
//! B4 skeleton routes every accepted pipe request to either a `sys.*`
//! control path or an MCP tool. B5 wires the `sys.*` bodies (REQ-DAEMON-003
//! / 009 / 010 / 013, design R1 / R2 / R5 / R7): the dispatcher now owns a
//! [`DaemonState`] that holds the bound `AppService` behind a `Mutex`,
//! preserving the embedder Arc and the 100 k-entry query-embed cache cap
//! across quiesce/resume cycles (R2). mcp.* calls stay as routing markers
//! in B5 — the AppService plumbing for them lands in B7.
//!
//! ## Wire envelope
//!
//! Every accepted request deserializes into [`Command`] (one JSON
//! `{"kind":"sys| mcp", ...}` object). Every response rides
//! [`serialize_response`] through the [`crate::frame`] codec
//! (REQ-DAEMON-006: `u32` header + ≤ 2 KiB payload).
//!
//! ## Boundary
//!
//! The dispatcher does NOT own an `AppService` directly — the bound
//! application service lives inside [`DaemonState::app`] (a `Mutex<Option<_>>`)
//! and is rebuilt on `sys.resume`. The shared adapter handles
//! (parse boundary, embedder Arc, reranker Arc, clock Arc) are stored
//! outside the `Mutex` so quiesce drops only the heavy `LanceStore` +
//! `AuditLogger` handles (R2).
//!
//! ## Role gating
//!
//! REQ-DAEMON-012's role gate (`cli|mcp-proxy`, with `sys.*` reserved for
//! `cli`) lives one level above the dispatcher and is unchanged by B5:
//! the daemon's accept loop consults [`Role`](crate::handshake::Role)
//! before handing anything to [`dispatch_command_with_state`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use memento_application::{AppService, Clock, SystemClock};
use memento_domain::{DomainError, TenantContext};
use memento_ports::{EmbedPort, ParsePort, RerankPort};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::frame;

/// The four `sys.*` commands (REQ-DAEMON-009/010/013, design D4).
///
/// `Quiesce` / `Resume` coordinate with the offline `tenant restore` flow
/// (REQ-DAEMON-009); `Metrics` exposes the daemon's Prometheus text
/// (REQ-DAEMON-010); `Shutdown` is the cooperative exit
/// (REQ-DAEMON-013). B4 only logs the dispatch — B5 wires the bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SysCommand {
    /// Drain in-flight ops and release the AppService store handle
    /// (REQ-DAEMON-009). No-op in B4; B5 wires the op-gate.
    Quiesce,
    /// Reopen the AppService store after a quiesce (REQ-DAEMON-009).
    /// No-op in B4; B5 wires the reopen + ensure_schema no-op.
    Resume,
    /// Render the daemon registry as Prometheus text (REQ-DAEMON-010).
    /// No-op in B4; B5 wires the metrics registry.
    Metrics,
    /// Cooperative shutdown (REQ-DAEMON-013). No-op in B4; B5 wires the
    /// graceful exit signal.
    Shutdown,
}

/// The seven `memory.*` tools (T-072, REQ-MS-002) routed through the
/// dispatcher. Each variant matches the literal name registered in the MCP
/// router; the dispatcher's routing table is the single source of truth
/// for both the MCP stdio surface and the named-pipe carrier (REQ-MS-006
/// equivalence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryTool {
    Search,
    IngestText,
    IngestDocument,
    GetChunk,
    Feedback,
    Delete,
    ContextFit,
}

/// The eight `code.*` tools (T-073, REQ-MS-002) routed through the
/// dispatcher. Read-only by design (REQ-CK-* layers L1..L4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeTool {
    ProjectOverview,
    SymbolLookup,
    CallersOf,
    CalleesOf,
    Impact,
    Dependencies,
    Search,
    GraphDump,
}

/// The MCP half of the public surface (REQ-MS-002, REQ-DAEMON-002). `Stats`
/// and `Health` are first-class CLI commands (REQ-CL-006 / REQ-OP-001 Q3)
/// and surface here so the dispatcher has one path for every MCP-callable
/// op — `cli → pipe → dispatcher → AppService` mirrors `MCP-stdio →
/// pipe → dispatcher → AppService` byte-for-byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "namespace", rename_all = "snake_case")]
pub enum McpCommand {
    /// A `memory.*` call (T-072).
    Memory {
        /// The tool variant (Search, IngestText, …).
        tool: MemoryTool,
    },
    /// A `code.*` call (T-073).
    Code {
        /// The tool variant (ProjectOverview, …).
        tool: CodeTool,
    },
    /// The CLI/MCP `stats` command (REQ-CL-006).
    Stats,
    /// The CLI/MCP `health` command (REQ-OP-001 Q3).
    Health,
}

/// The superset command: every accepted pipe request deserializes into
/// one variant of this enum (D4). The `kind` tag separates `sys.*` from
/// `mcp.*` at the wire boundary; the `command` content carries the inner
/// payload (a `SysCommand` verb, or an `McpCommand` envelope).
///
/// Wire shape (REQ-DAEMON-006 envelope):
/// - `{"kind":"sys","command":"quiesce"}`
/// - `{"kind":"mcp","command":{"namespace":"memory","tool":"search"}}`
/// - `{"kind":"mcp","command":{"namespace":"stats"}}`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "command", rename_all = "snake_case")]
pub enum Command {
    Sys(SysCommand),
    Mcp(McpCommand),
}

impl Command {
    /// The dotted path that identifies a command on the wire and in the
    /// audit log (`sys.quiesce`, `memory.search`, `code.graph_dump`, …).
    /// Stable: tests + B5's router depend on the exact shape.
    pub fn path(&self) -> String {
        match self {
            Command::Sys(sys) => format!("sys.{}", sys_name(*sys)),
            Command::Mcp(mcp) => match mcp {
                McpCommand::Memory { tool } => format!("memory.{}", mem_name(*tool)),
                McpCommand::Code { tool } => format!("code.{}", code_name(*tool)),
                McpCommand::Stats => "stats".to_string(),
                McpCommand::Health => "health".to_string(),
            },
        }
    }
}

fn sys_name(s: SysCommand) -> &'static str {
    match s {
        SysCommand::Quiesce => "quiesce",
        SysCommand::Resume => "resume",
        SysCommand::Metrics => "metrics",
        SysCommand::Shutdown => "shutdown",
    }
}

// ---- B5: daemon-owned state + sys.* bodies --------------------------------

/// The mutable process state that every `sys.*` branch reads or mutates
/// (REQ-DAEMON-009/010/013, design R2). One `DaemonState` is owned by the
/// daemon's accept loop; every connection's request runs through
/// [`dispatch_command_with_state`] with a borrow.
///
/// `app` is `Mutex<Option<AppService>>` so the heavy handles
/// (`LanceStore` + `AuditLogger`) can be dropped on `sys.quiesce` and
/// rebuilt on `sys.resume` (R2). The adapter handles (parse, embedder,
/// reranker, clock) live OUTSIDE the mutex — they are cheap Arcs that
/// survive quiesce so the rebuild does not reload the embedder.
///
/// `shutdown` is the cooperative exit flag for `sys.shutdown` (REQ-DAEMON-013,
/// R7): the dispatcher sets it; the accept loop in `memento-daemon` polls it
/// between accepts. `started_at` is stamped at construction so `status`
/// answers can render uptime (REQ-DAEMON-007).
pub struct DaemonState {
    /// Storage root (REQ-DAEMON-003 / design D8).
    pub root: PathBuf,
    /// The bound tenant context (REQ-TA-001/002 — preserved across cycles).
    pub ctx: TenantContext,
    /// Parse boundary (cheap Arc — survives quiesce).
    pub parse: Arc<dyn ParsePort>,
    /// Embedder Arc (preserved across quiesce per R2: the embedder keeps
    /// its loaded model, so resume reuses the existing session).
    pub embedder: Option<Arc<dyn EmbedPort>>,
    /// Optional cross-encoder reranker (A1) — re-attached on resume.
    pub reranker: Option<Arc<dyn RerankPort>>,
    /// Injectable clock (REQ-ML-003, design D5).
    pub clock: Arc<dyn Clock>,
    /// The bound application service. `None` after `sys.quiesce`.
    pub app: Mutex<Option<AppService>>,
    /// Wall-clock instant the daemon became ready (REQ-DAEMON-007 `status`).
    pub started_at: DateTime<Utc>,
    /// Cooperative shutdown flag (REQ-DAEMON-013). Set by `sys.shutdown`;
    /// polled by the accept loop in `memento-daemon`.
    pub shutdown: Arc<AtomicBool>,
}

impl DaemonState {
    /// Build a fresh state with an already-open `AppService`. The caller
    /// owns the resolve-from-env + open flow (the same pattern as
    /// `McpServer::startup`); the dispatcher only owns the wire side.
    pub fn new(
        root: PathBuf,
        ctx: TenantContext,
        parse: Arc<dyn ParsePort>,
        embedder: Option<Arc<dyn EmbedPort>>,
        reranker: Option<Arc<dyn RerankPort>>,
        clock: Arc<dyn Clock>,
        initial_app: AppService,
    ) -> Self {
        Self {
            root,
            ctx,
            parse,
            embedder,
            reranker,
            clock,
            app: Mutex::new(Some(initial_app)),
            started_at: Utc::now(),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Test-only constructor that mirrors the production shape (an
    /// already-open `AppService`) but uses the real `SystemClock` and
    /// `started_at = Utc::now()` so tests never depend on wall time for
    /// correctness — only the timestamp on responses.
    #[cfg(test)]
    pub fn for_tests(
        root: PathBuf,
        ctx: TenantContext,
        parse: Arc<dyn ParsePort>,
        embedder: Option<Arc<dyn EmbedPort>>,
        reranker: Option<Arc<dyn RerankPort>>,
        initial_app: AppService,
    ) -> Self {
        Self::new(
            root,
            ctx,
            parse,
            embedder,
            reranker,
            Arc::new(SystemClock),
            initial_app,
        )
    }

    /// Whether the cooperative shutdown flag has been raised.
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Raise the cooperative shutdown flag. The accept loop in
    /// `memento-daemon` is responsible for breaking and `process::exit`-ing
    /// (the dispatcher is single-shot per request and must not race the
    /// listen loop).
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Whether the bound application service is currently open
    /// (i.e. `sys.quiesce` has not dropped it).
    pub fn app_is_open(&self) -> bool {
        self.app.lock().expect("app lock").is_some()
    }
}

/// The wire envelope returned by [`dispatch_command_with_state`]: every
/// `sys.*` branch runs real lifecycle work in B5; the `mcp.*` branch still
/// returns the B4 routing marker (the mcp body wiring lands in B7 once the
/// daemon's accept loop also routes the per-tool AppService calls).
///
/// `tracing::info!` records the dispatch path so observability spans line
/// up with the audit log; the dispatcher's signal is always observable in
/// `RUST_LOG` even when `MEMENTO_METRICS=0`.
pub async fn dispatch_command_with_state(
    state: &DaemonState,
    cmd: Command,
) -> Result<Value, DomainError> {
    let path = cmd.path();
    tracing::info!(
        dispatch = %path,
        "dispatch_command_with_state (B5 sys.* wired; mcp.* stays as routing marker)"
    );
    match cmd {
        Command::Sys(sys) => dispatch_sys_with_state(state, sys).await,
        Command::Mcp(mcp) => dispatch_mcp(mcp),
    }
}

async fn dispatch_sys_with_state(
    state: &DaemonState,
    sys: SysCommand,
) -> Result<Value, DomainError> {
    match sys {
        SysCommand::Quiesce => sys_quiesce(state),
        SysCommand::Resume => sys_resume(state).await,
        SysCommand::Metrics => Ok(sys_metrics(state)),
        SysCommand::Shutdown => Ok(sys_shutdown(state)),
    }
}

/// `sys.quiesce` (REQ-DAEMON-009, R2): drop the bound `AppService`
/// (releasing `LanceStore` + `AuditLogger` handles). The shared adapter
/// Arcs (parse, embedder, reranker, clock) stay — only the heavy
/// handles die. Idempotent: a second quiesce reports `already_quiesced`
/// without touching the (already empty) slot.
fn sys_quiesce(state: &DaemonState) -> Result<Value, DomainError> {
    let mut guard = state.app.lock().expect("app lock");
    let ts = Utc::now();
    let phase = if guard.is_some() {
        *guard = None;
        "quiesced"
    } else {
        "already_quiesced"
    };
    tracing::info!(
        phase = phase,
        tenant_id = %state.ctx.tenant_id(),
        "sys.quiesce"
    );
    Ok(json!({
        "status": "ok",
        "phase": phase,
        "ts": ts.to_rfc3339(),
    }))
}

/// `sys.resume` (REQ-DAEMON-009, R2): rebuild the bound `AppService`
/// from the preserved adapter Arcs. `LanceStore::ensure_schema` is a no-op
/// on an already-migrated store (idempotent), so the rebuild does not
/// duplicate schema work. Idempotent: a second resume against an open
/// state reports `already_open` without reopening.
///
/// The lock is released before the (async) `AppService::open` call so the
/// returned future stays `Send` (a `MutexGuard` is not `Send`, so holding
/// it across `.await` would fail the `tokio::spawn` boundary that the
/// daemon's accept loop uses).
async fn sys_resume(state: &DaemonState) -> Result<Value, DomainError> {
    let ts = Utc::now();
    // Probe-and-bail: short-circuit when the slot is already populated.
    {
        let guard = state.app.lock().expect("app lock");
        if guard.is_some() {
            tracing::info!(
                phase = "already_open",
                tenant_id = %state.ctx.tenant_id(),
                "sys.resume"
            );
            return Ok(json!({
                "status": "ok",
                "phase": "already_open",
                "ts": ts.to_rfc3339(),
            }));
        }
    }
    // Open without holding the lock — the future is `Send` again.
    let app = AppService::open(
        &state.ctx,
        &state.root,
        state.parse.clone(),
        state.embedder.clone(),
        state.clock.clone(),
    )
    .await?;
    let app = match &state.reranker {
        Some(r) => app.with_reranker(r.clone()),
        None => app,
    };
    // Re-acquire and double-check (another task may have opened it while
    // we awaited) — the rebuild is idempotent so we just keep the
    // existing one.
    let phase = {
        let mut guard = state.app.lock().expect("app lock");
        if guard.is_some() {
            "already_open".to_string()
        } else {
            *guard = Some(app);
            "resumed".to_string()
        }
    };
    tracing::info!(
        phase = phase,
        tenant_id = %state.ctx.tenant_id(),
        "sys.resume"
    );
    Ok(json!({
        "status": "ok",
        "phase": phase,
        "ts": ts.to_rfc3339(),
    }))
}

/// `sys.metrics` (REQ-DAEMON-010, R5): render the daemon's Prometheus
/// registry as text. The body is stamped with `# source=daemon pid=<n>`
/// so the dump is unambiguously daemon-sourced (R5 reconciliation —
/// archive must update REQ-OBS-007 wording). Empty body when
/// `MEMENTO_METRICS` is unset (zero work, REQ-OBS-006).
fn sys_metrics(state: &DaemonState) -> Value {
    let body = memento_observability::metrics::render();
    let pid = std::process::id();
    let stamped = format!("# source=daemon pid={pid} tenant={}\n{body}", state.ctx.tenant_id());
    tracing::info!(
        bytes = body.len(),
        "sys.metrics"
    );
    json!({
        "status": "ok",
        "format": "prometheus_text",
        "body": stamped,
        "ts": Utc::now().to_rfc3339(),
    })
}

/// `sys.shutdown` (REQ-DAEMON-013, R7): raise the cooperative shutdown
/// flag. The accept loop in `memento-daemon` polls [`DaemonState::shutdown_requested`]
/// between accepts and `process::exit(0)`s once raised; the dispatcher
/// itself does NOT race the listen loop.
fn sys_shutdown(state: &DaemonState) -> Value {
    state.request_shutdown();
    tracing::info!(
        pid = std::process::id(),
        "sys.shutdown: cooperative exit requested"
    );
    json!({
        "status": "ok",
        "phase": "shutting_down",
        "ts": Utc::now().to_rfc3339(),
    })
}

fn mem_name(m: MemoryTool) -> &'static str {
    match m {
        MemoryTool::Search => "search",
        MemoryTool::IngestText => "ingest_text",
        MemoryTool::IngestDocument => "ingest_document",
        MemoryTool::GetChunk => "get_chunk",
        MemoryTool::Feedback => "feedback",
        MemoryTool::Delete => "delete",
        MemoryTool::ContextFit => "context_fit",
    }
}

fn code_name(c: CodeTool) -> &'static str {
    match c {
        CodeTool::ProjectOverview => "project_overview",
        CodeTool::SymbolLookup => "symbol_lookup",
        CodeTool::CallersOf => "callers_of",
        CodeTool::CalleesOf => "callees_of",
        CodeTool::Impact => "impact",
        CodeTool::Dependencies => "dependencies",
        CodeTool::Search => "search",
        CodeTool::GraphDump => "graph_dump",
    }
}

/// The wire envelope returned by [`dispatch_command`]: every variant
/// resolves successfully in B4 with a marker that proves the dispatch
/// reached the right path. The bodies (sys.quiesce dropping AppService,
/// sys.metrics rendering Prometheus text, memory.search calling the
/// application service, etc.) land in B5.
///
/// `tracing::info!` records the dispatch — the audit line is intentionally
/// distinct from the application's audit logger so the dispatcher's signal
/// is always observable in `RUST_LOG` even when `MEMENTO_METRICS=0`.
pub fn dispatch_command(cmd: Command) -> Result<Value, DomainError> {
    let path = cmd.path();
    tracing::info!(
        dispatch = %path,
        "dispatch_command (B4 skeleton; B5 wires AppService)"
    );
    match cmd {
        Command::Sys(sys) => dispatch_sys(sys),
        Command::Mcp(mcp) => dispatch_mcp(mcp),
    }
}

fn dispatch_sys(sys: SysCommand) -> Result<Value, DomainError> {
    // B4: sys.* is a logging no-op — the routing is proven, the body lives
    // in B5 (lifecycle + AppService binding).
    Ok(json!({
        "status": "dispatched",
        "command": sys_name(sys),
        "phase": "b4_skeleton",
    }))
}

fn dispatch_mcp(mcp: McpCommand) -> Result<Value, DomainError> {
    // B4: every MCP branch returns a routing marker. B5 replaces each
    // arm with the matching AppService call.
    let command = match mcp {
        McpCommand::Memory { tool } => format!("memory.{}", mem_name(tool)),
        McpCommand::Code { tool } => format!("code.{}", code_name(tool)),
        McpCommand::Stats => "stats".to_string(),
        McpCommand::Health => "health".to_string(),
    };
    Ok(json!({
        "status": "dispatched",
        "command": command,
        "phase": "b4_skeleton",
    }))
}

/// Serialize a JSON value into the JSON byte stream that rides the framed
/// pipe (REQ-DAEMON-006). The frame codec takes the bytes verbatim — see
/// [`crate::frame::write_message`] for the framing step.
///
/// # Panics
///
/// Panics only if the value contains non-serializable types — by
/// construction every value the dispatcher produces is plain JSON.
pub fn serialize_response(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("dispatcher response is always serializable JSON")
}

/// Encode a JSON value into wire-ready frames (`u32` header + payload
/// chunks ≤ 2 KiB, REQ-DAEMON-006). Tests use this to prove the
/// dispatcher output fits the pipe codec without a real pipe.
pub fn encode_response_frames(value: &Value) -> Vec<Vec<u8>> {
    frame::encode(&serialize_response(value))
}

#[cfg(test)]
mod tests {
    //! Routing tests: every variant of `Command` reaches the right branch
    //! and produces a stable JSON envelope. B5 replaces each branch body
    //! with real `AppService` / lifecycle work; the wire surface and the
    //! routing table stay frozen.

    use super::*;

    // ---- path() / enum coverage ------------------------------------------------

    #[test]
    fn sys_paths_are_dotted_snake_case() {
        assert_eq!(Command::Sys(SysCommand::Quiesce).path(), "sys.quiesce");
        assert_eq!(Command::Sys(SysCommand::Resume).path(), "sys.resume");
        assert_eq!(Command::Sys(SysCommand::Metrics).path(), "sys.metrics");
        assert_eq!(Command::Sys(SysCommand::Shutdown).path(), "sys.shutdown");
    }

    #[test]
    fn mcp_memory_paths_are_dotted_snake_case() {
        let cases = [
            (MemoryTool::Search, "memory.search"),
            (MemoryTool::IngestText, "memory.ingest_text"),
            (MemoryTool::IngestDocument, "memory.ingest_document"),
            (MemoryTool::GetChunk, "memory.get_chunk"),
            (MemoryTool::Feedback, "memory.feedback"),
            (MemoryTool::Delete, "memory.delete"),
            (MemoryTool::ContextFit, "memory.context_fit"),
        ];
        for (tool, expected) in cases {
            assert_eq!(Command::Mcp(McpCommand::Memory { tool }).path(), expected);
        }
    }

    #[test]
    fn mcp_code_paths_are_dotted_snake_case() {
        let cases = [
            (CodeTool::ProjectOverview, "code.project_overview"),
            (CodeTool::SymbolLookup, "code.symbol_lookup"),
            (CodeTool::CallersOf, "code.callers_of"),
            (CodeTool::CalleesOf, "code.callees_of"),
            (CodeTool::Impact, "code.impact"),
            (CodeTool::Dependencies, "code.dependencies"),
            (CodeTool::Search, "code.search"),
            (CodeTool::GraphDump, "code.graph_dump"),
        ];
        for (tool, expected) in cases {
            assert_eq!(Command::Mcp(McpCommand::Code { tool }).path(), expected);
        }
    }

    #[test]
    fn mcp_stats_and_health_have_flat_paths() {
        // stats / health have no `mcp.` prefix on `path()` because they're
        // not namespaced — the dispatcher's `mcp.` prefix is added at the
        // wire envelope level (`Command::Mcp(McpCommand::Stats)` → "stats").
        assert_eq!(Command::Mcp(McpCommand::Stats).path(), "stats");
        assert_eq!(Command::Mcp(McpCommand::Health).path(), "health");
    }

    // ---- wire deserialization --------------------------------------------------

    #[test]
    fn sys_command_deserializes_from_snake_case() {
        assert_eq!(
            serde_json::from_str::<SysCommand>(r#""quiesce""#).unwrap(),
            SysCommand::Quiesce
        );
        assert_eq!(
            serde_json::from_str::<SysCommand>(r#""shutdown""#).unwrap(),
            SysCommand::Shutdown
        );
    }

    #[test]
    fn memory_tool_deserializes_from_snake_case() {
        assert_eq!(
            serde_json::from_str::<MemoryTool>(r#""ingest_text""#).unwrap(),
            MemoryTool::IngestText
        );
        assert_eq!(
            serde_json::from_str::<MemoryTool>(r#""context_fit""#).unwrap(),
            MemoryTool::ContextFit
        );
    }

    #[test]
    fn code_tool_deserializes_from_snake_case() {
        assert_eq!(
            serde_json::from_str::<CodeTool>(r#""graph_dump""#).unwrap(),
            CodeTool::GraphDump
        );
        assert_eq!(
            serde_json::from_str::<CodeTool>(r#""project_overview""#).unwrap(),
            CodeTool::ProjectOverview
        );
    }

    #[test]
    fn command_roundtrips_via_serde_json() {
        // Every accepted envelope deserializes and re-serializes byte-
        // identical (REQ-DAEMON-006 wire shape lock). The outer `tag +
        // content` produces `{"kind":"sys","command":"quiesce"}` for sys
        // variants and `{"kind":"mcp","command":{...}}` for mcp variants.
        let cases = [
            r#"{"kind":"sys","command":"quiesce"}"#,
            r#"{"kind":"sys","command":"resume"}"#,
            r#"{"kind":"sys","command":"metrics"}"#,
            r#"{"kind":"sys","command":"shutdown"}"#,
            r#"{"kind":"mcp","command":{"namespace":"memory","tool":"search"}}"#,
            r#"{"kind":"mcp","command":{"namespace":"memory","tool":"ingest_text"}}"#,
            r#"{"kind":"mcp","command":{"namespace":"memory","tool":"context_fit"}}"#,
            r#"{"kind":"mcp","command":{"namespace":"code","tool":"graph_dump"}}"#,
            r#"{"kind":"mcp","command":{"namespace":"code","tool":"project_overview"}}"#,
            r#"{"kind":"mcp","command":{"namespace":"stats"}}"#,
            r#"{"kind":"mcp","command":{"namespace":"health"}}"#,
        ];
        for raw in cases {
            let cmd: Command = serde_json::from_str(raw).expect("dispatch JSON parses");
            let back = serde_json::to_string(&cmd).expect("re-serializes");
            let parsed: Command = serde_json::from_str(&back).expect("roundtrip parses");
            assert_eq!(cmd, parsed, "wire roundtrip: {raw}");
        }
    }

    // ---- dispatch_sys routing --------------------------------------------------

    #[test]
    fn sys_dispatch_returns_marker_json_for_every_variant() {
        for sys in [
            SysCommand::Quiesce,
            SysCommand::Resume,
            SysCommand::Metrics,
            SysCommand::Shutdown,
        ] {
            let value = dispatch_sys(sys).expect("sys dispatch is Ok in B4");
            assert_eq!(value["status"], "dispatched", "sys dispatch: {value:?}");
            assert_eq!(value["phase"], "b4_skeleton");
            let marker = value["command"].as_str().expect("command string");
            assert!(
                !marker.is_empty() && !marker.contains('.'),
                "sys marker is the verb only: {marker}"
            );
        }
    }

    // ---- dispatch_mcp routing --------------------------------------------------

    #[test]
    fn mcp_memory_dispatch_routes_every_tool() {
        for tool in [
            MemoryTool::Search,
            MemoryTool::IngestText,
            MemoryTool::IngestDocument,
            MemoryTool::GetChunk,
            MemoryTool::Feedback,
            MemoryTool::Delete,
            MemoryTool::ContextFit,
        ] {
            let value =
                dispatch_mcp(McpCommand::Memory { tool }).expect("mcp memory dispatch is Ok in B4");
            assert_eq!(value["status"], "dispatched");
            assert_eq!(value["phase"], "b4_skeleton");
            let marker = value["command"].as_str().expect("command string");
            assert!(
                marker.starts_with("memory.") && !marker.contains(".memory."),
                "memory marker shape: {marker}"
            );
        }
    }

    #[test]
    fn mcp_code_dispatch_routes_every_tool() {
        for tool in [
            CodeTool::ProjectOverview,
            CodeTool::SymbolLookup,
            CodeTool::CallersOf,
            CodeTool::CalleesOf,
            CodeTool::Impact,
            CodeTool::Dependencies,
            CodeTool::Search,
            CodeTool::GraphDump,
        ] {
            let value =
                dispatch_mcp(McpCommand::Code { tool }).expect("mcp code dispatch is Ok in B4");
            assert_eq!(value["status"], "dispatched");
            assert_eq!(value["phase"], "b4_skeleton");
            let marker = value["command"].as_str().expect("command string");
            assert!(
                marker.starts_with("code.") && !marker.contains(".code."),
                "code marker shape: {marker}"
            );
        }
    }

    #[test]
    fn mcp_stats_and_health_dispatch_return_flat_markers() {
        let value = dispatch_mcp(McpCommand::Stats).expect("stats ok");
        assert_eq!(value["command"], "stats");
        let value = dispatch_mcp(McpCommand::Health).expect("health ok");
        assert_eq!(value["command"], "health");
    }

    // ---- dispatch_command top-level routing ------------------------------------

    #[test]
    fn dispatch_command_routes_sys_and_mcp() {
        let value = dispatch_command(Command::Sys(SysCommand::Quiesce)).expect("sys");
        assert_eq!(value["status"], "dispatched");
        assert_eq!(value["command"], "quiesce");

        let value = dispatch_command(Command::Mcp(McpCommand::Memory {
            tool: MemoryTool::Search,
        }))
        .expect("mcp memory search");
        assert_eq!(value["command"], "memory.search");

        let value = dispatch_command(Command::Mcp(McpCommand::Code {
            tool: CodeTool::GraphDump,
        }))
        .expect("mcp code graph_dump");
        assert_eq!(value["command"], "code.graph_dump");

        let value = dispatch_command(Command::Mcp(McpCommand::Stats)).expect("stats");
        assert_eq!(value["command"], "stats");

        let value = dispatch_command(Command::Mcp(McpCommand::Health)).expect("health");
        assert_eq!(value["command"], "health");
    }

    // ---- serialization helpers -------------------------------------------------

    #[test]
    fn serialize_response_emits_valid_json_byte_identical() {
        let value = json!({
            "status": "ok",
            "data": [1, 2, 3],
            "nested": { "a": true, "b": null },
        });
        let bytes = serialize_response(&value);
        let parsed: Value = serde_json::from_slice(&bytes).expect("roundtrip parses");
        assert_eq!(parsed, value, "byte-identical JSON roundtrip");
    }

    #[test]
    fn encode_response_frames_splits_past_two_kib() {
        // The dispatcher output crosses the pipe through the frame codec
        // (REQ-DAEMON-006: ≤ 2 KiB per frame, continuation bit).
        let big = "x".repeat(5000);
        let value = json!({ "data": big });
        let frames = encode_response_frames(&value);
        assert!(frames.len() > 1, "5000-byte payload splits across frames");
        for (i, frame) in frames.iter().enumerate() {
            assert!(
                frame.len() <= frame::FRAME_HEADER + frame::MAX_FRAME,
                "frame {i} exceeds 2 KiB payload cap"
            );
        }
        // Reassemble by stripping headers — byte-identical to the JSON
        // payload before framing.
        let mut acc = Vec::new();
        for f in &frames {
            acc.extend_from_slice(&f[frame::FRAME_HEADER..]);
        }
        let parsed: Value = serde_json::from_slice(&acc).expect("reassembled JSON parses");
        assert_eq!(parsed["data"].as_str().unwrap().len(), 5000);
    }

    #[test]
    fn encode_response_frames_small_payload_is_one_frame() {
        let value = json!({"status": "ok"});
        let frames = encode_response_frames(&value);
        assert_eq!(frames.len(), 1, "small payload fits one frame");
        let raw = &frames[0];
        let header = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        // Continuation bit (bit 31) MUST be clear on the only frame.
        assert_eq!(header & (1u32 << 31), 0, "no continuation bit");
        assert!(
            header as usize <= frame::MAX_FRAME,
            "len field ≤ 2 KiB: {header}"
        );
    }

    // ---- B5: sys.* bodies via dispatch_command_with_state ---------------------
    //
    // The B4 routing marker stays in `dispatch_sys` so the B4 tests above
    // remain green. B5 wires the real lifecycle through
    // `dispatch_command_with_state`, which carries a `DaemonState` borrow.
    // The tests below build a real `DaemonState` against a `TempStore` so
    // they exercise the actual `AppService::open` / drop / re-open path
    // (R2 — `LanceStore` + `AuditLogger` handles released on quiesce,
    // adapter Arcs preserved).

    mod with_state_tests {
        use super::*;
        use memento_application::{AppService, SystemClock};
        use memento_parse::ParseService;
        use memento_parse::anydoc::{AnydocCommand, AnydocConfig};
        use memento_testkit::{StubEmbedPort, TempStore};
        use std::time::Duration;

        /// Build a real `DaemonState` against a temp root. The parse boundary
        /// is the test fallback (no subprocess) and the embedder is the
        /// deterministic stub — the same shape `McpServer::startup` uses in
        /// tests, minus the reranker (left as `None` to keep the test cheap).
        /// `SystemClock` is used because `TestClock`'s `Clock` impl lives
        /// behind `#[cfg(test)]` inside `memento-application` and is not
        /// visible from `memento-mcp`'s tests (cross-crate cfg(test) does not
        /// propagate); the tests do not depend on wall time, only on the
        /// timestamp stamped on the response.
        async fn state_with_open_app() -> (TempStore, DaemonState) {
            let ts = TempStore::new();
            let parse: Arc<dyn ParsePort> = Arc::new(ParseService::new(AnydocConfig {
                command: AnydocCommand {
                    program: "never-invoked".into(),
                    args: vec![],
                    env: vec![],
                },
                timeout: Duration::from_secs(1),
                stdout_limit: 1024,
                staging_dir: std::env::temp_dir(),
            }));
            let embedder: Option<Arc<dyn EmbedPort>> =
                Some(Arc::new(StubEmbedPort::default()));
            let app = AppService::open(
                &ts.ctx(),
                ts.root(),
                parse.clone(),
                embedder.clone(),
                Arc::new(SystemClock),
            )
            .await
            .expect("test app opens");
            let state = DaemonState::for_tests(
                ts.root().to_path_buf(),
                ts.ctx(),
                parse,
                embedder,
                None,
                app,
            );
            (ts, state)
        }

        #[tokio::test]
        async fn quiesce_drops_app_service_and_responds_with_timestamp() {
            // R2 / REQ-DAEMON-009: sys.quiesce drops the AppService (releasing
            // LanceStore + AuditLogger handles) and returns an OK envelope
            // stamped with the wall-clock timestamp.
            let (_ts, state) = state_with_open_app().await;
            assert!(state.app_is_open(), "baseline: app is open");

            let value = dispatch_command_with_state(
                &state,
                Command::Sys(SysCommand::Quiesce),
            )
            .await
            .expect("quiesce ok");
            assert_eq!(value["status"], "ok");
            assert_eq!(value["phase"], "quiesced");
            assert!(
                value["ts"].as_str().is_some_and(str::is_empty) == false
                    && value["ts"].is_string(),
                "ts stamped: {value}"
            );
            assert!(
                !state.app_is_open(),
                "AppService dropped from state.app"
            );
        }

        #[tokio::test]
        async fn quiesce_is_idempotent_when_no_app_service() {
            // R2: a second quiesce against an already-quiesced state reports
            // `already_quiesced` without panicking (the Mutex<Option<_>> guard
            // is empty on entry — no swap happens).
            let (_ts, state) = state_with_open_app().await;
            let _ = dispatch_command_with_state(
                &state,
                Command::Sys(SysCommand::Quiesce),
            )
            .await
            .expect("first quiesce");

            let value = dispatch_command_with_state(
                &state,
                Command::Sys(SysCommand::Quiesce),
            )
            .await
            .expect("second quiesce");
            assert_eq!(value["status"], "ok");
            assert_eq!(value["phase"], "already_quiesced");
        }

        #[tokio::test]
        async fn resume_rebuilds_app_service_when_empty() {
            // R2 / REQ-DAEMON-009: sys.resume reopens AppService using the
            // preserved adapter Arcs (parse, embedder, clock). The embedder
            // Arc stays the same — that's the R2 contract.
            let (ts, state) = state_with_open_app().await;
            let _ = dispatch_command_with_state(
                &state,
                Command::Sys(SysCommand::Quiesce),
            )
            .await
            .expect("quiesce");

            let value = dispatch_command_with_state(
                &state,
                Command::Sys(SysCommand::Resume),
            )
            .await
            .expect("resume");
            assert_eq!(value["status"], "ok");
            assert_eq!(value["phase"], "resumed");
            assert!(state.app_is_open(), "AppService rebuilt");
            // Audit log survives quiesce → resume: the log file persists
            // across the cycle (append-only, REQ-OBS-008 contract).
            let audit_path = ts
                .root()
                .join("logs")
                .join(format!("{}.jsonl", ts.tenant_id()));
            assert!(
                audit_path.exists(),
                "audit log file persists across quiesce/resume"
            );
        }

        #[tokio::test]
        async fn resume_is_idempotent_when_app_service_already_open() {
            // R2: a second resume against an already-open state reports
            // `already_open` without re-opening (avoids duplicate schema
            // work and double pre-warm).
            let (_ts, state) = state_with_open_app().await;
            let value = dispatch_command_with_state(
                &state,
                Command::Sys(SysCommand::Resume),
            )
            .await
            .expect("resume against open state");
            assert_eq!(value["status"], "ok");
            assert_eq!(value["phase"], "already_open");
            assert!(state.app_is_open());
        }

        #[tokio::test]
        async fn metrics_renders_prometheus_text_with_daemon_stamp() {
            // REQ-DAEMON-010 / R5: sys.metrics returns a JSON envelope whose
            // body is the Prometheus text from the daemon registry, stamped
            // with `# source=daemon pid=<n> tenant=<tid>`. With
            // MEMENTO_METRICS unset the registry is empty (REQ-OBS-006) and
            // the stamp is the only line — operators can still distinguish
            // daemon-sourced from CLI-sourced dumps.
            //
            // The env-var mutation is scoped to a synchronous prelude so the
            // `MutexGuard` never crosses the `.await` boundary (clippy's
            // `await_holding_lock` lint).
            static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            {
                let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                // SAFETY: serialized by ENV_LOCK.
                unsafe { std::env::remove_var("MEMENTO_METRICS") };
            }

            let (_ts, state) = state_with_open_app().await;
            let value = dispatch_command_with_state(
                &state,
                Command::Sys(SysCommand::Metrics),
            )
            .await
            .expect("metrics");
            assert_eq!(value["status"], "ok");
            assert_eq!(value["format"], "prometheus_text");
            let body = value["body"].as_str().expect("body string");
            let pid_line = format!("# source=daemon pid={}", std::process::id());
            assert!(
                body.starts_with(&pid_line),
                "stamp: {body}"
            );
            assert!(
                body.contains(&format!("tenant={}", state.ctx.tenant_id())),
                "tenant stamp: {body}"
            );
        }

        #[tokio::test]
        async fn shutdown_raises_flag_and_returns_ok() {
            // REQ-DAEMON-013 / R7: sys.shutdown sets the cooperative exit
            // flag (the accept loop in memento-daemon is responsible for
            // breaking + process::exit). The dispatcher never races the
            // listen loop.
            let (_ts, state) = state_with_open_app().await;
            assert!(!state.shutdown_requested(), "baseline: flag clear");

            let value = dispatch_command_with_state(
                &state,
                Command::Sys(SysCommand::Shutdown),
            )
            .await
            .expect("shutdown");
            assert_eq!(value["status"], "ok");
            assert_eq!(value["phase"], "shutting_down");
            assert!(state.shutdown_requested(), "flag raised");
        }

        #[tokio::test]
        async fn mcp_branches_still_route_as_b4_marker() {
            // B5 only wires sys.*; mcp.* stays as a routing marker (the
            // mcp body wiring lands in B7). The shape stays frozen so the
            // dispatcher's JSON envelope remains stable across batches.
            let (_ts, state) = state_with_open_app().await;
            let value = dispatch_command_with_state(
                &state,
                Command::Mcp(McpCommand::Memory {
                    tool: MemoryTool::Search,
                }),
            )
            .await
            .expect("mcp search routes");
            assert_eq!(value["status"], "dispatched");
            assert_eq!(value["phase"], "b4_skeleton");
            assert_eq!(value["command"], "memory.search");
        }

        #[tokio::test]
        async fn sys_command_roundtrips_through_dispatch_with_state() {
            // Top-level routing test: every sys.* variant reaches the
            // matching body under dispatch_command_with_state. The sys
            // branches return real envelopes (ok); the mcp branches stay
            // as the routing marker.
            let (_ts, state) = state_with_open_app().await;
            for sys in [
                SysCommand::Quiesce,
                SysCommand::Resume,
                SysCommand::Metrics,
                SysCommand::Shutdown,
            ] {
                let value = dispatch_command_with_state(
                    &state,
                    Command::Sys(sys),
                )
                .await
                .expect("sys dispatch ok");
                assert_eq!(
                    value["status"], "ok",
                    "sys.{:?} status: {value}", sys
                );
            }
            // The shutdown flag survived the four sys calls.
            assert!(state.shutdown_requested(), "shutdown sticky");
        }
    }
}
