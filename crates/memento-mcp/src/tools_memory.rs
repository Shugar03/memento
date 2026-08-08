//! memory.* tools (T-072, REQ-MS-002): thin delegation to the application
//! layer. NO business logic here (REQ-MS-006) — each tool validates its
//! parameters, delegates to [`AppService`] and shapes the response. Tool
//! descriptions come from the memento-i18n ES-first tables (REQ-MS-004);
//! errors are structured and bilingual (REQ-MS-005, see [`crate::errors`]).
//!
//! The 7 tools land with the T-072 commit; the empty `#[tool_router]` block
//! below keeps the skeleton (T-071) compiling with a valid (empty) router.

use rmcp::tool_router;

use crate::McpServer;

/// The 7 `memory.*` tools, generated into `McpServer::memory_tools()`.
#[tool_router(router = memory_tools, vis = "pub(crate)")]
impl McpServer {}
