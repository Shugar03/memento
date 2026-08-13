//! Memento RS observability infrastructure (change: observability, design D1).
//!
//! One infra crate below `memento-application` provides the four env-gated
//! capabilities consumed by application, adapters, and entrypoints:
//!
//! * [`tracing`] — per-binary tracing subscribers on stderr (REQ-OBS-001/002):
//!   `init_cli_subscriber` / `init_mcp_subscriber` / `init_worker_subscriber`,
//!   `MEMENTO_LOG_FORMAT=pretty|json`, RUST_LOG honored.
//! * [`metrics`] — lazy Prometheus registry (REQ-OBS-006/007): recorder is
//!   installed only when `MEMENTO_METRICS=1`; `render()` emits Prometheus
//!   text with no HTTP listener ever bound.
//! * [`events`] — best-effort JSONL operational event sink (REQ-OBS-008/009),
//!   AuditLogger pattern: `logs/<tid>.events.jsonl`, ids+counts only.
//! * [`sampler`] — gated process sampler (REQ-OBS-011): RSS + thread count
//!   every 30s into the tenant events file; injectable clock and probe.
//!
//! Everything is OFF by default; spans/metric macros are no-ops without a
//! subscriber or recorder, so the hot path stays zero-cost (REQ-OBS-004).

pub mod events;
pub mod metrics;
pub mod sampler;
pub mod tracing;

pub use events::{EventRecord, EventSink};
