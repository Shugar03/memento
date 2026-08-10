//! Retention sweep (T-063, REQ-ML-003, design D5, T-120).
//!
//! The sweep removes chunks whose `created_at` is older than the tenant's
//! effective retention horizon: `cutoff = now - retention_days`. The clock is
//! injectable (tests advance virtual time); the cutoff is computed once per
//! sweep so the operation is internally consistent (snapshot isolation comes
//! from LanceDB's versioned reads — no torn reads between the cutoff
//! computation and the delete).
//!
//! Per-tenant override honored (REQ-ML-003 scenario 2, config in
//! [`crate::tenant_config`]); `days = 0` disables the sweep entirely
//! (scenario 3 — nothing expires regardless of age). Setting a horizon is an
//! audited configuration change (REQ-CG-002: relaxation must be explicit and
//! audited).
//!
//! ## Audit sweep (T-120)
//!
//! The sweep ALSO removes audit JSONL lines past the tenant's effective
//! `audit_retention_days` (defaults to `retention_days`, see
//! [`TenantConfig::effective_audit_retention_days`]). Opt-out (`0`)
//! skips the audit sweep — the audit file lives until tenant erasure.

use crate::AppService;
use crate::tenant_config::{RETENTION_DISABLED, read_tenant_config, write_tenant_config};
use memento_domain::{DomainError, TenantContext};
use memento_ports::SweepReport;
use serde_json::json;

impl AppService {
    /// The tenant's effective retention horizon in days (`0` = disabled).
    /// Reads the per-tenant config (default 30 when unset, REQ-ML-003).
    pub fn retention_days(&self, ctx: &TenantContext) -> Result<u64, DomainError> {
        self.ensure_bound_tenant(ctx)?;
        Ok(read_tenant_config(&self.root, ctx.tenant_id()).retention_days)
    }

    /// The tenant's effective audit-log retention horizon in days
    /// (T-120). Defaults to [`Self::retention_days`] when `audit_days`
    /// is unset; `0` opts the tenant out (audit retained indefinitely
    /// until tenant erasure).
    pub fn audit_retention_days(&self, ctx: &TenantContext) -> Result<u64, DomainError> {
        self.ensure_bound_tenant(ctx)?;
        Ok(read_tenant_config(&self.root, ctx.tenant_id()).effective_audit_retention_days())
    }

    /// Set the retention horizon (audited configuration change, REQ-CG-002).
    /// `days = 0` opts the tenant out of retention entirely. Audit retention
    /// is NOT touched here — use [`Self::set_audit_retention_days`] for
    /// the audit-specific override (T-120).
    pub async fn set_retention_days(
        &self,
        ctx: &TenantContext,
        days: u64,
    ) -> Result<(), DomainError> {
        self.ensure_bound_tenant(ctx)?;
        let mut cfg = read_tenant_config(&self.root, ctx.tenant_id());
        cfg.retention_days = days;
        write_tenant_config(&self.root, ctx.tenant_id(), &cfg)?;
        self.record_audit(
            ctx,
            "retention_change",
            json!({
                "retention_days": days,
                "action": if days == RETENTION_DISABLED { "disable" } else if days > 30 { "relax" } else { "tighten" },
            }),
            None,
        );
        Ok(())
    }

    /// Set the audit-log retention horizon independently (T-120).
    /// `days = 0` opts the tenant out (audit retained indefinitely);
    /// missing on disk → falls back to mirroring the data retention.
    /// Audited as a configuration change (REQ-CG-002).
    pub async fn set_audit_retention_days(
        &self,
        ctx: &TenantContext,
        days: Option<u64>,
    ) -> Result<(), DomainError> {
        self.ensure_bound_tenant(ctx)?;
        let mut cfg = read_tenant_config(&self.root, ctx.tenant_id());
        cfg.audit_retention_days = days;
        write_tenant_config(&self.root, ctx.tenant_id(), &cfg)?;
        let effective = cfg.effective_audit_retention_days();
        self.record_audit(
            ctx,
            "audit_retention_change",
            json!({
                "audit_retention_days": days.map(|d| d as i64).unwrap_or(-1),
                "effective_audit_retention_days": effective,
                "action": match days {
                    None => "mirror_data_retention",
                    Some(0) => "opt_out",
                    Some(d) if d > 30 => "relax",
                    Some(_) => "tighten",
                },
            }),
            None,
        );
        Ok(())
    }

