//! Daemon pipe handshake (REQ-DAEMON-005/012, design D3).
//!
//! Every pipe connection starts with one HELLO (client → daemon) and one
//! WELCOME (daemon → client), exchanged as single framed messages
//! ([`crate::frame::read_message`] / [`crate::frame::write_message`]) before
//! the rmcp JSON-RPC session starts over the framed stream.
//!
//! Auth model (D3): the HELLO carries the raw `MEMENTO_TOKEN` one-shot plus
//! the filesystem cookie nonce ([`<root>/.daemon-<pid>.cookie`], S5.1). The
//! daemon validates the token against its bound tenant and the cookie
//! against the file it wrote at startup; a mismatch closes the connection
//! with `AUTH_FAILED` and leaves an audit/auth event line (REQ-DAEMON-005).
//!
//! The token is never hashed here: `CredentialStore` verifies the raw token
//! (Argon2id cannot verify a pre-hash — D3), so the daemon compares against
//! the token it already authenticated with at startup once per connection.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The transport protocol version (bumped on wire-breaking changes; both
/// sides refuse a mismatch).
pub const PROTOCOL_VERSION: u32 = 1;

/// Client role on the pipe (REQ-DAEMON-012 role gate; D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The `memento` CLI: full superset (public 15 + `cli.*` + `sys.*`).
    Cli,
    /// The MCP stdio proxy: the public 15 tools only.
    McpProxy,
}

/// Client capabilities offered by the daemon (WELCOME).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Embedding service available (false when `--no-embeddings`).
    Embedding,
    /// Cross-encoder rerank available (behind `MEMENTO_RERANK`).
    Rerank,
    /// `sys.quiesce` / `sys.resume` supported (REQ-DAEMON-009).
    Quiesce,
}

/// The daemon's FIXED spawn config, echoed to clients (REQ-DAEMON-003):
/// a client whose own flags diverge MUST refuse with `CONFIG_MISMATCH`
/// instead of silently running with different semantics (R3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnConfig {
    pub no_embeddings: bool,
    pub locale: Option<String>,
}

/// HELLO — client → daemon, first message on every connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    pub role: Role,
    /// Client process id (audit + stall diagnostics).
    pub pid: u32,
    /// Parent process id (spawner; diagnostics only).
    pub ppid: u32,
    /// Client crate version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// The `<root>/.daemon-<pid>.cookie` nonce (REQ-DAEMON-012).
    pub cookie: String,
    /// Raw `MEMENTO_TOKEN`, one-shot (D3).
    pub token: String,
    /// Client surface locale (`es` | `en`), mirrors the WELCOME echo.
    pub locale: Option<String>,
    /// Client-side `--no-embeddings` expectation (CONFIG_MISMATCH axis).
    pub no_embeddings: bool,
    /// Client anydoc staging dir (diagnostics only).
    pub staging: PathBuf,
}

/// WELCOME — daemon → client, second message on every connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub proto: u32,
    /// The daemon process id (status reports it; REQ-DAEMON-007).
    pub daemon_pid: u32,
    /// The bound tenant id (REQ-TA-001: one daemon = one tenant).
    pub tenant_id: String,
    pub capabilities: Vec<Capability>,
    /// The daemon's fixed spawn config (CONFIG_MISMATCH axis, R3).
    pub spawn: SpawnConfig,
}

impl Welcome {
    /// Whether the daemon offers the `embedding` capability.
    pub fn has_embedding(&self) -> bool {
        self.capabilities.contains(&Capability::Embedding)
    }

    /// Whether the daemon offers the `quiesce` capability.
    pub fn has_quiesce(&self) -> bool {
        self.capabilities.contains(&Capability::Quiesce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hello() -> Hello {
        Hello {
            proto: PROTOCOL_VERSION,
            role: Role::Cli,
            pid: 4242,
            ppid: 1337,
            version: "0.1.0".into(),
            cookie: "deadbeef".into(),
            token: "memo_tid_secret".into(),
            locale: Some("es".into()),
            no_embeddings: false,
            staging: PathBuf::from(r"C:\tmp\memento"),
        }
    }

    fn sample_welcome() -> Welcome {
        Welcome {
            proto: PROTOCOL_VERSION,
            daemon_pid: 777,
            tenant_id: "tid_1".into(),
            capabilities: vec![Capability::Embedding, Capability::Quiesce],
            spawn: SpawnConfig {
                no_embeddings: false,
                locale: Some("es".into()),
            },
        }
    }

    #[test]
    fn hello_serde_roundtrip_json() {
        // The handshake rides JSON over the framed pipe; the roundtrip must
        // preserve every field (S2.2).
        let hello = sample_hello();
        let json = serde_json::to_string(&hello).expect("serialize");
        let back: Hello = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(hello, back, "JSON roundtrip is lossless");
    }

    #[test]
    fn welcome_serde_roundtrip_json() {
        let welcome = sample_welcome();
        let json = serde_json::to_string(&welcome).expect("serialize");
        let back: Welcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(welcome, back, "JSON roundtrip is lossless");
    }

    #[test]
    fn role_serde_uses_snake_case() {
        assert_eq!(serde_json::to_string(&Role::Cli).unwrap(), "\"cli\"");
        assert_eq!(
            serde_json::to_string(&Role::McpProxy).unwrap(),
            "\"mcp_proxy\""
        );
        assert_eq!(
            serde_json::from_str::<Role>("\"mcp_proxy\"").unwrap(),
            Role::McpProxy
        );
    }

    #[test]
    fn capabilities_use_snake_case() {
        assert_eq!(
            serde_json::to_string(&Capability::Quiesce).unwrap(),
            "\"quiesce\""
        );
    }

    #[test]
    fn welcome_capability_helpers() {
        let welcome = sample_welcome();
        assert!(welcome.has_embedding());
        assert!(welcome.has_quiesce());
        let bare = Welcome {
            capabilities: vec![],
            ..sample_welcome()
        };
        assert!(!bare.has_embedding());
        assert!(!bare.has_quiesce());
    }

    #[test]
    fn proto_mismatch_is_detectable() {
        // Both sides check `proto` before trusting the payload (S2.2).
        let mut hello = sample_hello();
        hello.proto = PROTOCOL_VERSION + 1;
        assert_ne!(hello.proto, PROTOCOL_VERSION);
    }
}
