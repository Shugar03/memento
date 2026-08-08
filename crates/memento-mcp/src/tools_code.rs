//! code.* tools (T-073, REQ-MS-002): the 8 read-only code-knowledge tools.
//!
//! Each tool delegates to the application's [`CodeFacade`]
//! ([`AppService::code`], T-067) — the REQ-TA-005 context guard fires
//! there BEFORE any adapter work, and the okf adapter enforces tenant
//! isolation on every query (REQ-CK-011). Indexing itself is CLI-only
//! (design: the MCP surface is read-only); unindexed projects surface the
//! structured bilingual NOT_FOUND of REQ-CK-003.
//!
//! The 8 tools land with the T-073 commit; the empty `#[tool_router]` block
//! below keeps the skeleton (T-071) compiling with a valid (empty) router.

use rmcp::tool_router;

use crate::McpServer;

/// The 8 read-only `code.*` tools, generated into `McpServer::code_tools()`.
#[tool_router(router = code_tools, vis = "pub(crate)")]
impl McpServer {}