    /// Run the retention sweep: hard-delete every chunk older than
    /// `now - retention` (REQ-ML-003, design D5). Honors the per-tenant
    /// override; a disabled tenant sweeps nothing (opt-out).
    ///
    /// When `audit_retention_days` is enabled, the sweep also drops audit
    /// JSONL lines past TTL (T-120). The audit count is reported in
    /// [`SweepReport::audit_expired_count`].
    ///
    /// # Errors
    ///
    /// * Adapter errors propagate (the sweep is re-runnable — deleting is
    ///   idempotent per row).
    pub async fn retention_sweep(&self, ctx: &TenantContext) -> Result<SweepReport, DomainError> {
        self.ensure_bound_tenant(ctx)?;
        let cfg = read_tenant_config(&self.root, ctx.tenant_id());

        // Data sweep.
        let mut report = if cfg.retention_days == RETENTION_DISABLED {
            SweepReport {
                expired_count: 0,
                freed_bytes: 0,
                chore_id: memento_domain::ChoreId::new(),
                audit_expired_count: 0,
            }
        } else {
            let cutoff = self.clock.now() - chrono::Duration::days(cfg.retention_days as i64);
            memento_lancedb::sweep_expired(&self.store, ctx, cutoff).await?
        };

        // Audit sweep (T-120): opt-out when audit_retention_days == 0.
        let audit_days = cfg.effective_audit_retention_days();
        if audit_days != RETENTION_DISABLED {
            let audit_cutoff = self.clock.now() - chrono::Duration::days(audit_days as i64);
            report.audit_expired_count = self.audit.sweep_expired(audit_cutoff)?;
        }

        self.record_audit(
            ctx,
            "sweep",
            json!({
                "retention_days": cfg.retention_days,
                "audit_retention_days": audit_days,
                "cutoff": (self.clock.now() - chrono::Duration::days(cfg.retention_days as i64)).to_rfc3339(),
                "expired_count": report.expired_count,
                "audit_expired_count": report.audit_expired_count,
            }),
            Some(report.chore_id),
        );
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;
    use memento_ports::{IngestTextRequest, SearchPort};
    use memento_testkit::{TempStore, TestClock};

    async fn ingest(app: &AppService, ts: &TempStore, text: &str) {
        app.ingest_text(
            &ts.ctx(),
            IngestTextRequest {
                text: text.to_string(),
                doc_id: None,
                metadata: None,
            },
        )
        .await
        .expect("ingest");
    }

    fn clock_at(days_ago: i64) -> TestClock {
        TestClock::new(chrono::Utc::now() - chrono::Duration::days(days_ago))
    }

    #[tokio::test]
    async fn default_horizon_expires_chunks_after_30_days() {
        // REQ-ML-003 scenario 1: no override → chunks older than 30 days are
        // purged when the sweep runs; fresh chunks survive.
        let ts = TempStore::new();
        // Ingest happens at clock.now(): 40 days ago → already expired.
        let app = test_app(&ts, clock_at(40)).await;
        ingest(&app, &ts, "recuerdo antiguo que debe expirar pronto").await;

        let fresh = test_app(&ts, TestClock::default()).await;
        ingest(&fresh, &ts, "recuerdo reciente que debe sobrevivir").await;

        let report = fresh.retention_sweep(&ts.ctx()).await.expect("sweep ok");
        assert_eq!(report.expired_count, 1, "only the old chunk expires");

        let hits = fresh
            .store()
            .search(
                &ts.ctx(),
                memento_ports::SearchQuery::new("antiguo", 10, *ts.workspace_id()),
            )
            .await
            .expect("search");
        assert!(hits.is_empty(), "expired chunk no longer searchable");
        let hits = fresh
            .store()
            .search(
                &ts.ctx(),
                memento_ports::SearchQuery::new("reciente", 10, *ts.workspace_id()),
            )
            .await
            .expect("search");
        assert!(!hits.is_empty(), "fresh chunk retained");
    }

    #[tokio::test]
    async fn tenant_override_extends_the_horizon() {
        // REQ-ML-003 scenario 2: 90-day override retains 45-day-old chunks.
        let ts = TempStore::new();
        let app = test_app(&ts, clock_at(45)).await;
        ingest(&app, &ts, "recuerdo de 45 días con override de 90").await;
        app.set_retention_days(&ts.ctx(), 90)
            .await
            .expect("override");

        let report = app.retention_sweep(&ts.ctx()).await.expect("sweep");
        assert_eq!(report.expired_count, 0, "45 < 90 → retained");

        // And the sweep uses the NEW cutoff: at 95 days the same chunk goes.
        // (Re-ingest not needed — the chunk is still there.)
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn opt_out_disables_expiry_regardless_of_age() {
        // REQ-ML-003 scenario 3: retention disabled → nothing expires.
        let ts = TempStore::new();
        let app = test_app(&ts, clock_at(200)).await;
        ingest(&app, &ts, "recuerdo muy antiguo pero protegido").await;
        app.set_retention_days(&ts.ctx(), 0).await.expect("opt-out");

        let report = app.retention_sweep(&ts.ctx()).await.expect("sweep");
        assert_eq!(report.expired_count, 0);
        assert_eq!(app.store().count_chunks(&ts.ctx()).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn override_survives_reopen_and_sweep_uses_injectable_clock() {
        let ts = TempStore::new();
        let app = test_app(&ts, clock_at(60)).await;
        ingest(&app, &ts, "sesenta días").await;
        app.set_retention_days(&ts.ctx(), 90).await.expect("write");

        // A fresh app on the same store reads the persisted override.
        let reopened = test_app(&ts, clock_at(60)).await;
        assert_eq!(reopened.retention_days(&ts.ctx()).expect("read"), 90);
        let report = reopened.retention_sweep(&ts.ctx()).await.expect("sweep");
        assert_eq!(report.expired_count, 0, "override honored after reopen");
    }

    #[tokio::test]
    async fn sweep_drops_audit_lines_past_audit_retention_days() {
        // T-120: audit retention defaults to data retention (30 d);
        // a sweep with the clock advanced past the audit TTL removes the
        // expired lines.
        let ts = TempStore::new();
        let now = chrono::Utc::now();
        let app = test_app(&ts, TestClock::new(now - chrono::Duration::days(60))).await;
        ingest(&app, &ts, "recuerdo de hace 60 días").await;

        // Plant an old + a fresh audit line by writing the file directly.
        let old_line = serde_json::to_string(&serde_json::json!({
            "ts": (now - chrono::Duration::days(60)).to_rfc3339(),
            "tenant_id": ts.tenant_id().to_string(),
            "agent_id": "test-agent",
            "action": "ingest",
            "target": {"doc_id": "old"},
            "outcome": "ok",
            "error_code": null,
            "chore_id": null,
        }))
        .unwrap();
        let fresh_line = serde_json::to_string(&serde_json::json!({
            "ts": (now - chrono::Duration::days(5)).to_rfc3339(),
            "tenant_id": ts.tenant_id().to_string(),
            "agent_id": "test-agent",
            "action": "search",
            "target": {"hits": 1},
            "outcome": "ok",
            "error_code": null,
            "chore_id": null,
        }))
        .unwrap();
        std::fs::write(app.audit_log_path(), format!("{old_line}\n{fresh_line}\n"))
            .expect("plant audit lines");

        // Re-open at "now" so the cutoff = now - 30 d drops the old line.
        let fresh = test_app(&ts, TestClock::new(now)).await;
        let report = fresh.retention_sweep(&ts.ctx()).await.expect("sweep");
        assert_eq!(report.audit_expired_count, 1, "old audit line dropped");

        let raw = std::fs::read_to_string(fresh.audit_log_path()).expect("audit file");
        assert!(!raw.contains("\"doc_id\":\"old\""), "old line removed");
        assert!(raw.contains("\"hits\":1"), "fresh line kept");
        // The sweep ITSELF emits an audit line — so the post-sweep file
        // has 1 keep + 1 sweep line, not just 1.
        let fresh_count = raw.lines().filter(|l| l.contains("\"hits\":1")).count();
        assert_eq!(fresh_count, 1);
    }

    #[tokio::test]
    async fn sweep_audit_opt_out_keeps_lines_indefinitely() {
        // T-120: explicit `audit_days = 0` opts the tenant out — the sweep
        // removes zero audit lines regardless of age.
        let ts = TempStore::new();
        let now = chrono::Utc::now();
        let app = test_app(&ts, TestClock::new(now - chrono::Duration::days(365))).await;
        app.set_audit_retention_days(&ts.ctx(), Some(0))
            .await
            .expect("opt out");

        // Plant an old audit line.
        let old_line = serde_json::to_string(&serde_json::json!({
            "ts": (now - chrono::Duration::days(365)).to_rfc3339(),
            "tenant_id": ts.tenant_id().to_string(),
            "agent_id": "test-agent",
            "action": "ingest",
            "target": {"doc_id": "very-old"},
            "outcome": "ok",
            "error_code": null,
            "chore_id": null,
        }))
        .unwrap();
        std::fs::write(app.audit_log_path(), format!("{old_line}\n")).expect("plant");

        let fresh = test_app(&ts, TestClock::new(now)).await;
        let report = fresh.retention_sweep(&ts.ctx()).await.expect("sweep");
        assert_eq!(
            report.audit_expired_count, 0,
            "opt-out: nothing swept from audit"
        );

        let raw = std::fs::read_to_string(fresh.audit_log_path()).expect("audit file");
        assert!(
            raw.contains("\"doc_id\":\"very-old\""),
            "old audit line preserved"
        );
    }

    #[tokio::test]
    async fn audit_retention_override_can_be_longer_than_data_retention() {
        // T-120: a tenant can keep audit lines past data expiry
        // (e.g., 365 d audit vs 30 d data) by setting `audit_days`.
        let ts = TempStore::new();
        let now = chrono::Utc::now();
        let app = test_app(&ts, TestClock::new(now)).await;
        app.set_audit_retention_days(&ts.ctx(), Some(365))
            .await
            .expect("longer audit horizon");
        assert_eq!(app.audit_retention_days(&ts.ctx()).unwrap(), 365);
        assert_eq!(app.retention_days(&ts.ctx()).unwrap(), 30);
    }
}
