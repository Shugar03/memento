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
//!
//! ## Retention (T-120)
//!
//! Audit retention mirrors data retention by default (30 d, REQ-ML-003):
//! see [`crate::sweep`] for the sweep that drops expired JSONL lines.
//! `0` opts the tenant out (audit retained indefinitely). The file is
//! fully removed on tenant erasure (GDPR Art. 17 — the audit is part of
//! the tenant's footprint).

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

    /// Sweep expired audit lines (T-120). Lines whose `ts` is strictly
    /// older than `cutoff` are removed; the file is rewritten atomically
    /// (temp + rename, same pattern as the credential store). Malformed
    /// lines are kept as-is to avoid dropping evidence that the audit
    /// pipeline may be relying on for incident response.
    ///
    /// Returns the number of lines removed.
    ///
    /// # Errors
    ///
    /// * `Io` — the file cannot be read or the temp file cannot be written.
    pub fn sweep_expired(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<usize, DomainError> {
        use std::io::{BufRead, BufReader, Write};

        let path = self.log_path();
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(DomainError::Io { source: err }),
        };
        let reader = BufReader::new(file);

        let mut kept: Vec<String> = Vec::new();
        let mut removed = 0usize;
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(err) => return Err(DomainError::Io { source: err }),
            };
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(v) => {
                    let ts_str = v.get("ts").and_then(|x| x.as_str()).unwrap_or("");
                    let line_ts = chrono::DateTime::parse_from_rfc3339(ts_str)
                        .map(|t| t.with_timezone(&chrono::Utc))
                        .ok();
                    match line_ts {
                        Some(ts) if ts < cutoff => {
                            removed += 1;
                        }
                        _ => kept.push(line),
                    }
                }
                Err(_) => kept.push(line), // keep malformed
            }
        }

        if removed == 0 {
            return Ok(0);
        }

        // Atomic rewrite: write to .<pid>.tmp, then rename over the live
        // file. The audit logger holds a `Mutex<File>` on the live path;
        // we write to a sibling temp and rename, so concurrent appenders
        // either see the old file (and the rename overwrites after the
        // rename — appends between read and rename are LOST on this
        // host). Audit sweeps run from the worker between runs (T-090,
        // graceful-shutdown semantic), so the race window is the same
        // operator-driven window as the existing rotation sweep.
        let tmp = path.with_extension(format!("jsonl.sweep-{}.tmp", std::process::id()));
        let mut out = std::fs::File::create(&tmp).map_err(|source| DomainError::Io { source })?;
        for line in &kept {
            writeln!(out, "{line}").map_err(|source| DomainError::Io { source })?;
        }
        out.flush().map_err(|source| DomainError::Io { source })?;
        drop(out);
        std::fs::rename(&tmp, &path).map_err(|source| DomainError::Io { source })?;
        Ok(removed)
    }

    /// Delete the audit log file entirely (used by tenant erasure, REQ-CG-001).
    /// Idempotent: returns `true` if the file existed and was removed,
    /// `false` if it was already absent.
    pub fn erase(&self) -> Result<bool, DomainError> {
        match std::fs::remove_file(self.log_path()) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(DomainError::Io { source: err }),
        }
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

    #[test]
    fn sweep_expired_removes_only_old_lines() {
        // T-120: lines older than `cutoff` are removed; lines at/after
        // `cutoff` are kept; malformed lines are kept (preserve evidence).
        let ts = TempStore::new();
        let logger = AuditLogger::new(ts.root(), ts.tenant_id()).expect("logger");
        let _ctx = ts.ctx();

        let old_ts = chrono::Utc::now() - chrono::Duration::days(60);
        let fresh_ts = chrono::Utc::now() - chrono::Duration::days(5);

        // Plant an old + a fresh + a malformed line by writing the file
        // directly (logger.record stamps `now()` which we cannot pin).
        let mut old = serde_json::to_string(&serde_json::json!({
            "ts": old_ts.to_rfc3339(),
            "tenant_id": ts.tenant_id().to_string(),
            "agent_id": "test-agent",
            "action": "ingest",
            "target": {"doc_id": "old"},
            "outcome": "ok",
            "error_code": null,
            "chore_id": null,
        }))
        .unwrap();
        let mut fresh = serde_json::to_string(&serde_json::json!({
            "ts": fresh_ts.to_rfc3339(),
            "tenant_id": ts.tenant_id().to_string(),
            "agent_id": "test-agent",
            "action": "search",
            "target": {"hits": 1},
            "outcome": "ok",
            "error_code": null,
            "chore_id": null,
        }))
        .unwrap();
        // Ensure trailing newline (file is JSONL).
        old.push('\n');
        fresh.push('\n');
        let malformed = "this is not valid json\n".to_string();

        std::fs::write(logger.log_path(), format!("{old}{fresh}{malformed}"))
            .expect("plant audit lines");

        let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
        let removed = logger.sweep_expired(cutoff).expect("sweep ok");
        assert_eq!(removed, 1, "only the old line is past TTL");

        let raw = std::fs::read_to_string(logger.log_path()).expect("audit file");
        assert!(!raw.contains("\"doc_id\":\"old\""), "old line removed");
        assert!(raw.contains("\"hits\":1"), "fresh line kept");
        assert!(raw.contains("this is not valid json"), "malformed kept");

        // A second sweep with the same cutoff removes nothing.
        let removed2 = logger.sweep_expired(cutoff).expect("sweep ok");
        assert_eq!(removed2, 0, "idempotent");
    }

    #[test]
    fn sweep_expired_on_missing_file_is_zero() {
        // T-120: no audit file yet → sweep is a no-op (returns 0).
        let ts = TempStore::new();
        let logger = AuditLogger::new(ts.root(), ts.tenant_id()).expect("logger");
        std::fs::remove_file(logger.log_path()).expect("no file");
        let removed = logger
            .sweep_expired(chrono::Utc::now())
            .expect("missing file is fine");
        assert_eq!(removed, 0);
    }

    #[test]
    fn erase_removes_the_file_and_is_idempotent() {
        // T-120: tenant erasure removes the audit log file entirely.
        let ts = TempStore::new();
        let logger = AuditLogger::new(ts.root(), ts.tenant_id()).expect("logger");
        logger.ok(
            &ts.ctx(),
            "ingest",
            json!({"doc_id": "d1", "chunks": 1, "duplicate": false}),
            None,
        );
        assert!(logger.log_path().exists());

        assert!(logger.erase().expect("erase"), "first call returns true");
        assert!(!logger.log_path().exists(), "file gone");
        assert!(
            !logger.erase().expect("erase"),
            "idempotent: false on absent"
        );
    }
}
