//! memento-worker — Memento RS background worker (cluster J, T-090..T-092).
//!
//! The worker is a SEPARATE PROCESS running the retention + maintenance
//! rotation on the bound tenant (REQ-TA-002/003: `MEMENTO_TOKEN` +
//! `MEMENTO_AGENT_ID`, same startup contract as the CLI and MCP surfaces).
//! The design data flow:
//!
//! ```text
//! memento-worker ──► sweep/compact/prune ──► backup (encrypt w/ per-backup key)
//! ```
//!
//! Modules:
//! * [`scheduler`] — the rotation engine: 24h timer + `--now` trigger +
//!   injectable clock (T-090, design D5).
//! * [`sweep`] — retention sweep job with lazy compact (T-091).
//! * [`maintenance`] — prune-per-rotation job (T-091).
//! * [`backup_job`] — backup job over `AppService::backup` (T-092).
//! * [`startup`] — process-bound tenant resolution + worker `AppService`.

pub mod backup_job;
pub mod maintenance;
pub mod scheduler;
pub mod sweep;
