//! Structured MCP errors (REQ-MS-005) with bilingual text (REQ-MS-004).
//!
//! The domain taxonomy (design D7) is the single error source: every tool
//! returns a `DomainError`; this module converts it to the MCP surface.
//!
//! * Tool level — [`ToolError`] converts into a `CallToolResult::error`
//!   (via `IntoCallToolResult`): the caller CAN read the message, and the
//!   text block carries the stable machine code, the ES primary message and
//!   the EN fallback in one structured JSON payload.
//! * Protocol level — the same conversion rides an `ErrorData` with the
//!   MCP application-error code (-32000) for paths that cannot produce a
//!   tool result (startup, identity).

use memento_domain::DomainError;
use memento_i18n::{Locale, format_error_json};
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock, ErrorCode, ErrorData};
use serde_json::{Value, json};

/// MCP application-error code (JSON-RPC -32000; the SDK exposes no named
/// constant for it).
pub const MCP_APPLICATION_ERROR: i32 = -32000;

/// The structured payload shared by both error surfaces (REQ-MS-004/005):
/// stable code + deterministic exit code + ES primary message + EN
/// fallback + localized detail.
pub fn structured_payload(err: &DomainError) -> Value {
    let es = format_error_json(err, Locale::Es);
    let en = format_error_json(err, Locale::En);
    json!({
        "code": err.code(),
        "exit_code": err.exit_code(),
        "message": es["message"],
        "message_es": es["message"],
        "message_en": en["message"],
        "detail": es["detail"],
    })
}

/// Tool-level error: wraps the domain taxonomy for the tool surface.
#[derive(Debug)]
pub struct ToolError(pub DomainError);

impl From<DomainError> for ToolError {
    fn from(err: DomainError) -> Self {
        Self(err)
    }
}

impl IntoCallToolResult for ToolError {
    fn into_call_tool_result(self) -> Result<CallToolResponse, ErrorData> {
        // Business errors surface as tool-level errors the caller CAN read
        // (rmcp guidance: use CallToolResult::error for "the tool didn't
        // work" paths). The stable code rides in the JSON text block.
        let payload = structured_payload(&self.0);
        let text = serde_json::to_string(&payload).expect("structured error serializes");
        Ok(CallToolResult::error(vec![ContentBlock::text(text)]).into())
    }
}

impl From<ToolError> for ErrorData {
    fn from(err: ToolError) -> Self {
        let payload = structured_payload(&err.0);
        let message = format!(
            "[{}] {}",
            err.0.code(),
            payload["message"].as_str().unwrap_or_default()
        );
        ErrorData::new(ErrorCode(MCP_APPLICATION_ERROR), message, Some(payload))
    }
}
