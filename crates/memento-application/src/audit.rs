//! Per-tenant audit log (REQ-CG-003; T-060 core — the full event matrix and
//! the no-secrets scan land in T-066).
//!
//! Every audit event is a JSONL line in `<root>/logs/<tenant_id>.jsonl`
//! (design D8): `{ts, tenant_id, agent_id, action, target, outcome,
//! error_code, chore_id}`. The contract:
//!
//! * **Attribution always present** — timestamp, tenant, agent, action.
//! * **Target is ids + counts only** — never chunk content, never query
//!   text, never credentials (REQ-CG-003). The no-secrets scan test (T-066)
//!   enforces this mechanically.
//! * **Best-effort by design** — an audit write failure is traced loudly but
//!   never fails the data operation it documents (the audit must not be able
//!   to deadlock compliance work). The audit log's own retention policy is
//!   decided separately (T-120).
//!
//! The logger also emits a `tracing::info!` event per line (structured
//! tracing → JSONL, per the design).

use memento_domain::{AgentId, ChoreId, DomainError, TenantContext, TenantId};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One audit line. `action` is a stable verb (`ingest`, `search`,
/// `context_fit`, `feedback`, `delete`, `sweep`, `erase`, `backup`,
/// `restore`, `export`); `target` carries ids/counts only.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub tenant_id: TenantId,
    pub agent_id: AgentId,
    pub action: String,
    pub target: serde_json::Value,
    pub outcome: &'static str,
    pub error_code: Option<&'static str>,
    pub chore_id: Option<ChoreId>,
}

/// Append-only per-tenant JSONL audit sink (D8: `logs/<tid>.jsonl`).
#[derive(Debug)]
pub struct AuditLogger {
    file: Mutex<File>,
    tenant_id: TenantId,
    log_dir: PathBuf,
}

impl AuditLogger {
    /// Open (creating if needed) the audit log for `tenant_id` under
    /// `<root>/logs/`.
    ///
    /// # Errors
    ///
    /// * `Io` — the log directory cannot be created or the file cannot be
    ///   opened for append.
    pub fn new(root: impl AsRef<Path>, tenant_id: &TenantId) -> Result<Self, DomainError> {
        let log_dir = root.as_ref().join("logs");
        std::fs::create_dir_all(&log_dir).map_err(|source| DomainError::Io { source })?;
        let path = log_dir.join(format!("{tenant_id}.jsonl"));
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

    /// The directory holding this tenant's audit lines (tests inspect it).
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// The audit file path for this tenant.
    pub fn log_path(&self) -> PathBuf {
        self.log_dir.join(format!("{}.jsonl", self.tenant_id))
    }

    /// Record an audit line (best-effort: failures are traced, never
    /// propagated — see module docs).
    pub fn record(&self, event: &AuditEvent) {
        match serde_json::to_string(event) {
            Ok(line) => {
                let mut file = match self.file.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if let Err(err) = writeln!(file, "{line}").and_then(|()| file.flush()) {
                    tracing::error!(%err, tenant = %self.tenant_id, action = %event.action,
                        "audit log write failed");
                }
                tracing::info!(
                    tenant = %self.tenant_id,
                    agent = %event.agent_id,
                    action = %event.action,
                    outcome = event.outcome,
                    "audit event"
                );
            }
            Err(err) => {
                tracing::error!(%err, "audit event serialization failed");
            }
        }
    }

    /// Build + record an "ok" event with the standard shape.
    pub fn ok(
        &self,
        ctx: &TenantContext,
        action: &str,
        target: serde_json::Value,
        chore_id: Option<ChoreId>,
    ) {
        self.record(&AuditEvent {
            ts: chrono::Utc::now(),
            tenant_id: *ctx.tenant_id(),
            agent_id: ctx.agent_id().clone(),
            action: action.to_string(),
            target,
            outcome: "ok",
            error_code: None,
            chore_id,
        })
    }

    /// Build + record an "error" event (the failed operation is audited with
    /// its stable code, never with content).
    pub fn error(
        &self,
        ctx: &TenantContext,
        action: &str,
        target: serde_json::Value,
        code: &'static str,
        chore_id: Option<ChoreId>,
    ) {
        self.record(&AuditEvent {
            ts: chrono::Utc::now(),
            tenant_id: *ctx.tenant_id(),
            agent_id: ctx.agent_id().clone(),
            action: action.to_string(),
            target,
            outcome: "error",
            error_code: Some(code),
            chore_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_domain::{AgentId, ChunkId};
    use memento_testkit::TempStore;
    use serde_json::json;

    #[test]
    fn logger_writes_jsonl_lines_with_attribution() {
        let ts = TempStore::new();
        let logger = AuditLogger::new(ts.root(), ts.tenant_id()).expect("logger opens");
        let ctx = ts.ctx();

        logger.ok(
            &ctx,
            "ingest",
            json!({"doc_id": "d1", "chunks": 3, "duplicate": false}),
            Some(ChoreId::new()),
        );
        logger.error(&ctx, "search", json!({"hits": 0}), "INVALID_INPUT", None);

        let raw = std::fs::read_to_string(logger.log_path()).expect("audit file");
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is JSON"))
            .collect();
        assert_eq!(lines.len(), 2);

        assert_eq!(lines[0]["action"], "ingest");
        assert_eq!(lines[0]["outcome"], "ok");
        assert_eq!(lines[0]["tenant_id"], ts.tenant_id().to_string());
        assert_eq!(lines[0]["agent_id"], "test-agent");
        assert!(lines[0]["ts"].is_string(), "timestamp present");
        assert!(lines[0]["chore_id"].is_string());
        assert_eq!(lines[1]["action"], "search");
        assert_eq!(lines[1]["outcome"], "error");
        assert_eq!(lines[1]["error_code"], "INVALID_INPUT");
        assert!(lines[1]["chore_id"].is_null());
    }

    #[test]
    fn second_logger_appends_to_the_same_file() {
        let ts = TempStore::new();
        let a = AuditLogger::new(ts.root(), ts.tenant_id()).expect("a");
        let ctx = ts.ctx();
        a.ok(&ctx, "delete", json!({"count": 1}), None);

        let b = AuditLogger::new(ts.root(), ts.tenant_id()).expect("b");
        b.ok(&ctx, "erase", json!({"count": 0}), None);

        let raw = std::fs::read_to_string(a.log_path()).expect("audit file");
        assert_eq!(raw.lines().count(), 2, "appends, never truncates");
    }

    #[test]
    fn agent_id_always_captured() {
        let ts = TempStore::new();
        let logger = AuditLogger::new(ts.root(), ts.tenant_id()).expect("logger");
        let ctx = memento_domain::TenantContext::new_for_tests(
            *ts.tenant_id(),
            *ts.workspace_id(),
            AgentId::new("agente-x"),
        );
        logger.ok(&ctx, "feedback", json!({"chunk_id": ChunkId::new()}), None);
        let raw = std::fs::read_to_string(logger.log_path()).expect("audit file");
        assert!(raw.contains("\"agent_id\":\"agente-x\""), "{raw}");
    }
}
