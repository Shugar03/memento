//! Best-effort JSONL operational event sink (REQ-OBS-008/009, design D5).
//!
//! Operational events append to `<root>/logs/<tid>.events.jsonl`, separate
//! from the audit log (`<tid>.jsonl`). Same contract as the audit writer
//! (audit.rs:52-117): `Mutex<File>`, best-effort — a write failure is traced
//! loudly but NEVER fails the data operation it documents. Records use the
//! audit schema shape `{ts, tenant_id, agent_id, action, target, outcome,
//! error_code, chore_id}` with ids and counts only (REQ-OBS-009, T-066 rule
//! extended to events: never chunk content, query text, or credentials).
//!
//! Enabled by `MEMENTO_EVENTS=1` (design addition resolving REQ-OBS-004 vs
//! REQ-OBS-008): off by default → zero I/O on the hot path. Retention is
//! swept with the same cutoff as audit (REQ-OBS-010, application sweep).

use memento_domain::{AgentId, ChoreId, DomainError, TenantId};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One operational event line. `action` is a stable verb (`search`,
/// `context_fit`, `tenant_open`, `model_fallback`, `cache_evict`,
/// `fts_build`, `pre_warm`, `sample`); `target` carries ids/counts only.
#[derive(Debug, Clone, Serialize)]
pub struct EventRecord {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub tenant_id: TenantId,
    /// `None` when the actor is the worker process itself (e.g. sampler
    /// events, REQ-OBS-011): never faked, serialized as null (REQ-OBS-009).
    pub agent_id: Option<AgentId>,
    pub action: String,
    pub target: serde_json::Value,
    pub outcome: &'static str,
    pub error_code: Option<&'static str>,
    pub chore_id: Option<ChoreId>,
}

/// Append-only per-tenant JSONL operational event sink (D5:
/// `logs/<tid>.events.jsonl`).
#[derive(Debug)]
pub struct EventSink {
    file: Mutex<File>,
    tenant_id: TenantId,
    log_dir: PathBuf,
}

impl EventSink {
    /// Open (creating if needed) the events file for `tenant_id` under
    /// `<root>/logs/`.
    ///
    /// # Errors
    ///
    /// * `Io` — the log directory cannot be created or the file cannot be
    ///   opened for append.
    pub fn tenant(root: impl AsRef<Path>, tenant_id: &TenantId) -> Result<Self, DomainError> {
        let log_dir = root.as_ref().join("logs");
        std::fs::create_dir_all(&log_dir).map_err(|source| DomainError::Io { source })?;
        let path = log_dir.join(format!("{tenant_id}.events.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| DomainError::Io { source })?;
        Ok(Self {
            file: Mutex::new(file),
            tenant_id: *tenant_id,
            log_dir,
        })
    }

    /// The directory holding this tenant's event lines (tests inspect it).
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// The events file path for this tenant.
    pub fn log_path(&self) -> PathBuf {
        self.log_dir
            .join(format!("{}.events.jsonl", self.tenant_id))
    }

    /// The tenant this sink is bound to (the sampler stamps it).
    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Record an operational event (best-effort: failures are traced, never
    /// propagated — see module docs).
    pub fn record(&self, event: &EventRecord) {
        match serde_json::to_string(event) {
            Ok(line) => {
                let mut file = match self.file.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if let Err(err) = writeln!(file, "{line}").and_then(|()| file.flush()) {
                    tracing::error!(%err, tenant = %self.tenant_id, action = %event.action,
                        "events log write failed");
                }
                tracing::info!(
                    tenant = %self.tenant_id,
                    agent = ?event.agent_id,
                    action = %event.action,
                    outcome = event.outcome,
                    "operational event"
                );
            }
            Err(err) => {
                tracing::error!(%err, "events record serialization failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EventRecord, EventSink};
    use memento_domain::{AgentId, ChoreId, DomainError, TenantId};
    use serde_json::json;

    fn record_for(
        tid: TenantId,
        action: &str,
        target: serde_json::Value,
        outcome: &'static str,
        error_code: Option<&'static str>,
        chore_id: Option<ChoreId>,
    ) -> EventRecord {
        EventRecord {
            ts: chrono::Utc::now(),
            tenant_id: tid,
            agent_id: Some(AgentId::new("test-agent")),
            action: action.to_string(),
            target,
            outcome,
            error_code,
            chore_id,
        }
    }

    #[test]
    fn sink_writes_jsonl_lines_with_audit_schema() {
        // REQ-OBS-008: events append to logs/<tid>.events.jsonl (separate
        // from the audit file). REQ-OBS-009: same shape as audit —
        // {ts, tenant_id, agent_id, action, target, outcome, error_code,
        // chore_id}, ids+counts only.
        let dir = tempfile::tempdir().expect("tempdir");
        let tid = TenantId::new();
        let sink = EventSink::tenant(dir.path(), &tid).expect("sink opens");
        assert_eq!(
            sink.log_path(),
            dir.path().join("logs").join(format!("{tid}.events.jsonl")),
            "events file is logs/<tid>.events.jsonl"
        );

        sink.record(&record_for(
            sink.tenant_id(),
            "search",
            json!({"hits": 2, "query_cache": "miss"}),
            "ok",
            None,
            Some(ChoreId::new()),
        ));
        sink.record(&record_for(
            sink.tenant_id(),
            "context_fit",
            json!({"chunks": 0}),
            "error",
            Some("NOT_FOUND"),
            None,
        ));

        let raw = std::fs::read_to_string(sink.log_path()).expect("events file");
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is JSON"))
            .collect();
        assert_eq!(lines.len(), 2);

        assert_eq!(lines[0]["action"], "search");
        assert_eq!(lines[0]["outcome"], "ok");
        assert_eq!(lines[0]["tenant_id"], sink.tenant_id().to_string());
        assert_eq!(lines[0]["agent_id"], "test-agent");
        assert!(lines[0]["ts"].is_string(), "timestamp present");
        assert!(lines[0]["chore_id"].is_string(), "chore_id kept when Some");
        assert_eq!(lines[0]["target"]["hits"], 2, "ids+counts only");
        assert!(lines[0]["error_code"].is_null());

        assert_eq!(lines[1]["action"], "context_fit");
        assert_eq!(lines[1]["outcome"], "error");
        assert_eq!(lines[1]["error_code"], "NOT_FOUND");
        assert!(
            lines[1]["chore_id"].is_null(),
            "chore_id omitted as null when None"
        );
    }

    #[test]
    fn second_sink_appends_to_the_same_file() {
        // REQ-OBS-008: append-only, never truncates (AuditLogger pattern).
        let dir = tempfile::tempdir().expect("tempdir");
        let tid = TenantId::new();
        let a = EventSink::tenant(dir.path(), &tid).expect("a");
        a.record(&record_for(
            tid,
            "search",
            json!({"hits": 1}),
            "ok",
            None,
            None,
        ));

        let b = EventSink::tenant(dir.path(), &tid).expect("b");
        b.record(&record_for(
            tid,
            "cache_evict",
            json!({"entries": 3}),
            "ok",
            None,
            None,
        ));

        let raw = std::fs::read_to_string(a.log_path()).expect("events file");
        assert_eq!(raw.lines().count(), 2, "appends, never truncates");
    }

    #[test]
    fn record_without_agent_serializes_null() {
        // Sampler events (REQ-OBS-011) have no agent — the worker is
        // tenant-bound, not agent-bound. REQ-OBS-009 keeps the field in the
        // schema shape; it serializes as null when absent (same rule as
        // chore_id: never fake an id).
        let dir = tempfile::tempdir().expect("tempdir");
        let tid = TenantId::new();
        let sink = EventSink::tenant(dir.path(), &tid).expect("sink opens");
        sink.record(&EventRecord {
            ts: chrono::Utc::now(),
            tenant_id: tid,
            agent_id: None,
            action: "sample".to_string(),
            target: json!({"rss_bytes": 42, "thread_count": 1}),
            outcome: "ok",
            error_code: None,
            chore_id: None,
        });

        let raw = std::fs::read_to_string(sink.log_path()).expect("events file");
        let line: serde_json::Value = serde_json::from_str(raw.trim()).expect("JSON line");
        assert_eq!(line["action"], "sample");
        assert_eq!(line["target"]["rss_bytes"], 42);
        assert!(
            line["agent_id"].is_null(),
            "absent agent stays null, never faked"
        );
    }

    #[test]
    fn unwritable_events_file_never_fails_data_op() {
        // REQ-OBS-008: best-effort by design — an unwritable events file
        // must never fail the data operation it documents. record() is
        // infallible by signature, and a fresh open on an unwritable file
        // surfaces a typed DomainError (Windows: append-open of a read-only
        // file is denied) instead of panicking.
        let dir = tempfile::tempdir().expect("tempdir");
        let tid = TenantId::new();
        let sink = EventSink::tenant(dir.path(), &tid).expect("sink opens");

        let mut perms = std::fs::metadata(sink.log_path())
            .expect("metadata")
            .permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(sink.log_path(), perms).expect("readonly");

        // The data op keeps working (no panic, no error — API is infallible).
        sink.record(&record_for(
            tid,
            "search",
            json!({"hits": 0}),
            "ok",
            None,
            None,
        ));

        // A fresh sink on the unwritable file fails with a typed error.
        match EventSink::tenant(dir.path(), &tid) {
            Err(DomainError::Io { .. }) => {}
            other => panic!("expected DomainError::Io, got {other:?}"),
        }

        // Restore so tempdir cleanup succeeds.
        let mut perms = std::fs::metadata(sink.log_path())
            .expect("metadata")
            .permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(sink.log_path(), perms).expect("writable");
    }
}
