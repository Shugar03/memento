//! CLI transport layer (REQ-DAEMON-002/004/006).
//!
//! Exposes the named-pipe client to the rest of the CLI. When
//! `MEMENTO_NO_DAEMON=1` (or the `--no-daemon` flag) is set, the client returns
//! `None` and the CLI falls back to its existing in-process AppService
//! startup path. Otherwise the client tries to discover and connect to a
//! running daemon; if the daemon is not alive, the lazy-spawn logic is
//! responsible for starting it (B5 lifecycle).
//!
//! The wire protocol reuses the `frame` + `handshake` shapes from
//! `memento-mcp` — the same `interprocess` crate, same codec, same HELLO,
//! same `\\.\pipe\memento-<root-hash>-<tenant>` name derivation.

pub mod pipe_client;

pub use pipe_client::{DaemonClient, DaemonError};
