//! Daemon command dispatcher (REQ-DAEMON-002, design D4).
//!
//! B4 skeleton: routes every accepted pipe request to either a `sys.*`
//! control path or an MCP tool. `sys.*` just logs in this batch — B5 wires
//! the lifecycle + AppService integration (REQ-DAEMON-003/009/010/013).
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
//! The dispatcher does NOT own an `AppService` — that binding lands in B5.
//! Today every branch returns a JSON marker that proves the dispatch
//! reached the right path; the bodies (dropping the store handle, rendering
//! Prometheus text, calling into the application service, …) come in B5.
//!
//! ## Role gating
//!
//! REQ-DAEMON-012's role gate (`cli|mcp-proxy`, with `sys.*` reserved for
//! `cli`) lives one level above the dispatcher and is out of scope for B4:
//! the daemon's accept loop will consult [`Role`](crate::handshake::Role)
//! before handing anything to [`dispatch_command`].

use memento_domain::DomainError;
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
}
